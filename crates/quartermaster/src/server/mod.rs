//! The background server and the commands that reach it.
//!
//! ```text
//! quartermaster                       attach (starting the server first if none is running)
//! quartermaster --foreground          server and client in this one process, ending with the UI
//! quartermaster server run            the server itself (the spawner runs this; hidden)
//! quartermaster server status         pid, version, uptime, windows, working agents
//! quartermaster server stop [--force] stop it; asks first when agents are working
//! quartermaster server restart [--force]
//! ```
//!
//! One server per data directory holds `run.lock` for as long as it lives; the socket
//! (`server.sock`, 0600, same-user peers only) is only the way in. Windows come and go without
//! touching the core: the server keeps the UI's end of the core link for its whole life, so the
//! crew keeps working with no window attached.
//!
//! - [`serve`]: the socket, the peer check, the handshake and control requests.
//! - [`spawn`]: finding the server and starting it (launchd one-shot, or a direct spawn).
//! - [`attach`]: the client in the terminal: re-attaches after a restart, respawns a crashed
//!   server once (a second loss within 30 s is an error), and handles version skew: the same
//!   protocol attaches with a "restart the server" notice, another protocol is refused with an
//!   offer to restart that says first whether agents are working.

mod attach;
mod serve;
mod spawn;

use crate::{Failure, fail, home, lock};
use qm_types::data_dir::DataDir;
use qm_types::wire::{self, ExitReason, Frame, Request, Response, ServerStatus};
use std::io::{IsTerminal, Write};
use std::time::Duration;

/// What the command line asked for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Attach,
    Foreground,
    ServerRun,
    ServerStatus,
    ServerStop { force: bool },
    ServerRestart { force: bool },
}

const USAGE: &str = "usage: quartermaster [--foreground]\n       \
                     quartermaster server status | stop [--force] | restart [--force]";

/// Reads the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let force = |rest: &[&str]| match rest {
        [] => Ok(false),
        ["--force"] => Ok(true),
        _ => Err(USAGE.to_owned()),
    };
    match args.as_slice() {
        [] => Ok(Command::Attach),
        // Developer commands are handled (or ignored) before this point; they always started the app.
        ["--foreground"] | ["dev", ..] => Ok(Command::Foreground),
        ["server", "run"] => Ok(Command::ServerRun),
        ["server", "status"] => Ok(Command::ServerStatus),
        ["server", "stop", rest @ ..] => Ok(Command::ServerStop {
            force: force(rest)?,
        }),
        ["server", "restart", rest @ ..] => Ok(Command::ServerRestart {
            force: force(rest)?,
        }),
        _ => Err(USAGE.to_owned()),
    }
}

fn client_runtime() -> Result<tokio::runtime::Runtime, Failure> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| fail("can't start the runtime")(&e))
}

fn prepared_dir() -> Result<DataDir, Failure> {
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o077));
    let dir = DataDir::resolve().map_err(|e| fail("can't find the data directory")(&e))?;
    home::prepare(&dir).map_err(|e| {
        fail(&format!(
            "can't create the data directory {}",
            dir.root().display()
        ))(&e)
    })?;
    Ok(dir)
}

/// `quartermaster`: attach to the server.
pub fn attach() -> Result<(), Failure> {
    let dir = prepared_dir()?;
    let message = client_runtime()?
        .block_on(attach::run(&dir))
        .map_err(|message| Failure { message })?;
    println!("{message}");
    Ok(())
}

/// `quartermaster server status|stop|restart`.
pub fn control(command: &Command) -> Result<(), Failure> {
    let dir = prepared_dir()?;
    let rt = client_runtime()?;
    rt.block_on(async {
        match command {
            Command::ServerStatus => status(&dir).await,
            Command::ServerStop { force } => stop(&dir, *force, false).await.map(drop),
            Command::ServerRestart { force } => {
                if stop(&dir, *force, true).await? {
                    let stream = spawn::connect_or_start(&dir).await?;
                    drop(stream);
                    println!("Started.");
                }
                Ok(())
            }
            _ => Ok(()),
        }
    })
    .map_err(|message| Failure { message })
}

