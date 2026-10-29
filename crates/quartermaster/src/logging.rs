//! Logging to `logs/` with secret redaction.
//!
//! One file per local day, `logs/quartermaster-YYYY-MM-DD.log`, mode 0600, appended to. A server
//! that runs past local midnight moves on to the new day's file with the first line written after
//! it, so no file holds more than one day. The day comes from the injected [`Clock`]. Every line
//! goes through [`redact`] before it reaches the disk, as a backstop: code should never log a
//! secret in the first place. The level comes from `QM_LOG` (an `EnvFilter` directive), `info` by
//! default. Nothing is logged to the terminal, which belongs to the UI.
//!
//! Seven days of files are kept: today's and the six before it. Older ones are removed when the
//! server starts and each time the day changes.

use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use qm_types::clock::Clock;
use std::borrow::Cow;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

/// The environment variable holding the log filter.
pub const QM_LOG: &str = "QM_LOG";

const REDACTED: &str = "[redacted]";

/// How many days of log files are kept, today's included.
const KEEP_DAYS: i64 = 7;

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

/// The path of the log file for `date`, a local calendar day.
pub fn log_file(logs: &Path, date: Date) -> PathBuf {
    logs.join(format!("quartermaster-{date}.log"))
}

/// The local calendar day `at` falls on.
fn local_day(at: Timestamp) -> Date {
    at.to_zoned(TimeZone::system()).date()
}

/// Removes the day's log files older than [`KEEP_DAYS`] days, counting today. A file name this
/// module doesn't write is left alone; a file that can't be removed is left for the next day.
fn prune_old(logs: &Path, today: Date) {
    let Ok(oldest) = today.checked_sub(jiff::Span::new().days(KEEP_DAYS - 1)) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(logs) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let day = name
            .to_str()
            .and_then(|n| n.strip_prefix("quartermaster-"))
            .and_then(|n| n.strip_suffix(".log"))
            .and_then(|d| d.parse::<Date>().ok());
        if day.is_some_and(|day| day < oldest) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn open_log(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

/// Installs the global subscriber, writing redacted lines to the day's file in `logs`. Lines
/// written after local midnight go to the next day's file; see the module docs.
pub fn init(logs: &Path, clock: Arc<dyn Clock>) -> io::Result<()> {
    let log = DailyFile::open(logs.to_path_buf(), clock)?;
    let filter = EnvFilter::try_from_env(QM_LOG).unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(RedactingFile(Arc::new(log)))
        .try_init()
        .map_err(io::Error::other)
}

/// The day's log file, reopened when the clock's local day moves on.
#[derive(Debug)]
struct DailyFile {
    logs: PathBuf,
    clock: Arc<dyn Clock>,
    day: Mutex<Day>,
}

/// The file being written and the day it is for.
#[derive(Debug)]
struct Day {
    date: Date,
    file: File,
}

impl DailyFile {
    fn open(logs: PathBuf, clock: Arc<dyn Clock>) -> io::Result<Self> {
        let date = local_day(clock.now());
        prune_old(&logs, date);
        let file = open_log(&log_file(&logs, date))?;
        Ok(Self {
            logs,
            clock,
            day: Mutex::new(Day { date, file }),
        })
    }

    /// Writes `bytes` to the file for today, opening it first if the day has changed.
    fn write_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        let today = local_day(self.clock.now());
        let mut day = self.day.lock().unwrap_or_else(PoisonError::into_inner);
        if day.date != today {
            prune_old(&self.logs, today);
            day.file = open_log(&log_file(&self.logs, today))?;
            day.date = today;
        }
        day.file.write_all(bytes)
    }
}

/// A log file that redacts each event before writing it.
#[derive(Clone, Debug)]
struct RedactingFile(Arc<DailyFile>);

impl<'a> MakeWriter<'a> for RedactingFile {
    type Writer = EventBuffer;

    fn make_writer(&'a self) -> EventBuffer {
        EventBuffer {
            log: Arc::clone(&self.0),
            buf: Vec::new(),
        }
    }
}

/// Collects one event, then redacts and writes it in one piece when dropped.
#[derive(Debug)]
struct EventBuffer {
    log: Arc<DailyFile>,
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
        self.log.write_bytes(clean.as_bytes())?;
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
