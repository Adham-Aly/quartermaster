//! The fallback core: what the UI talks to when the real core couldn't start.
//!
//! The binary answers the UI itself: `Refresh` gets the empty Bridge (a default `Snapshot`
//! carrying this Mac's name, and why the core didn't start: an app line in the conversation and
//! `SettingsView::unavailable`, so Settings says so rather than look empty), and `RequestQuit`
//! and `Quit` get `Closing`, after which the loop ends. It also ends when the UI drops its end of
//! the link.

use qm_types::api::{ChatLine, Command, CoreServer, Snapshot, Update};
use qm_types::domain::{MessageKind, MessageSource};
use qm_types::ids::MessageId;

/// The snapshot the fallback core serves: empty, with `problem` shown.
pub fn snapshot(machine: &str, problem: &str) -> Snapshot {
    let mut snapshot = Snapshot::default();
    machine.clone_into(&mut snapshot.machine);
    snapshot.settings.unavailable = Some(problem.to_owned());
    snapshot.conversation.push(ChatLine::new(
        MessageId::new(1),
        MessageSource::App,
        MessageKind::AppNote,
        jiff::Timestamp::now(),
        problem,
    ));
    snapshot
}

/// Serves the UI until it quits or goes away, showing `problem`.
pub async fn serve(mut server: CoreServer, machine: String, problem: String) {
    while let Some(command) = server.recv().await {
        let update = match command {
            Command::Refresh => Update::Snapshot(Box::new(snapshot(&machine, &problem))),
            Command::RequestQuit | Command::Quit => {
                let _ = server.send(Update::Closing);
                tracing::info!("the UI asked to quit");
                return;
            }
            other => {
                tracing::debug!(?other, "command not handled by the fallback core");
                continue;
            }
        };
        if server.send(update).is_err() {
            return;
        }
    }
}

/// This Mac's short name, as shown in the top bar.
pub fn machine_name() -> String {
    nix::unistd::gethostname()
        .ok()
        .and_then(|h| h.into_string().ok())
        .map(|h| h.split('.').next().unwrap_or_default().to_owned())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "this Mac".to_owned())
}