/// One request and its answer; `None` when no server is running.
async fn ask(dir: &DataDir, request: &Request) -> Result<Option<Response>, String> {
    let Some(stream) = spawn::connect(dir).await? else {
        return Ok(None);
    };
    let (mut rd, mut wr) = stream.into_split();
    let io = |e: wire::WireError| e.to_string();
    wire::write_frame(&mut wr, &Frame::Request(request.clone()))
        .await
        .map_err(io)?;
    match wire::read_frame(&mut rd).await.map_err(io)? {
        Some(Frame::Response(response)) => Ok(Some(response)),
        Some(Frame::Refuse(refuse)) => Err(refuse.reason),
        _ => Err("the server closed the connection without answering".to_owned()),
    }
}

async fn status(dir: &DataDir) -> Result<(), String> {
    match ask(dir, &Request::Status).await? {
        Some(Response::Status(s)) => {
            print_status(&s);
            Ok(())
        }
        None => Err("The Quartermaster server isn't running.".to_owned()),
        Some(_) => Err("the server answered something else".to_owned()),
    }
}

fn print_status(s: &ServerStatus) {
    let secs = s.uptime_secs;
    println!(
        "Quartermaster server {} (pid {}, protocol {})\nup {}h {:02}m {:02}s · {} window{} attached · {} crew member{}",
        s.version,
        s.pid,
        s.protocol,
        secs / 3600,
        secs / 60 % 60,
        secs % 60,
        s.windows,
        if s.windows == 1 { "" } else { "s" },
        s.working,
        if s.working == 1 { "" } else { "s" },
    );
}

/// Stops the server, asking first when agents are working. `true` when it is stopped (or wasn't
/// running).
async fn stop(dir: &DataDir, force: bool, restart: bool) -> Result<bool, String> {
    let mut force = force;
    loop {
        let request = if restart {
            Request::Restart { force }
        } else {
            Request::Stop { force }
        };
        match ask(dir, &request).await? {
            None => {
                println!("The Quartermaster server isn't running.");
                return Ok(true);
            }
            Some(Response::Stopping) => {
                wait_gone(dir).await?;
                println!("Stopped.");
                return Ok(true);
            }
            Some(Response::Working { working }) => {
                if !std::io::stdin().is_terminal() {
                    return Err(format!(
                        "{working} agent{} working; stopping the server pauses them until it \
                         starts again. Run it with --force to stop anyway.",
                        if working == 1 { " is" } else { "s are" }
                    ));
                }
                print!(
                    "{working} agent{} working; stopping the server pauses them until it starts \
                     again. Stop anyway? [y/N] ",
                    if working == 1 { " is" } else { "s are" }
                );
                let _ = std::io::stdout().flush();
                let mut line = String::new();
                let _ = std::io::stdin().read_line(&mut line);
                if !matches!(line.trim(), "y" | "Y" | "yes") {
                    println!("Left running.");
                    return Ok(false);
                }
                force = true;
            }
            Some(Response::Status(_)) => {
                return Err("the server answered something else".to_owned());
            }
        }
    }
}

/// Waits for the server to let go of `run.lock`.
async fn wait_gone(dir: &DataDir) -> Result<(), String> {
    for _ in 0..600 {
        match lock::RunLock::acquire(&dir.run_lock()) {
            Ok(_free) => return Ok(()),
            Err(lock::LockError::AlreadyRunning) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(format!("can't check the run lock: {e}")),
        }
    }
    Err("the server is still stopping after 30 seconds".to_owned())
}

/// `quartermaster server run`: the server, in this process.
pub fn run_server() -> Result<(), Failure> {
    // Leave the terminal's session first. This fails (harmlessly) when we already lead one.
    let _ = nix::unistd::setsid();
    let label = std::env::var(spawn::LAUNCHD_LABEL).ok();
    let result = serve_forever();
    if let Some(label) = label {
        // The job has done its work; removing it ends this process, so it comes last.
        let _ = spawn::remove_job(&label);
    }
    result
}

