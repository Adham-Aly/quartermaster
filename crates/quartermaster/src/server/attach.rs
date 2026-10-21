//! The attach client: puts this terminal on the server's window.
//!
//! `qm_tui::attach` does the terminal work (full-screen mode, forwarding keys and the mouse,
//! printing what the server draws). This keeps the window attached across a server restart or
//! crash. Closing the window, or the server telling it to leave, detaches it; the server and its
//! agents keep running.

use super::spawn;
use qm_types::data_dir::DataDir;
use qm_types::wire::ExitReason;
use std::time::{Duration, Instant};

/// Attaches until this window is told to leave. Returns what to print after the terminal is back.
pub async fn run(dir: &DataDir) -> Result<String, String> {
    let mut last_lost: Option<Instant> = None;
    loop {
        let stream = spawn::connect_or_start(dir).await?;
        match qm_tui::attach(stream).await.map_err(|e| e.to_string())? {
            Some(ExitReason::Detached) => {
                return Ok(
                    "Detached. Quartermaster keeps running; run `quartermaster` to come back."
                        .to_owned(),
                );
            }
            Some(ExitReason::Quit | ExitReason::ServerStopped) => {
                return Ok("Quartermaster stopped.".to_owned());
            }
            Some(ExitReason::Restarting) => {
                println!("Restarting…");
                wait_for_old_server(dir).await;
            }
            None => {
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

/// Waits (briefly) for the server that said it was restarting to stop answering, so the next
/// connection reaches the new one.
async fn wait_for_old_server(dir: &DataDir) {
    for _ in 0..200 {
        if !matches!(spawn::connect(dir).await, Ok(Some(_))) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
