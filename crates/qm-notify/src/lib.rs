//! macOS notifications for requests waiting on you: a banner with the Quartermaster mark for each
//! new one, and the mark in the menu bar with a count and a menu of what's waiting.
//!
//! The server runs [`run`] beside the UI host, which publishes a [`Waiting`] whenever the Actions
//! inbox, the windows' focus or the setting changes. While notifications are on, a small helper
//! app (`helper/main.swift`, built by `build.rs` and embedded) runs from `Quartermaster.app` in
//! the data directory: macOS shows banners only for a signed app that `LaunchServices` launched,
//! so the server starts it with `open` and it connects back on `notifier.sock`. They speak JSON
//! lines (`tracker` has the server's side). A click comes back as the request's key, which the
//! host opens; the helper brings your terminal forward. The helper quits when the connection
//! closes, so it never outlives the server.
//!
//! A banner is held back while you are looking: a window has focus and its terminal app is the
//! frontmost app (the helper checks that last part as it posts).

mod bundle;
mod tracker;

use qm_types::api::ActionKey;
use qm_types::attention::{Health, NotifierEnd, Waiting};
use qm_types::data_dir::DataDir;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tracker::Tracker;

/// How long the helper has to connect after it is launched.
const CONNECT_WITHIN: Duration = Duration::from_secs(20);

/// The first wait before launching the helper again after it failed; it doubles up to the most.
const RETRY_FIRST: Duration = Duration::from_secs(5);
const RETRY_MOST: Duration = Duration::from_secs(600);

/// Why the helper isn't running.
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("can't install the notifier app at {}: {source}", path.display())]
    Install { path: PathBuf, source: io::Error },
    #[error("the notifier app can't draw its icon: {0}")]
    Icon(String),
    #[error("can't sign the notifier app: {0}")]
    Sign(String),
    #[error("can't listen for the notifier app on {}: {source}", path.display())]
    Listen { path: PathBuf, source: io::Error },
    #[error("can't launch the notifier app: {0}")]
    Launch(String),
    #[error("the notifier app didn't connect within {} seconds", CONNECT_WITHIN.as_secs())]
    NoConnect,
    #[error("the notifier app's connection failed: {0}")]
    Connection(io::Error),
}

/// How a session with the helper ended.
enum Ended {
    /// The host is gone: the server is stopping.
    HostGone,
    /// You turned notifications off.
    Off,
    /// The helper went away.
    Left,
}

/// Runs the notifier until the host's end goes away. `since` is when the server started: requests
/// already waiting then get no banner (they're in the menu).
pub async fn run(dir: DataDir, mut end: NotifierEnd, since: jiff::Timestamp) {
    let mut tracker = Tracker::new(since);
    let mut retry = RETRY_FIRST;
    loop {
        // Off: keep up with the inbox, so turning it on doesn't announce what was already there.
        loop {
            let waiting = end.waiting.borrow_and_update().clone();
            if waiting.enabled {
                break;
            }
            end.health.send_replace(Health::Off);
            tracker.absorb(&waiting);
            if end.waiting.changed().await.is_err() {
                return;
            }
        }
        tracker.reconnected();
        end.health.send_replace(Health::Starting);
        match session(&dir, &mut end, &mut tracker).await {
            Ok((Ended::HostGone, _)) => return,
            Ok((Ended::Off, _)) => {
                tracing::info!("notifications turned off");
                retry = RETRY_FIRST;
                continue;
            }
            Ok((Ended::Left, ready)) => {
                tracing::warn!("the notifier app quit; starting it again");
                if ready {
                    retry = RETRY_FIRST;
                }
            }
            Err(e) => {
                end.health.send_replace(Health::Failed);
                tracing::warn!(error = %e, "notifications aren't working");
            }
        }
        if !wait(&mut end, &mut tracker, retry).await {
            return;
        }
        retry = (retry * 2).min(RETRY_MOST);
    }
}

/// Waits `delay` while keeping up with the inbox; `false` when the host went away.
async fn wait(end: &mut NotifierEnd, tracker: &mut Tracker, delay: Duration) -> bool {
    let sleep = tokio::time::sleep(delay);
    tokio::pin!(sleep);
    loop {
        tokio::select! {
            () = &mut sleep => return true,
            changed = end.waiting.changed() => {
                if changed.is_err() {
                    return false;
                }
                tracker.absorb(&end.waiting.borrow_and_update());
            }
        }
    }
}

