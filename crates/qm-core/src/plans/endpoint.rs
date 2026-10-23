//! The plan pages' endpoint: a small HTTP/1.1 listener on `127.0.0.1`, in the server process,
//! that takes a plan reply sent from a page ([`super::submit`]) and nothing else.
//!
//! - **One route**, [`qm_plans::SUBMIT_PATH`]: `POST` a submission, or `OPTIONS` for a browser's
//!   preflight (which does nothing). Any other path is 404 and any other method 405, both with no
//!   body. Nothing lists tasks, reads state or reaches any other part of the app.
//! - **Checks before the core sees anything**: `Host` is exactly `127.0.0.1:<port>` (against DNS
//!   rebinding); `Origin` is `null` or `file://`, what a page opened from disk sends (a website
//!   sends its own origin); `Content-Type` is `text/plain`, so the browser needs no preflight.
//! - **Limits**: 16 KiB of headers, a 160 KiB body (refused by its `Content-Length` before it is
//!   read), 5 s to send the request, 4 connections at once, `Connection: close` on every answer.
//! - **The port is kept** in [`DataDir::plan_endpoint`], so a page rendered before a restart still
//!   reaches the app. When that port is taken a new one is saved and the open pages are rendered
//!   again for it; when nothing can be bound the pages are rendered without Send.
//!
//! **Nothing about a request is logged** (its body carries the page's key and the user's reply):
//! only what happened, and for a submission its task, round and outcome.

use super::submit::{self, Answer, Status};
use crate::tasks::TaskService;
use qm_types::data_dir::DataDir;
use qm_types::domain::TaskKind;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, watch};
use tokio::task::JoinHandle;

/// The most request-line and header bytes read.
const MAX_HEADER_BYTES: usize = 16 * 1024;
/// The largest body taken.
const MAX_BODY_BYTES: usize = 160 * 1024;
/// How long a connection may take to send its whole request.
const READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections answered at once.
const MAX_CONNECTIONS: u32 = 4;
/// How long stopping waits for connections being answered.
const STOP_GRACE: Duration = Duration::from_secs(2);

/// The origins a page opened from disk sends: Safari and Chrome send `null`; `file://` is taken
/// in case a browser sends that.
const PAGE_ORIGINS: [&str; 2] = ["null", "file://"];

/// The listening endpoint; [`Endpoint::stop`] ends it.
#[derive(Debug)]
pub struct Endpoint {
    port: u16,
    stop: watch::Sender<bool>,
    accepting: JoinHandle<()>,
    slots: Arc<Semaphore>,
}

/// `plan-endpoint.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    port: u16,
}

impl Endpoint {
    /// Binds the endpoint (on the saved port when it can), saves its port, renders the open
    /// pages again for it when it changed, and starts answering. `None` when nothing can be
    /// bound: the pages then offer Copy reply only.
    pub async fn start(svc: TaskService) -> Option<Self> {
        let data = svc.deps().data_dir.clone();
        let saved = read_saved(&data);
        let listener = match bind(saved).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::warn!(error = %e, "the plan pages' endpoint couldn't listen; pages offer Copy reply only");
                refresh_pages(&svc, None).await;
                return None;
            }
        };
        let port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(e) => {
                tracing::warn!(error = %e, "the plan pages' endpoint has no address");
                refresh_pages(&svc, None).await;
                return None;
            }
        };
        if saved != Some(port) {
            if let Some(old) = saved {
                tracing::info!(
                    old,
                    port,
                    "the plan pages' saved port was taken; using a new one"
                );
            }
            if let Err(e) = save(&data, port) {
                tracing::warn!(error = %e, "couldn't save the plan pages' port");
            }
        }
        svc.set_plan_port(Some(port));
        refresh_pages(&svc, Some(port)).await;
        tracing::info!(port, "plan pages' endpoint listening on 127.0.0.1");

        let (stop, stopped) = watch::channel(false);
        let slots = Arc::new(Semaphore::new(MAX_CONNECTIONS as usize));
        let accepting = tokio::spawn(accept_loop(
            svc,
            listener,
            port,
            Arc::clone(&slots),
            stopped,
        ));
        Some(Self {
            port,
            stop,
            accepting,
            slots,
        })
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stops taking connections and waits a moment for the ones being answered. The saved port
    /// stays, so the pages still send after the next start.
    pub async fn stop(self, svc: &TaskService) {
        let _ = self.stop.send(true);
        let _ = self.accepting.await;
        let all = tokio::time::timeout(STOP_GRACE, self.slots.acquire_many(MAX_CONNECTIONS)).await;
        if all.is_err() {
            tracing::warn!("stopped the plan pages' endpoint with a request still being answered");
        }
        svc.set_plan_port(None);
    }
}

