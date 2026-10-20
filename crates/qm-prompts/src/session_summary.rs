//! The summary a new Quartermaster session opens with.
//!
//! When the seat can't resume its session (the engine or account changed, the resume failed, the
//! session closed), the new session's first turn starts with the last few turns of the
//! conversation, so the Quartermaster picks up where it left off. The summary is written by code
//! from the stored conversation, not by a model call: it costs nothing and can't fail.
//!
//! ```text
//! <qm:app event="session_summary">
//! This is a new session (the Quartermaster moved to another engine). The last few turns, oldest first:
//! 14:10 user: how's 42 going?
//! 14:10 you replied: T-0042 has the fix in and is re-running the auth suite before it opens the PR.
//! 14:12 user: tell 42 a unit test that reproduces the race is enough, skip the full e2e run
//! 14:12 app noted: Steered: T-0042
//! </qm:app>
//! ```
//!
//! The lines reach this module already neutralised by the core.

use jiff::Zoned;
use std::fmt::Write as _;

/// How many of the last turns the summary covers.
pub const SUMMARY_TURNS: usize = 6;
/// Each line is cut to this many characters.
pub const SUMMARY_LINE_CHARS: usize = 300;
/// The lines together are cut, from the oldest end, to this many characters.
pub const SUMMARY_CHARS: usize = 2_000;

/// Why the session is new: `Startup` without a resumable session.
pub const REASON_STARTUP: &str = "the app started a new session";
/// The engine couldn't resume the stored session.
pub const REASON_RESUME_FAILED: &str = "the previous session couldn't be resumed";
/// The previous session closed.
pub const REASON_CLOSED: &str = "the previous session closed";
/// The seat's model moved to another engine or account.
pub const REASON_ENGINE_SWITCH: &str = "the Quartermaster moved to another engine";

/// Who said a line, as the new session reads it (the Quartermaster is "you").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    /// Your message: "user:".
    You,
    /// The Quartermaster: "you replied:", "you asked:", "you acknowledged:".
    Quartermaster(Said),
    /// The app's acknowledgement: "app noted:".
    App,
}

/// How the Quartermaster said it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Said {
    Replied,
    Asked,
    Acknowledged,
}

/// One line of the conversation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SummaryLine {
    pub at: Zoned,
    pub who: Who,
    pub text: String,
}

/// The summary block, ending in a newline. `lines` are oldest first.
pub fn render(reason: &str, lines: &[SummaryLine]) -> String {
    let rendered: Vec<String> = lines.iter().map(line).collect();
    // Keep the newest lines that fit.
    let mut kept: Vec<&String> = Vec::new();
    let mut total = 0;
    for l in rendered.iter().rev() {
        let n = l.chars().count() + 1;
        if total + n > SUMMARY_CHARS {
            break;
        }
        total += n;
        kept.push(l);
    }
    kept.reverse();

    let mut out = String::from("<qm:app event=\"session_summary\">\n");
    if kept.is_empty() {
        let _ = writeln!(
            out,
            "This is a new session ({reason}). Nothing was said in the last few turns."
        );
    } else {
        let _ = writeln!(
            out,
            "This is a new session ({reason}). The last few turns, oldest first:"
        );
        for l in kept {
            out.push_str(l);
            out.push('\n');
        }
    }
    out.push_str("</qm:app>\n");
    out
}

fn line(l: &SummaryLine) -> String {
    let who = match l.who {
        Who::You => "user:",
        Who::Quartermaster(Said::Replied) => "you replied:",
        Who::Quartermaster(Said::Asked) => "you asked:",
        Who::Quartermaster(Said::Acknowledged) => "you acknowledged:",
        Who::App => "app noted:",
    };
    format!(
        "{} {who} {}",
        l.at.strftime("%H:%M"),
        crate::context_block::cut(&l.text, SUMMARY_LINE_CHARS)
    )
}
