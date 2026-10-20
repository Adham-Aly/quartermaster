//! The app's GitHub token, from `gh auth token`.
//!
//! The token lives only in memory: [`GithubToken`]'s `Debug` prints `GithubToken(<redacted>)`, it
//! has no `Display` and no `Serialize`, and nothing here logs it or puts it in an error. It is read
//! in the app's own environment and never handed to an agent.

use crate::GithubError;
use qm_types::BoxFuture;
use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How long `gh auth token` may take.
pub const GH_TIMEOUT: Duration = Duration::from_secs(10);
/// How long a token read from `gh` is reused before asking again.
pub const TOKEN_CACHE: Duration = Duration::from_secs(600);

/// What the app says when `gh` isn't installed (the same sentence as the local delivery probe).
pub const GH_MISSING: &str = "GitHub CLI isn't installed on this Mac; the setup check can fix that";
/// What the app says when `gh` isn't logged in.
pub const GH_LOGGED_OUT: &str =
    "GitHub CLI isn't logged in on this Mac; the setup check can fix that";

/// A GitHub token. Never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct GithubToken(String);

impl GithubToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    /// The token itself, for the `Authorization` header only.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for GithubToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GithubToken(<redacted>)")
    }
}

/// Where the client gets its token.
pub trait TokenSource: Send + Sync + fmt::Debug {
    fn token(&self) -> BoxFuture<'_, Result<GithubToken, GithubError>>;
    /// After a 401: the next call asks again.
    fn invalidate(&self);
}

/// Where `gh` is, read each time it runs: the setup check can find it, or install it, after
/// the app started, and the next use then takes the new one without a restart.
pub type GhProgram = std::sync::Arc<dyn Fn() -> PathBuf + Send + Sync>;

/// A [`GhProgram`] that is always `program`.
pub fn fixed_gh(program: impl Into<PathBuf>) -> GhProgram {
    let program = program.into();
    std::sync::Arc::new(move || program.clone())
}

/// `gh auth token --hostname github.com`, in the app's own environment, cached for
/// [`TOKEN_CACHE`].
pub struct GhCli {
    program: GhProgram,
    cache: Mutex<Option<(GithubToken, Instant)>>,
}

impl fmt::Debug for GhCli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GhCli")
            .field("program", &self.program())
            .finish_non_exhaustive()
    }
}

impl GhCli {
    /// Always this `gh`.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self::following(fixed_gh(program))
    }

    /// The `gh` that `program` names at each use (the setup check's, else `gh` on `PATH`).
    pub fn following(program: GhProgram) -> Self {
        Self {
            program,
            cache: Mutex::new(None),
        }
    }

    /// The `gh` it runs now.
    pub fn program(&self) -> PathBuf {
        (self.program)()
    }

    fn cached(&self) -> Option<GithubToken> {
        let guard = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        guard
            .as_ref()
            .filter(|(_, at)| at.elapsed() < TOKEN_CACHE)
            .map(|(t, _)| t.clone())
    }

    async fn ask(&self) -> Result<GithubToken, GithubError> {
        let mut command = tokio::process::Command::new(self.program());
        command
            .args(["auth", "token", "--hostname", "github.com"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let child = match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(GithubError::Unavailable(GH_MISSING.to_owned()));
            }
            Err(e) => {
                return Err(GithubError::Unavailable(format!(
                    "GitHub CLI couldn't be started on this Mac: {e}"
                )));
            }
        };
        let output = match tokio::time::timeout(GH_TIMEOUT, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                return Err(GithubError::Unavailable(format!(
                    "GitHub CLI failed on this Mac: {e}"
                )));
            }
            Err(_) => {
                return Err(GithubError::Unavailable(
                    "GitHub CLI didn't answer within 10 seconds".to_owned(),
                ));
            }
        };
        let token = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        if !output.status.success() || token.is_empty() {
            return Err(GithubError::Unavailable(GH_LOGGED_OUT.to_owned()));
        }
        Ok(GithubToken(token))
    }
}

impl TokenSource for GhCli {
    fn token(&self) -> BoxFuture<'_, Result<GithubToken, GithubError>> {
        Box::pin(async move {
            if let Some(token) = self.cached() {
                return Ok(token);
            }
            let token = self.ask().await?;
            *self.cache.lock().unwrap_or_else(PoisonError::into_inner) =
                Some((token.clone(), Instant::now()));
            Ok(token)
        })
    }

    fn invalidate(&self) {
        *self.cache.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}
