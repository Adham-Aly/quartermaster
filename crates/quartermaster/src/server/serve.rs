//! The socket: accepting connections, the peer check, the handshake and the control requests.

use super::host::{Host, QuitOutcome};
use qm_types::wire::{
    self, Frame, Hello, Kind, PROTOCOL, Refuse, Request, Response, ServerStatus, Welcome,
};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::time::timeout;

/// macOS refuses a socket path longer than this many bytes (including the terminator).
const MAX_SOCKET_PATH: usize = 104;
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
/// Frames queued for one window before it counts as stalled and misses frames.
const OUT_QUEUE: usize = 64;

/// What the connection handlers share.
pub struct Shared {
    pub host: Host,
    pub started: Instant,
    pub started_unix: i64,
    next_id: AtomicU64,
    /// A stop was asked for over the socket, and why.
    pub stop: mpsc::UnboundedSender<Stop>,
    uid: u32,
}

/// A stop asked for over the socket.
pub enum Stop {
    Stop,
    Restart,
}

impl Shared {
    pub fn new(
        host: Host,
        started_unix: i64,
        stop: mpsc::UnboundedSender<Stop>,
        uid: u32,
    ) -> Arc<Shared> {
        Arc::new(Shared {
            host,
            started: Instant::now(),
            started_unix,
            next_id: AtomicU64::new(1),
            stop,
            uid,
        })
    }
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
    match first.kind {
        Kind::Hello => attach(first, rd, wr, shared).await,
        Kind::Request => request(&first, wr, shared).await,
        _ => refuse(&mut wr, "Expected a hello first.").await,
    }
}

async fn refuse(wr: &mut tokio::net::unix::OwnedWriteHalf, reason: &str) {
    let frame = Frame::json(
        Kind::Refuse,
        &Refuse {
            reason: reason.to_owned(),
        },
    );
    let _ = wire::write_frame(wr, &frame).await;
}

async fn attach(
    first: Frame,
    mut rd: OwnedReadHalf,
    mut wr: tokio::net::unix::OwnedWriteHalf,
    shared: Arc<Shared>,
) {
    let Ok(hello) = first.parse::<Hello>() else {
        refuse(&mut wr, "That hello wasn't readable.").await;
        return;
    };
    if hello.protocol != PROTOCOL {
        let reason = format!(
            "This Quartermaster server speaks protocol {PROTOCOL} and your client speaks {}; \
             run `quartermaster server restart` to use the version you installed.",
            hello.protocol
        );
        refuse(&mut wr, &reason).await;
        return;
    }
    let id = shared.next_id.fetch_add(1, Ordering::Relaxed);
    let welcome = Welcome {
        client_id: id,
        version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol: PROTOCOL,
    };
    if wire::write_frame(&mut wr, &Frame::json(Kind::Welcome, &welcome))
        .await
        .is_err()
    {
        return;
    }
    tracing::info!(client = id, size = ?hello.size, ssh = hello.ssh, "window attached");

    let (out, mut out_rx) = mpsc::channel::<Frame>(OUT_QUEUE);
    shared.host.attach(id, hello, out);
    // The writer ends when the host lets go of the window's sender.
    let writer = tokio::spawn(async move {
        while let Some(frame) = out_rx.recv().await {
            if wire::write_frame(&mut wr, &frame).await.is_err() {
                break;
            }
        }
    });
    while let Ok(Some(f)) = wire::read_frame(&mut rd).await {
        match f.kind {
            Kind::Input => {
                if let Ok(event) = f.parse() {
                    shared.host.input(id, event);
                }
            }
            Kind::Resize => {
                if let Ok(size) = f.parse() {
                    shared.host.resize(id, size);
                }
            }
            Kind::Detach => break,
            _ => {}
        }
    }
    shared.host.detach(id);
    tracing::info!(client = id, "window detached");
    // Dropping the host's sender (above) ends the writer; don't wait for a stalled one.
    let _ = timeout(Duration::from_secs(1), writer).await;
}