fn read_saved(data: &DataDir) -> Option<u16> {
    let text = std::fs::read_to_string(data.plan_endpoint()).ok()?;
    let saved: Saved = serde_json::from_str(&text).ok()?;
    (saved.port != 0).then_some(saved.port)
}

fn save(data: &DataDir, port: u16) -> std::io::Result<()> {
    let text = serde_json::to_string(&Saved { port }).map_err(std::io::Error::other)?;
    qm_types::fsio::write_atomic(&data.plan_endpoint(), text.as_bytes())
}

/// Binds the saved port, else any free one.
async fn bind(saved: Option<u16>) -> std::io::Result<TcpListener> {
    if let Some(port) = saved {
        match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(listener) => return Ok(listener),
            Err(e) => {
                tracing::info!(port, error = %e, "the plan pages' saved port can't be bound; using another");
            }
        }
    }
    TcpListener::bind(("127.0.0.1", 0)).await
}

/// Renders each open plan page that takes a reply again when it sends somewhere other than
/// `port` (or `None`: nowhere), so its policy and address match the running endpoint.
async fn refresh_pages(svc: &TaskService, port: Option<u16>) {
    let data = svc.deps().data_dir.clone();
    let pages = tokio::task::spawn_blocking(move || super::api::live(&data))
        .await
        .unwrap_or_default();
    for (task, state, dir) in pages {
        let wants = if state.takes_page_reply() && state.page_token.is_some() {
            port
        } else {
            None
        };
        if state.page_port == wants {
            continue;
        }
        let open = svc
            .db()
            .call(move |conn| qm_store::tasks::load(conn, task))
            .await
            .ok()
            .flatten()
            .is_some_and(|row| {
                row.kind == TaskKind::Plan && crate::tasks::lifecycle::is_open(row.status)
            });
        if !open {
            continue;
        }
        let project = svc.project_name_of(task).await;
        match tokio::task::spawn_blocking(move || super::rerender(&dir, task, &project, port)).await
        {
            Ok(Ok(true)) => tracing::info!(%task, "rendered a plan page again for the endpoint"),
            Ok(Ok(false)) => {
                tracing::info!(%task, "left a plan page whose plan changed since it was shown");
            }
            Ok(Err(why)) => tracing::warn!(%task, %why, "couldn't render a plan page again"),
            Err(e) => tracing::warn!(%task, error = %e, "couldn't render a plan page again"),
        }
    }
}

