//! What can go wrong talking to GitHub, as sentences an agent (or you) can act on.
//!
//! No error ever carries the token: the client never formats a request's headers, and the token
//! source's errors name the `gh` command, never its output.

use jiff::Timestamp;

/// A GitHub failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GithubError {
    /// The app can't talk to GitHub at all on this Mac (`gh` missing or logged out).
    #[error("{0}")]
    Unavailable(String),
    /// GitHub refused the app's login even after asking `gh` again.
    #[error("GitHub refused the app's login (401); the setup check can fix that")]
    Auth,
    /// GitHub has no such thing (or the login can't see it).
    #[error("GitHub returned 404 for {0}")]
    NotFound(String),
    /// The rate limit is used up.
    #[error("GitHub's rate limit is used up{}", reset_suffix(.reset_at.as_ref()))]
    RateLimited { reset_at: Option<Timestamp> },
    /// Any other answer GitHub gave that isn't success.
    #[error("GitHub returned {status}: {message}")]
    Http { status: u16, message: String },
    /// GitHub couldn't be reached, or didn't answer in time.
    #[error("couldn't reach GitHub: {0}")]
    Network(String),
    /// GitHub's answer couldn't be read.
    #[error("couldn't read GitHub's answer: {0}")]
    Decode(String),
}

fn reset_suffix(at: Option<&Timestamp>) -> String {
    at.map(|t| format!(" until {t}")).unwrap_or_default()
}

impl GithubError {
    /// One plain sentence for the agent ("GitHub returned 404 for api-server's pull request
    /// #212").
    pub fn plain(&self) -> String {
        self.to_string()
    }

    /// Whether the call may simply be tried again later (the network, GitHub's own trouble, the
    /// rate limit), as opposed to an answer that won't change by waiting.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Network(_) | Self::RateLimited { .. } | Self::Unavailable(_) => true,
            Self::Http { status, .. } => *status >= 500,
            Self::Auth | Self::NotFound(_) | Self::Decode(_) => false,
        }
    }
}

/// GitHub's rate limit as the last response reported it (`x-ratelimit-remaining`,
/// `x-ratelimit-reset`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RateState {
    pub remaining: Option<u32>,
    pub reset_at: Option<Timestamp>,
}