/// Installs and launches the helper, then keeps it up to date until something ends it. Also says
/// whether the helper said it was ready.
async fn session(
    dir: &DataDir,
    end: &mut NotifierEnd,
    tracker: &mut Tracker,
) -> Result<(Ended, bool), NotifyError> {
    let app = dir.notifier_app();
    let identity = bundle::Identity::of(dir.root());
    let installed = app.clone();
    tokio::task::spawn_blocking(move || bundle::install(&installed, identity))
        .await
        .map_err(|e| NotifyError::Launch(e.to_string()))??;
    let socket = dir.notifier_socket();
    let listener = listen(&socket)?;
    launch(&app, &socket).await?;
    let stream = accept(&listener).await;
    // Nobody else connects: the socket's name goes as soon as the helper is in.
    let _ = std::fs::remove_file(&socket);
    let (read, mut write) = stream?.into_split();
    let mut lines = BufReader::new(read).lines();
    let mut ready = false;

    let first = end.waiting.borrow_and_update().clone();
    tracing::debug!(looking = ?first.looking, "looking");
    let mut looking = first.looking.clone();
    send(&mut write, tracker.step(&first)).await?;
    loop {
        tokio::select! {
            changed = end.waiting.changed() => {
                if changed.is_err() {
                    return Ok((Ended::HostGone, ready));
                }
                let waiting: Waiting = end.waiting.borrow_and_update().clone();
                if !waiting.enabled {
                    tracker.absorb(&waiting);
                    return Ok((Ended::Off, ready));
                }
                if waiting.looking != looking {
                    tracing::debug!(looking = ?waiting.looking, "looking");
                    looking.clone_from(&waiting.looking);
                }
                send(&mut write, tracker.step(&waiting)).await?;
            }
            line = lines.next_line() => match line.map_err(NotifyError::Connection)? {
                Some(line) => heard(&line, end, &mut ready),
                None => return Ok((Ended::Left, ready)),
            },
        }
    }
}

/// Binds the socket the helper connects to: private to you, a stale one replaced.
fn listen(path: &Path) -> Result<UnixListener, NotifyError> {
    let failed = |source| NotifyError::Listen {
        path: path.to_owned(),
        source,
    };
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(failed(e)),
    }
    let listener = UnixListener::bind(path).map_err(failed)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(failed)?;
    Ok(listener)
}

/// Launches the helper through `LaunchServices`, in the background, telling it where to connect.
async fn launch(app: &Path, socket: &Path) -> Result<(), NotifyError> {
    let output = tokio::process::Command::new("/usr/bin/open")
        .args(["-n", "-g"])
        .arg(app)
        .arg("--args")
        .arg("--connect")
        .arg(socket)
        .output()
        .await
        .map_err(|e| NotifyError::Launch(e.to_string()))?;
    if output.status.success() {
        Ok(())
    } else {
        let said = String::from_utf8_lossy(&output.stderr);
        Err(NotifyError::Launch(format!(
            "{} {}",
            output.status,
            said.trim()
        )))
    }
}

/// The helper's connection: the first one from a process of yours.
async fn accept(listener: &UnixListener) -> Result<UnixStream, NotifyError> {
    let uid = rustix::process::getuid().as_raw();
    tokio::time::timeout(CONNECT_WITHIN, async {
        loop {
            let (stream, _) = listener.accept().await.map_err(NotifyError::Connection)?;
            match stream.peer_cred() {
                Ok(peer) if peer.uid() == uid => return Ok(stream),
                _ => tracing::warn!("refused a notifier connection from another user"),
            }
        }
    })
    .await
    .map_err(|_| NotifyError::NoConnect)?
}

async fn send(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    lines: Vec<String>,
) -> Result<(), NotifyError> {
    for line in lines {
        write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .map_err(NotifyError::Connection)?;
    }
    Ok(())
}

/// What the helper said.
#[derive(serde::Deserialize)]
struct Said {
    event: String,
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    error: Option<String>,
    /// Whether macOS shows a banner's text: `always`, `when_unlocked` or `never`.
    #[serde(default)]
    previews: Option<String>,
    /// How many banners were withdrawn.
    #[serde(default)]
    count: Option<String>,
}

fn heard(line: &str, end: &NotifierEnd, ready: &mut bool) {
    let Ok(event) = serde_json::from_str::<Said>(line) else {
        tracing::warn!("the notifier app said something unreadable");
        return;
    };
    match event.event.as_str() {
        "ready" => {
            *ready = true;
            let previews = event.previews.as_deref().unwrap_or("unknown");
            if previews == "never" {
                end.health.send_replace(Health::TextHidden);
                tracing::info!(
                    "notifications ready, but macOS hides their text (Show previews is Never)"
                );
            } else {
                end.health.send_replace(Health::Ready);
                tracing::info!(previews, "notifications ready");
            }
        }
        "denied" => {
            *ready = true;
            end.health.send_replace(Health::Denied);
            tracing::info!(
                error = event.error.as_deref().unwrap_or(""),
                "notifications are off in System Settings; the menu bar mark still shows"
            );
        }
        // What became of each banner, for the log: requests are named by key, never by title.
        "posted" => {
            if let Some(error) = event.error {
                tracing::warn!(key = event.key.as_deref(), error, "a banner couldn't post");
            } else {
                tracing::info!(key = event.key.as_deref(), "banner posted");
            }
        }
        "held" => tracing::info!(
            key = event.key.as_deref(),
            "banner held back: you are looking at Quartermaster"
        ),
        "withdrawn" => {
            tracing::debug!(count = event.count.as_deref(), "answered banners withdrawn");
        }
        "open" => {
            let key = event
                .key
                .and_then(|k| serde_json::from_str::<ActionKey>(&k).ok());
            if let Some(key) = key {
                let _ = end.opened.send(key);
            }
        }
        _ => {}
    }
}