fn serve_forever() -> Result<(), Failure> {
    let dir = prepared_dir()?;
    // The loser of two simultaneous launches leaves quietly; the winner serves both.
    let _lock = match lock::RunLock::acquire(&dir.run_lock()) {
        Ok(lock) => lock,
        Err(lock::LockError::AlreadyRunning) => return Ok(()),
        Err(e) => return Err(fail("can't take the run lock")(&e)),
    };
    let booted = crate::boot(&dir)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("qm-core")
        .build()
        .map_err(|e| fail("can't start the runtime")(&e))?;
    let result = runtime.block_on(serve(dir.clone(), booted));
    runtime.shutdown_timeout(Duration::from_secs(2));
    let _ = std::fs::remove_file(dir.server_socket());
    tracing::info!("server stopped");
    result.map_err(|message| Failure { message })
}

async fn serve(dir: DataDir, booted: crate::Booted) -> Result<(), String> {
    use tokio::signal::unix::{SignalKind, signal};
    // Ignoring SIGHUP is installing a handler that does nothing.
    let mut hup = signal(SignalKind::hangup()).map_err(|e| e.to_string())?;
    let mut term = signal(SignalKind::terminate()).map_err(|e| e.to_string())?;
    let mut int = signal(SignalKind::interrupt()).map_err(|e| e.to_string())?;

    let (client, core_end) = qm_types::api::link();
    let core = tokio::spawn(crate::wiring::start_core(
        dir.clone(),
        booted.db,
        booted.config,
        core_end,
        crate::core_link::machine_name(),
        booted.dev_script,
    ));
    let (handle, commands) = qm_tui::host::control();
    let (windows_tx, windows) = serve::windows();
    // The UI host holds the UI's end of the core link for the server's whole life, so windows
    // coming and going never touch the crew. It ends when the core closes.
    let mut host = tokio::spawn(qm_tui::host::serve(client, windows, commands));

    let listener = serve::bind(&dir.server_socket())?;
    let started_unix = jiff::Timestamp::now().as_second();
    let shared = serve::Shared::new(
        handle.clone(),
        windows_tx,
        started_unix,
        nix::unistd::getuid().as_raw(),
    );
    tracing::info!(socket = %dir.server_socket().display(), "server listening");

    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    connections.spawn(serve::connection(stream, shared.clone()));
                }
                Err(e) => tracing::warn!("accept failed: {e}"),
            },
            _ = hup.recv() => tracing::info!("ignoring SIGHUP"),
            _ = term.recv() => {
                tracing::info!("SIGTERM: stopping");
                handle.quit(true, ExitReason::ServerStopped).await;
            }
            _ = int.recv() => {
                tracing::info!("SIGINT: stopping");
                handle.quit(true, ExitReason::ServerStopped).await;
            }
            // The core closed (a stop, or the core ended): the windows were told to leave.
            done = &mut host => {
                if let Ok(Err(e)) = done {
                    tracing::warn!("the UI host failed: {e}");
                }
                break;
            }
        }
    }
    drop(listener);
    let _ = std::fs::remove_file(dir.server_socket());
    connections.abort_all();
    let _ = tokio::time::timeout(Duration::from_secs(5), core).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Command, String> {
        parse(&args.iter().map(ToString::to_string).collect::<Vec<_>>())
    }

    #[test]
    fn commands_parse() {
        assert_eq!(p(&[]), Ok(Command::Attach));
        assert_eq!(p(&["--foreground"]), Ok(Command::Foreground));
        assert_eq!(p(&["server", "run"]), Ok(Command::ServerRun));
        assert_eq!(
            p(&["server", "stop"]),
            Ok(Command::ServerStop { force: false })
        );
        assert_eq!(
            p(&["server", "restart", "--force"]),
            Ok(Command::ServerRestart { force: true })
        );
        assert!(p(&["server", "stop", "--nope"]).is_err());
        assert!(p(&["bogus"]).is_err());
    }
}
