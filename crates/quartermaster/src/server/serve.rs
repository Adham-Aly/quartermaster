//! The socket: accepting connections, the peer check, the handshake and the control requests.

use qm_tui::host::{Conn, Handle, QuitOutcome, Windows};
use qm_types::wire::{self, ExitReason, Frame, PROTOCOL, Refuse, Request, Response, ServerStatus};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// macOS refuses a socket path longer than this many bytes (including the terminator).
const MAX_SOCKET_PATH: usize = 104;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);

/// What the connection handlers share.
pub struct Shared {
    host: Handle,
    windows: mpsc::UnboundedSender<Box<dyn Conn>>,
    started: Instant,
    started_unix: i64,
    uid: u32,
}

impl Shared {
    /// `windows` is where the UI host receives new windows' streams.
    pub fn new(
        host: Handle,
        windows: mpsc::UnboundedSender<Box<dyn Conn>>,
        started_unix: i64,
        uid: u32,
    ) -> Arc<Shared> {
        Arc::new(Shared {
            host,
            windows,
            started: Instant::now(),
            started_unix,
            uid,
        })
    }
}

/// The channel the host takes windows from, and the end the server sends them on.
pub fn windows() -> (mpsc::UnboundedSender<Box<dyn Conn>>, Windows) {
    mpsc::unbounded_channel()
}

/// Binds `path`, replacing a stale socket. Call only while holding `run.lock`: holding it proves
/// nobody else serves this data directory.
pub fn bind(path: &Path) -> Result<UnixListener, String> {
    if path.as_os_str().len() >= MAX_SOCKET_PATH {
        return Err(format!(
            "the socket path {} is longer than macOS allows ({} bytes); \
             set QUARTERMASTER_HOME to a shorter folder",
            path.display(),
            MAX_SOCKET_PATH - 1
        ));
    }
    let _ = std::fs::remove_file(path);
    let listener =
        UnixListener::bind(path).map_err(|e| format!("can't listen on {}: {e}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("can't make {} private: {e}", path.display()))?;
    Ok(listener)
}

/// One connection, from accept to close.
pub async fn connection(stream: UnixStream, shared: Arc<Shared>) {
    match stream.peer_cred() {
        Ok(cred) if cred.uid() == shared.uid => {}
        Ok(cred) => {
            tracing::warn!(uid = cred.uid(), "refused a connection from another user");
            return;
        }
        Err(e) => {
            tracing::warn!("couldn't read who connected: {e}");
            return;
        }
    }
    let (mut rd, mut wr) = stream.into_split();
    let first = match timeout(HELLO_TIMEOUT, wire::read_frame(&mut rd)).await {
        Ok(Ok(Some(frame))) => frame,
        Ok(Err(e)) => {
            tracing::debug!("bad first frame: {e}");
            refuse(&mut wr, "That wasn't a Quartermaster client.").await;
            return;
        }
        Ok(Ok(None)) | Err(_) => return,
    };
    match first {
        Frame::Hello(_) => attach(&first, rd, wr, &shared),
        Frame::Request(request) => answer(request, wr, &shared).await,
        _ => refuse(&mut wr, "Expected a hello first.").await,
    }
}

async fn refuse(wr: &mut OwnedWriteHalf, reason: &str) {
    let _ = wire::write_frame(wr, &Frame::Refuse(Refuse::new(reason))).await;
}

/// Hands a window to the UI host. The host reads the hello itself (and checks its protocol), so
/// the frame already read here is put back in front of the stream.
fn attach(hello: &Frame, rd: OwnedReadHalf, wr: OwnedWriteHalf, shared: &Shared) {
    let Ok(bytes) = hello.encode() else { return };
    let reader = tokio::io::AsyncReadExt::chain(std::io::Cursor::new(bytes), rd);
    let conn: Box<dyn Conn> = Box::new(tokio::io::join(reader, wr));
    if shared.windows.send(conn).is_ok() {
        tracing::info!("window attached");
    }
}

async fn answer(request: Request, mut wr: OwnedWriteHalf, shared: &Shared) {
    let response = match request {
        Request::Status => {
            let host = shared.host.status().await;
            Response::Status(ServerStatus {
                pid: std::process::id(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                protocol: PROTOCOL,
                started_unix: shared.started_unix,
                uptime_secs: shared.started.elapsed().as_secs(),
                windows: host.windows,
                working: host.working,
            })
        }
        Request::Stop { force } | Request::Restart { force } => {
            let restart = matches!(request, Request::Restart { .. });
            let reason = if restart {
                ExitReason::Restarting
            } else {
                ExitReason::ServerStopped
            };
            match shared.host.quit(force, reason).await {
                QuitOutcome::Working(working) => Response::Working { working },
                QuitOutcome::Closing => {
                    // Answer first: the server may be gone before a late write.
                    let _ = wire::write_frame(&mut wr, &Frame::Response(Response::Stopping)).await;
                    return;
                }
            }
        }
    };
    let _ = wire::write_frame(&mut wr, &Frame::Response(response)).await;
}
