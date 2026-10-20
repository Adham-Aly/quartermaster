//! The one-shot local HTTP listener a browser login redirects back to.
//!
//! Adapted from Pi `packages/ai/src/auth/oauth/callback-server.ts` and the callback server in
//! `openai-chatgpt.ts` (MIT), and OpenAI Codex `codex-rs/login/src/server.rs` (Apache-2.0,
//! Copyright OpenAI). It listens on `127.0.0.1:<port>` only, answers any other path with 404,
//! answers a callback it can't accept with a small error page and keeps waiting (a stray or stale
//! request must not end the login), and resolves on the first callback that passes the caller's
//! check.
//!
//! **Nothing about a request is logged**: its query string carries the authorization code and
//! `state`. Log lines name only what happened ("a callback on an unknown path").

use crate::AuthError;
use std::io;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use url::Url;

/// The path every ChatGPT login redirects to.
pub const CALLBACK_PATH: &str = "/auth/callback";

/// The most of a request we read before giving up on it: the request line and headers only.
const MAX_REQUEST_BYTES: usize = 16 * 1024;

/// How long one connection may take to send its request line and headers.
const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A bound, not yet answered callback listener.
#[derive(Debug)]
pub struct CallbackServer {
    listener: TcpListener,
    port: u16,
    path: String,
}

/// What a callback check decided about one request.
#[derive(Debug)]
pub enum CallbackVerdict<T> {
    /// Accepted: the login goes on with this.
    Accept(T),
    /// The browser reported an error (`error=…`): the login fails with this message.
    Fail(String),
    /// Not acceptable (a missing code, a state mismatch): an error page with this message, and the
    /// server keeps waiting for a good callback.
    Reject(String),
}

impl CallbackServer {
    /// Binds `127.0.0.1:<port>` for callbacks on [`CALLBACK_PATH`]. Port 0 picks a free port;
    /// [`CallbackServer::port`] says which.
    pub async fn bind(port: u16) -> Result<Self, AuthError> {
        Self::bind_with_path(port, CALLBACK_PATH).await
    }

    /// Binds `127.0.0.1:<port>` for callbacks on `path` (OpenRouter's random path).
    pub async fn bind_with_path(port: u16, path: &str) -> Result<Self, AuthError> {
        let listener = TcpListener::bind(("127.0.0.1", port)).await.map_err(|e| {
            if e.kind() == io::ErrorKind::AddrInUse {
                AuthError::Login(format!(
                    "Port {port} is in use, probably by another login or the Codex CLI. \
                         Cancel that login and try again."
                ))
            } else {
                AuthError::Login(format!(
                    "couldn't listen on 127.0.0.1:{port} for the login callback: {e}"
                ))
            }
        })?;
        let port = listener.local_addr().map_err(AuthError::Io)?.port();
        Ok(Self {
            listener,
            port,
            path: path.to_owned(),
        })
    }

    /// The port it listens on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The path callbacks arrive on.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The redirect URI to register: `http://127.0.0.1:<port><path>`.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}{}", self.port, self.path)
    }

    /// Answers requests until `check` accepts or fails one. `check` sees the callback URL
    /// (`http://127.0.0.1:<port>/auth/callback?…`) of every request on the server's path.
    pub async fn wait<T>(
        self,
        mut check: impl FnMut(&Url) -> CallbackVerdict<T>,
    ) -> Result<T, AuthError> {
        loop {
            let (mut stream, _) = self.listener.accept().await.map_err(AuthError::Io)?;
            // A connection that sends nothing must not hold up the login.
            let Ok(Some(target)) =
                tokio::time::timeout(READ_TIMEOUT, read_request_target(&mut stream)).await
            else {
                continue;
            };
            let Ok(url) = Url::parse(&format!("http://127.0.0.1:{}{target}", self.port)) else {
                respond(&mut stream, 400, &page("That callback could not be read.")).await;
                continue;
            };
            if url.path() != self.path {
                tracing::debug!("login callback server: a request on an unknown path");
                respond(&mut stream, 404, &page("Callback route not found.")).await;
                continue;
            }
            match check(&url) {
                CallbackVerdict::Accept(value) => {
                    respond(
                        &mut stream,
                        200,
                        &page("Signed in. You can close this window and go back to Quartermaster."),
                    )
                    .await;
                    return Ok(value);
                }
                CallbackVerdict::Fail(message) => {
                    respond(&mut stream, 400, &page("The account was not connected.")).await;
                    return Err(AuthError::Login(message));
                }
                CallbackVerdict::Reject(message) => {
                    tracing::debug!("login callback server: rejected a callback");
                    respond(&mut stream, 400, &page(&message)).await;
                }
            }
        }
    }
}

/// Reads the request line and headers; returns the request target (`/auth/callback?…`).
async fn read_request_target(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0_u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() >= MAX_REQUEST_BYTES {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let line = text.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    if method != "GET" || !target.starts_with('/') {
        return None;
    }
    Some(target.to_owned())
}

async fn respond(stream: &mut TcpStream, status: u16, body: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        _ => "Bad Request",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n{body}",
        body.len()
    );
    // The browser going away mid-answer changes nothing for the login.
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

fn page(message: &str) -> String {
    format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Quartermaster</title>\
         <style>body{{font:16px -apple-system,system-ui,sans-serif;margin:4em auto;max-width:32em;\
         padding:0 1em;color:#222}}@media(prefers-color-scheme:dark){{body{{background:#1c1c1e;\
         color:#eee}}}}</style></head><body><h1>Quartermaster</h1><p>{}</p></body></html>",
        escape_html(message)
    )
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}
