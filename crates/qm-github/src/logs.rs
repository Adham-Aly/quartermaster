//! Log excerpts for a failing check. The crate returns raw text; the core neutralises it before
//! any agent sees it.

use crate::checks::{CheckRun, CommitStatus};
use crate::client::{GithubClient, Method};
use crate::error::GithubError;
use crate::repo::RepoRef;

/// The last lines of a log kept in an excerpt.
pub const EXCERPT_LINES: usize = 40;
/// The longest excerpt, in characters.
pub const EXCERPT_CHARS: usize = 3_000;
/// The most of a job log read, in bytes.
pub const LOG_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Strips a GitHub Actions timestamp (`2026-01-01T12:00:00.1234567Z `) from the start of a line.
fn strip_timestamp(line: &str) -> &str {
    let b = line.as_bytes();
    let looks = b.len() > 20
        && b[4] == b'-'
        && b[7] == b'-'
        && b[10] == b'T'
        && b[..4].iter().all(u8::is_ascii_digit);
    if looks
        && let Some((stamp, rest)) = line.split_once(' ')
        && stamp.ends_with('Z')
    {
        return rest;
    }
    line
}

/// Cuts to the last `max` characters, starting with "…" when cut.
fn tail_chars(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_owned();
    }
    let skip = n - max + 1;
    let mut out = String::from("…");
    out.extend(text.chars().skip(skip));
    out
}

/// A job log's excerpt: every `##[error]` line plus the last [`EXCERPT_LINES`] lines, at most
/// [`EXCERPT_CHARS`] characters.
pub fn excerpt_from_log(log: &str) -> String {
    let lines: Vec<&str> = log.lines().map(strip_timestamp).collect();
    let tail_from = lines.len().saturating_sub(EXCERPT_LINES);
    let mut kept: Vec<&str> = lines[..tail_from]
        .iter()
        .copied()
        .filter(|l| l.contains("##[error]"))
        .collect();
    kept.extend_from_slice(&lines[tail_from..]);
    tail_chars(&kept.join("\n"), EXCERPT_CHARS)
}

/// Another app's output, cut the same way.
pub fn excerpt_from_output(output: &str) -> String {
    let lines: Vec<&str> = output.lines().collect();
    let from = lines.len().saturating_sub(EXCERPT_LINES);
    tail_chars(&lines[from..].join("\n"), EXCERPT_CHARS)
}

/// A commit status's excerpt: its description and target URL only.
pub fn status_excerpt(status: &CommitStatus) -> Option<String> {
    let text = [status.description.clone(), status.target_url.clone()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then(|| tail_chars(&text, EXCERPT_CHARS))
}

impl GithubClient {
    /// The excerpt for a failing check run: the job's log for GitHub Actions, the run's output for
    /// another app. `None` when there's nothing to show.
    pub async fn log_excerpt(
        &self,
        r: &RepoRef,
        run: &CheckRun,
    ) -> Result<Option<String>, GithubError> {
        if let Some(job) = run.actions_job {
            let response = self
                .send(
                    Method::Get,
                    &format!("{}/actions/jobs/{job}/logs", r.path()),
                    None,
                )
                .await?;
            if response.ok() {
                let mut log = response.body;
                if log.len() > LOG_MAX_BYTES {
                    let mut cut = log.len() - LOG_MAX_BYTES;
                    while !log.is_char_boundary(cut) {
                        cut += 1;
                    }
                    log = log.split_off(cut);
                }
                return Ok(Some(excerpt_from_log(&log)));
            }
            if response.status != 404 && response.status != 410 {
                return Err(response.error(&format!("the log of check {}", run.name), self.rate()));
            }
        }
        Ok(run.output.as_deref().map(excerpt_from_output))
    }
}