async fn request(first: &Frame, mut wr: tokio::net::unix::OwnedWriteHalf, shared: Arc<Shared>) {
    let Ok(request) = first.parse::<Request>() else {
        refuse(&mut wr, "That request wasn't readable.").await;
        return;
    };
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
            match shared.host.quit(force).await {
                QuitOutcome::Working(working) => Response::Working { working },
                QuitOutcome::Closing => {
                    let stop = if matches!(request, Request::Restart { .. }) {
                        Stop::Restart
                    } else {
                        Stop::Stop
                    };
                    // Answer first: the server may be gone before a late write.
                    let _ = wire::write_frame(
                        &mut wr,
                        &Frame::json(Kind::Response, &Response::Stopping),
                    )
                    .await;
                    let _ = shared.stop.send(stop);
                    return;
                }
            }
        }
    };
    let _ = wire::write_frame(&mut wr, &Frame::json(Kind::Response, &response)).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use qm_types::wire::{Size, read_frame, write_frame};

    /// A server on a throwaway socket with the stub host and no core behind it.
    fn serve_on(name: &str) -> (std::path::PathBuf, qm_types::api::CoreServer) {
        let path = std::env::temp_dir().join(format!("qm-{name}-{}.sock", std::process::id()));
        let listener = bind(&path).unwrap();
        let (client, core_end) = qm_types::api::link();
        let (host, _done) = super::super::host::spawn(client);
        let (stop, _stop_rx) = mpsc::unbounded_channel();
        let shared = Shared::new(host, 0, stop, nix::unistd::getuid().as_raw());
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(connection(stream, shared.clone()));
            }
        });
        (path, core_end)
    }

    #[tokio::test]
    async fn the_socket_is_private_and_a_hello_gets_a_welcome_and_a_screen() {
        let (path, _core) = serve_on("hello");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let mut s = UnixStream::connect(&path).await.unwrap();
        let hello = Hello {
            protocol: PROTOCOL,
            size: Size { cols: 80, rows: 24 },
            ..Hello::default()
        };
        write_frame(&mut s, &Frame::json(Kind::Hello, &hello))
            .await
            .unwrap();
        let welcome = read_frame(&mut s).await.unwrap().unwrap();
        assert_eq!(welcome.kind, Kind::Welcome);
        let screen = read_frame(&mut s).await.unwrap().unwrap();
        assert_eq!(screen.kind, Kind::Output);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn another_protocol_is_refused_with_a_sentence() {
        let (path, _core) = serve_on("proto");
        let mut s = UnixStream::connect(&path).await.unwrap();
        let hello = Hello {
            protocol: PROTOCOL + 1,
            ..Hello::default()
        };
        write_frame(&mut s, &Frame::json(Kind::Hello, &hello))
            .await
            .unwrap();
        let refusal = read_frame(&mut s).await.unwrap().unwrap();
        assert_eq!(refusal.kind, Kind::Refuse);
        assert!(
            refusal
                .parse::<Refuse>()
                .unwrap()
                .reason
                .contains("server restart")
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn status_counts_windows() {
        let (path, _core) = serve_on("status");
        let mut s = UnixStream::connect(&path).await.unwrap();
        write_frame(&mut s, &Frame::json(Kind::Request, &Request::Status))
            .await
            .unwrap();
        let answer = read_frame(&mut s).await.unwrap().unwrap();
        let Response::Status(status) = answer.parse().unwrap() else {
            panic!("expected a status");
        };
        assert_eq!(status.windows, 0);
        assert_eq!(status.pid, std::process::id());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_long_socket_path_is_refused_in_words() {
        let long = std::path::PathBuf::from(format!("/tmp/{}/server.sock", "x".repeat(120)));
        assert!(bind(&long).unwrap_err().contains("shorter folder"));
    }
}