async fn accept_loop(
    svc: TaskService,
    listener: TcpListener,
    port: u16,
    slots: Arc<Semaphore>,
    mut stopped: watch::Receiver<bool>,
) {
    loop {
        // A slot first, so a fifth connection waits in the backlog rather than being read.
        let permit = tokio::select! {
            permit = Arc::clone(&slots).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return,
            },
            _ = stopped.changed() => return,
        };
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = stopped.changed() => return,
        };
        match accepted {
            Ok((stream, _)) => {
                let svc = svc.clone();
                tokio::spawn(async move {
                    answer(&svc, stream, port).await;
                    drop(permit);
                });
            }
            Err(e) => {
                tracing::debug!(error = %e, "the plan pages' endpoint couldn't accept a connection");
                drop(permit);
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// A request as read: its line, the headers that matter, and its body.
#[derive(Debug, Default)]
struct Request {
    method: String,
    target: String,
    host: Option<String>,
    origin: Option<String>,
    content_type: Option<String>,
    content_length: Option<String>,
    private_network: bool,
    body: Vec<u8>,
}

/// Why a request was refused before it was understood.
#[derive(Debug, PartialEq, Eq)]
enum Refused {
    /// Not a request we can read.
    Bad,
    /// Headers over the limit.
    TooLarge,
    /// A body over the limit, by its `Content-Length`.
    BodyTooLarge,
    /// A `POST` without a `Content-Length`.
    NoLength,
    /// The connection closed or timed out.
    Gone,
}

/// Answers one connection.
async fn answer(svc: &TaskService, mut stream: TcpStream, port: u16) {
    let read = tokio::time::timeout(READ_TIMEOUT, read_request(&mut stream)).await;
    let response = match read {
        Err(_) | Ok(Err(Refused::Gone)) => return,
        Ok(Err(Refused::Bad)) => Response::empty(400),
        Ok(Err(Refused::TooLarge)) => Response::empty(431),
        Ok(Err(Refused::BodyTooLarge)) => Response::empty(413),
        Ok(Err(Refused::NoLength)) => Response::empty(411),
        Ok(Ok(request)) => route(svc, &request, port).await,
    };
    let _ = stream.write_all(&response.bytes()).await;
    let _ = stream.shutdown().await;
}

/// What the endpoint does with a request it read.
async fn route(svc: &TaskService, request: &Request, port: u16) -> Response {
    if request.target != qm_plans::SUBMIT_PATH {
        tracing::debug!("plan pages' endpoint: a request on an unknown path");
        return Response::empty(404);
    }
    if !matches!(request.method.as_str(), "POST" | "OPTIONS") {
        return Response::empty(405).header("Allow", "POST, OPTIONS");
    }
    if request.host.as_deref() != Some(format!("127.0.0.1:{port}").as_str()) {
        tracing::debug!("plan pages' endpoint: refused a request for another host");
        return Response::refused();
    }
    let Some(origin) = request
        .origin
        .as_deref()
        .and_then(|o| PAGE_ORIGINS.iter().find(|p| **p == o).copied())
    else {
        tracing::debug!("plan pages' endpoint: refused a request from another origin");
        return Response::refused();
    };
    if request.method == "OPTIONS" {
        let mut response = Response::empty(204)
            .header("Access-Control-Allow-Origin", origin)
            .header("Access-Control-Allow-Methods", "POST")
            .header("Access-Control-Allow-Headers", "content-type")
            .header("Access-Control-Max-Age", "600")
            .header("Vary", "Origin");
        if request.private_network {
            response = response.header("Access-Control-Allow-Private-Network", "true");
        }
        return response;
    }
    if !plain_text(request.content_type.as_deref()) {
        return Response::empty(415).header("Access-Control-Allow-Origin", origin);
    }
    let answer = submit::submit(svc, &request.body).await;
    Response::json(&answer, origin)
}

/// `text/plain`, optionally with `charset=utf-8`.
fn plain_text(content_type: Option<&str>) -> bool {
    let Some(value) = content_type else {
        return false;
    };
    let mut parts = value.split(';').map(str::trim);
    if !parts
        .next()
        .is_some_and(|t| t.eq_ignore_ascii_case("text/plain"))
    {
        return false;
    }
    parts.all(|p| {
        p.split_once('=').is_some_and(|(k, v)| {
            k.trim().eq_ignore_ascii_case("charset")
                && v.trim().trim_matches('"').eq_ignore_ascii_case("utf-8")
        })
    })
}

/// Reads the request line, the headers and (for `POST`) the body.
async fn read_request(stream: &mut TcpStream) -> Result<Request, Refused> {
    let mut buf = Vec::with_capacity(2048);
    let mut chunk = [0_u8; 4096];
    let end = loop {
        if let Some(at) = find_end(&buf) {
            break at;
        }
        if buf.len() > MAX_HEADER_BYTES {
            return Err(Refused::TooLarge);
        }
        let n = stream.read(&mut chunk).await.map_err(|_| Refused::Gone)?;
        if n == 0 {
            return Err(Refused::Gone);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    if end > MAX_HEADER_BYTES {
        return Err(Refused::TooLarge);
    }
    let mut request = parse_head(&buf[..end])?;
    if request.method != "POST" {
        return Ok(request);
    }
    let length = match request.content_length.as_deref() {
        None => return Err(Refused::NoLength),
        Some(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
            text.parse::<usize>().map_err(|_| Refused::BodyTooLarge)?
        }
        Some(_) => return Err(Refused::Bad),
    };
    if length > MAX_BODY_BYTES {
        return Err(Refused::BodyTooLarge);
    }
    let mut body = buf[end + 4..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk).await.map_err(|_| Refused::Gone)?;
        if n == 0 {
            return Err(Refused::Gone);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    request.body = body;
    Ok(request)
}

/// Where the headers end (`\r\n\r\n`).
fn find_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// The request line and the headers that matter. A header that matters given twice, a folded
/// line or a `Transfer-Encoding` makes the request unreadable.
fn parse_head(head: &[u8]) -> Result<Request, Refused> {
    let text = std::str::from_utf8(head).map_err(|_| Refused::Bad)?;
    let mut lines = text.split("\r\n");
    let line = lines.next().ok_or(Refused::Bad)?;
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Refused::Bad);
    };
    if !matches!(version, "HTTP/1.1" | "HTTP/1.0") || method.is_empty() || target.is_empty() {
        return Err(Refused::Bad);
    }
    let mut request = Request {
        method: method.to_owned(),
        target: target.to_owned(),
        ..Request::default()
    };
    for line in lines {
        if line.starts_with([' ', '\t']) {
            return Err(Refused::Bad);
        }
        let (name, value) = line.split_once(':').ok_or(Refused::Bad)?;
        if name.is_empty() || name.contains([' ', '\t']) {
            return Err(Refused::Bad);
        }
        let value = value.trim_matches([' ', '\t']).to_owned();
        let slot = match name.to_ascii_lowercase().as_str() {
            "host" => &mut request.host,
            "origin" => &mut request.origin,
            "content-type" => &mut request.content_type,
            "content-length" => &mut request.content_length,
            "transfer-encoding" => return Err(Refused::Bad),
            "access-control-request-private-network" => {
                request.private_network = value.eq_ignore_ascii_case("true");
                continue;
            }
            _ => continue,
        };
        if slot.replace(value).is_some() {
            return Err(Refused::Bad);
        }
    }
    Ok(request)
}

/// An answer.
#[derive(Debug)]
struct Response {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Response {
    fn empty(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    fn header(mut self, name: &'static str, value: &str) -> Self {
        self.headers.push((name, value.to_owned()));
        self
    }

    /// 403 `forbidden`, for a request from somewhere other than a plan page: no
    /// `Access-Control-Allow-Origin`, so no other origin can read it.
    fn refused() -> Self {
        Self::json_body(&Answer {
            status: Status::Forbidden,
            message: "This request isn't from a plan page.".to_owned(),
        })
    }

    fn json(answer: &Answer, origin: &str) -> Self {
        Self::json_body(answer)
            .header("Access-Control-Allow-Origin", origin)
            .header("Vary", "Origin")
    }

    fn json_body(answer: &Answer) -> Self {
        let body = serde_json::json!({
            "status": answer.status.word(),
            "message": answer.message,
        })
        .to_string();
        Self {
            status: answer.status.http(),
            headers: vec![("Content-Type", "application/json; charset=utf-8".to_owned())],
            body: body.into_bytes(),
        }
    }

    fn bytes(&self) -> Vec<u8> {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
        for (name, value) in &self.headers {
            let _ = write!(head, "{name}: {value}\r\n");
        }
        let _ = write!(
            head,
            "Content-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
             Connection: close\r\n\r\n",
            self.body.len()
        );
        let mut out = head.into_bytes();
        out.extend_from_slice(&self.body);
        out
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "",
    }
}
