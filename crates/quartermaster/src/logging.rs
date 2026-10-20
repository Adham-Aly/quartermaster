//! Logging to `logs/` with secret redaction.
//!
//! One file per day, `logs/quartermaster-YYYY-MM-DD.log`, mode 0600, appended to. Every line goes
//! through [`redact`] before it reaches the disk, as a backstop: code should never log a secret in
//! the first place. The level comes from `QM_LOG` (an `EnvFilter` directive), `info` by default.
//! Nothing is logged to the terminal, which belongs to the UI.

use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// The environment variable holding the log filter.
pub const QM_LOG: &str = "QM_LOG";

const REDACTED: &str = "[redacted]";

/// Token prefixes that mark a credential: everything after the prefix is hidden.
const SECRET_PREFIXES: &[&str] = &[
    "sk-ant-",
    "sk-proj-",
    "sk-or-",
    "sk-",
    "github_pat_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "xoxb-",
    "xoxp-",
    "AIza",
    "eyJ",
];

/// Key names whose value is hidden in `key=value`, `key: value` and `"key":"value"` forms, and the
/// scheme words whose following token is hidden (`Bearer abc`).
const SECRET_KEYS: &[&str] = &[
    "token",
    "secret",
    "password",
    "passwd",
    "api_key",
    "apikey",
    "api-key",
    "authorization",
    "cookie",
    "credential",
];
const SECRET_SCHEMES: &[&str] = &["bearer", "basic"];

/// The path of today's log file.
pub fn log_file(logs: &Path) -> PathBuf {
    let date = jiff::Zoned::now().date();
    logs.join(format!("quartermaster-{date}.log"))
}

/// Installs the global subscriber, writing redacted lines to today's file in `logs`.
pub fn init(logs: &Path) -> io::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(log_file(logs))?;
    let filter = EnvFilter::try_from_env(QM_LOG).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(RedactingFile(Arc::new(Mutex::new(file))))
        .try_init()
        .map_err(io::Error::other)
}

/// A log file that redacts each event before writing it.
#[derive(Clone, Debug)]
struct RedactingFile(Arc<Mutex<File>>);

impl<'a> MakeWriter<'a> for RedactingFile {
    type Writer = EventBuffer;

    fn make_writer(&'a self) -> EventBuffer {
        EventBuffer {
            file: Arc::clone(&self.0),
            buf: Vec::new(),
        }
    }
}

/// Collects one event, then redacts and writes it in one piece when dropped.
#[derive(Debug)]
struct EventBuffer {
    file: Arc<Mutex<File>>,
    buf: Vec<u8>,
}

impl Write for EventBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let text = String::from_utf8_lossy(&self.buf);
        let clean = redact(&text);
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        file.write_all(clean.as_bytes())?;
        self.buf.clear();
        Ok(())
    }
}

impl Drop for EventBuffer {
    fn drop(&mut self) {
        let _ = self.flush();
    }
}

fn is_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/' | '+' | '=' | '~')
}

/// Hides anything that looks like a credential in `text`.
pub fn redact(text: &str) -> Cow<'_, str> {
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    let mut rest = text;
    // Set when the previous word was a secret key or scheme: the next token is hidden.
    let mut hide_next = false;
    while let Some(c) = rest.chars().next() {
        if !is_token_char(c) {
            out.push(c);
            rest = &rest[c.len_utf8()..];
            continue;
        }
        let end = rest.find(|c: char| !is_token_char(c)).unwrap_or(rest.len());
        let word = &rest[..end];
        rest = &rest[end..];
        if hide_next {
            hide_next = false;
            out.push_str(REDACTED);
            changed = true;
            continue;
        }
        // `key=value` within one token.
        if let Some((key, value)) = word.split_once('=')
            && !value.is_empty()
            && is_secret_key(key)
        {
            out.push_str(key);
            out.push('=');
            out.push_str(REDACTED);
            changed = true;
            continue;
        }
        if let Some(prefix) = SECRET_PREFIXES
            .iter()
            .find(|p| word.starts_with(**p) && word.len() >= p.len() + 8)
        {
            out.push_str(prefix);
            out.push_str(REDACTED);
            changed = true;
            continue;
        }
        out.push_str(word);
        let lower = word.to_ascii_lowercase();
        if SECRET_SCHEMES.contains(&lower.as_str()) {
            hide_next = rest.starts_with(' ');
        } else if is_secret_key(word.trim_end_matches('=')) {
            // `key: value`, `key = value`, `"key":"value"`, `key=` followed by a quoted value.
            let sep = rest.trim_start_matches([' ', '"', '\'']);
            if let Some(after) = sep.strip_prefix([':', '='])
                && after
                    .trim_start_matches([' ', '"', '\''])
                    .starts_with(is_token_char)
            {
                hide_next = true;
            } else if word.ends_with('=') {
                hide_next = rest.starts_with(['"', '\'']);
            }
        }
    }
    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(text)
    }
}

fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|k| key.ends_with(k))
}
