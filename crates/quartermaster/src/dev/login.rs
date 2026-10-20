//! `quartermaster dev login chatgpt --ref REF [--codex]`.
//!
//! Runs Sign in with ChatGPT (or, with `--codex`, the Codex login), opens the browser with `open`
//! and prints the URL too, and stores the credential under `--ref` in `<home>/auth.json`. When the
//! browser can't reach this Mac's callback port (another device, a remote session), paste the URL
//! the browser ended on: it is handed to the local callback server, which finishes the login the
//! same way. Prints the ref and the expiry, never a token.

use super::{Flags, Startup, dev_fail};
use crate::Failure;
use qm_auth::chatgpt::{ChatGptEndpoints, ChatGptFlow, ChatGptLogin, install_id};
use qm_auth::{Credential, CredentialRef, CredentialStore, FileCredentialStore};
use std::io::{Read, Write};
use std::time::Duration;

/// How long the login waits for the browser.
const LOGIN_TIMEOUT: Duration = Duration::from_mins(10);

pub(super) async fn run(startup: &Startup, flags: &Flags) -> Result<(), Failure> {
    let reference = flags
        .get("--ref")
        .ok_or_else(|| dev_fail("dev login chatgpt needs --ref REF (e.g. chatgpt:dev)"))?;
    let key = CredentialRef::new(reference).map_err(|e| dev_fail(e.to_string()))?;
    let flow = if flags.has("--codex") {
        ChatGptFlow::Codex
    } else {
        ChatGptFlow::SignInWithChatGpt
    };
    let id = install_id(&startup.dir).map_err(|e| dev_fail(e.to_string()))?;
    let pending = ChatGptLogin::new(flow, ChatGptEndpoints::default(), id)
        .start()
        .await
        .map_err(|e| dev_fail(e.to_string()))?;
    let port = pending.callback_port();
    println!("Open this URL to log in (opening your browser now):");
    println!("{}", pending.authorize_url());
    println!(
        "If the browser can't reach this Mac, paste the URL it ends on (http://…:{port}/auth/callback?…) here."
    );
    let _ = std::process::Command::new("open")
        .arg(pending.authorize_url())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    // A pasted callback URL is replayed against the local callback server.
    std::thread::spawn(move || {
        let mut line = String::new();
        while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
            let pasted = line.trim().to_owned();
            line.clear();
            if pasted.is_empty() {
                continue;
            }
            match replay_callback(&pasted, port) {
                Ok(()) => {}
                Err(e) => println!("couldn't use that URL: {e}"),
            }
        }
    });

    let credential = pending
        .finish(LOGIN_TIMEOUT)
        .await
        .map_err(|e| dev_fail(e.to_string()))?;
    let expiry = match &credential {
        Credential::OAuth { expires_at, .. } => {
            expires_at.map_or_else(|| "no expiry".to_owned(), |t| t.to_string())
        }
        Credential::ApiKey { .. } => "no expiry".to_owned(),
    };
    let store = FileCredentialStore::open(startup.dir.auth_file())
        .map_err(|e| dev_fail(format!("can't open the credential file: {e}")))?;
    store
        .put(&key, credential)
        .map_err(|e| dev_fail(format!("can't store the credential: {e}")))?;
    println!("stored {key} (expires {expiry})");
    Ok(())
}

/// Sends the pasted callback's path and query to the local callback server.
fn replay_callback(pasted: &str, port: u16) -> Result<(), String> {
    let after_scheme = pasted.split_once("://").map_or(pasted, |(_, rest)| rest);
    let path = after_scheme
        .find('/')
        .map(|i| &after_scheme[i..])
        .ok_or("it has no path")?;
    if !path.starts_with(qm_auth::callback::CALLBACK_PATH) {
        return Err(format!(
            "it should be the callback URL ({})",
            qm_auth::callback::CALLBACK_PATH
        ));
    }
    if path.contains(['\r', '\n', ' ']) {
        return Err("it contains spaces or line breaks".into());
    }
    let mut stream =
        std::net::TcpStream::connect(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| e.to_string())?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .map_err(|e| e.to_string())?;
    let mut response = String::new();
    let _ = stream.read_to_string(&mut response);
    Ok(())
}
