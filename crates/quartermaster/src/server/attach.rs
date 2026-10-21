//! The attach client: puts this terminal on the server's window.
//!
//! `qm_tui::attach` does the terminal work (full-screen mode, forwarding keys and the mouse,
//! printing what the server draws). This keeps the window attached across a server restart or
//! crash. Closing the window, or the server telling it to leave, detaches it; the server and its
//! agents keep running.

use super::spawn;
use qm_core::updates::Version;
use qm_tui::{Left, Notice};
use qm_types::data_dir::DataDir;
use qm_types::wire::{ExitReason, PROTOCOL, Request, Response, ServerStatus};
use std::io::{IsTerminal, Write};
use std::time::{Duration, Instant};

/// Attaches until this window is told to leave. Returns what to print after the terminal is back.
pub async fn run(dir: &DataDir) -> Result<String, String> {
    let mut last_lost: Option<Instant> = None;
    loop {
        let stream = spawn::connect_or_start(dir).await?;
        let notice = match compare(dir).await {
            Skew::Same => None,
            Skew::ClientNewer(notice) => Some(notice),
            Skew::Protocol(running) => {
                drop(stream);
                if offer_restart(dir, &running).await? {
                    continue;
                }
                return Ok("Left the server running; this terminal can't attach to it.".to_owned());
            }
        };
        match qm_tui::attach(stream, notice)
            .await
            .map_err(|e| e.to_string())?
        {
            Left::Told(ExitReason::Detached) => {
                return Ok(
                    "Detached. Quartermaster keeps running; run `quartermaster` to come back."
                        .to_owned(),
                );
            }
            Left::Told(ExitReason::Quit | ExitReason::ServerStopped) => {
                return Ok("Quartermaster stopped.".to_owned());
            }
            Left::Told(ExitReason::Restarting) => {
                println!("Restarting…");
                wait_for_old_server(dir).await;
            }
            // The notice's button: restart the server so it runs this newer copy. It asks first
            // when agents are working.
            Left::Pressed => {
                if !super::stop(dir, false, true).await? {
                    return Ok("Left the server running.".to_owned());
                }
            }
            Left::Lost => {
                // Start it once more; a second loss within 30 s shows the error instead.
                if last_lost.is_some_and(|t| t.elapsed() < Duration::from_secs(30)) {
                    return Err(format!(
                        "The Quartermaster server stopped unexpectedly twice; the log in {} says why.",
                        dir.logs().display()
                    ));
                }
                last_lost = Some(Instant::now());
            }
        }
    }
}

/// How this copy and the running server differ, if they do.
enum Skew {
    Same,
    /// The same protocol, but this copy is newer: it attaches, with a notice to restart.
    ClientNewer(Notice),
    /// A different protocol: it can't attach. Carries what the server said about itself.
    Protocol(ServerStatus),
}

async fn compare(dir: &DataDir) -> Skew {
    let Ok(Some(Response::Status(running))) = super::ask(dir, &Request::Status).await else {
        // Whatever it is, attaching will say so in its own words.
        return Skew::Same;
    };
    classify(running, env!("CARGO_PKG_VERSION"))
}

fn classify(running: ServerStatus, own: &str) -> Skew {
    if running.protocol != PROTOCOL {
        return Skew::Protocol(running);
    }
    match (Version::parse(own), Version::parse(&running.version)) {
        (Some(own), Some(theirs)) if own > theirs => Skew::ClientNewer(Notice {
            text: format!("Quartermaster {own} installed — restart the server to use it"),
            button: "Restart".to_owned(),
        }),
        _ => Skew::Same,
    }
}

/// The running server speaks another protocol: says whether agents are working, and offers to
/// restart it. `true` when it was restarted.
async fn offer_restart(dir: &DataDir, running: &ServerStatus) -> Result<bool, String> {
    let working = match running.working {
        0 => "No agents are working.".to_owned(),
        1 => "1 agent is working; restarting pauses it until the server starts again.".to_owned(),
        n => {
            format!("{n} agents are working; restarting pauses them until the server starts again.")
        }
    };
    let message = format!(
        "The running Quartermaster server ({}) speaks protocol {} and this terminal ({}) speaks \
         {PROTOCOL}. {working}",
        running.version,
        running.protocol,
        env!("CARGO_PKG_VERSION"),
    );
    if !std::io::stdin().is_terminal() {
        return Err(format!(
            "{message} Run `quartermaster server restart` to use this version."
        ));
    }
    print!("{message}\nRestart the server now? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    let _ = std::io::stdin().read_line(&mut line);
    if !matches!(line.trim(), "y" | "Y" | "yes") {
        return Ok(false);
    }
    // They've seen the count: no second question.
    super::stop(dir, true, true).await
}

/// Waits for the server that said it was restarting to let go of `run.lock`. Its socket goes
/// first, but a server started before the lock is free would find it taken and leave.
async fn wait_for_old_server(dir: &DataDir) {
    let _ = super::wait_gone(dir).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn running(version: &str, protocol: u32) -> ServerStatus {
        ServerStatus {
            version: version.to_owned(),
            protocol,
            ..ServerStatus::default()
        }
    }

    #[test]
    fn a_newer_client_on_the_same_protocol_attaches_with_a_notice() {
        let Skew::ClientNewer(notice) = classify(running("0.1.0", PROTOCOL), "0.2.0") else {
            panic!("expected a notice");
        };
        assert_eq!(
            notice.text,
            "Quartermaster 0.2.0 installed — restart the server to use it"
        );
        assert_eq!(notice.button, "Restart");
    }

    #[test]
    fn the_same_or_an_older_client_has_no_notice() {
        assert!(matches!(
            classify(running("0.2.0", PROTOCOL), "0.2.0"),
            Skew::Same
        ));
        assert!(matches!(
            classify(running("0.3.0", PROTOCOL), "0.2.0"),
            Skew::Same
        ));
    }

    #[test]
    fn another_protocol_wins_over_the_version() {
        assert!(matches!(
            classify(running("0.1.0", PROTOCOL + 1), "0.2.0"),
            Skew::Protocol(_)
        ));
    }
}
