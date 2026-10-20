//! A crewmate turn's input and its markers. Pure.
//!
//! Blocks, in order, each text: the first block if any (the brief, or a reopened session's note
//! and the brief); one block with every claimed wake rendered; your message last, unmarked,
//! exactly as stored (neutralised when stored).

use qm_engine::{InputBlock, TurnInput};
use qm_policy::markers::neutralise;
use qm_store::wakes::ClaimedWake;
use qm_types::domain::WakeKind;

/// What comes first in a turn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum FirstBlock {
    /// The brief, exactly as saved: the first turn.
    Brief(String),
    /// A new session after the old one was lost: why, then the brief.
    Reopened {
        why: String,
        brief: String,
    },
    None,
}

/// A turn's input.
pub(crate) fn assemble(
    first: &FirstBlock,
    wakes: &[ClaimedWake],
    message: Option<&str>,
    supervisor: &str,
) -> TurnInput {
    let mut blocks = Vec::with_capacity(3);
    let mut push = |text: String| {
        if !text.trim().is_empty() {
            blocks.push(InputBlock::Text { text });
        }
    };
    match first {
        FirstBlock::Brief(brief) => push(brief.clone()),
        FirstBlock::Reopened { why, brief } => {
            push(reopened_note(why));
            push(brief.clone());
        }
        FirstBlock::None => {}
    }
    if !wakes.is_empty() {
        push(render_wakes(wakes, supervisor));
    }
    if let Some(m) = message {
        push(neutralise(m));
    }
    TurnInput { blocks }
}

/// Every wake in one block, oldest first.
pub(crate) fn render_wakes(wakes: &[ClaimedWake], supervisor: &str) -> String {
    wakes
        .iter()
        .map(|w| render_wake(w, supervisor))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The note a reopened session starts with.
pub(crate) fn reopened_note(why: &str) -> String {
    format!(
        "<qm:app event=\"session_reopened\">{} Your work so far is on your branch and in your worktree.</qm:app>",
        neutralise(why.trim())
    )
}

/// One wake as the crewmate reads it. A `steer` is the supervisor's own words in a supervisor
/// marker; everything else is an app marker naming its kind.
pub(crate) fn render_wake(wake: &ClaimedWake, supervisor: &str) -> String {
    let text = match wake.payload.get("text").and_then(serde_json::Value::as_str) {
        Some(t) => t.to_owned(),
        None => serde_json::to_string(&wake.payload).unwrap_or_default(),
    };
    let text = neutralise(&text);
    if wake.kind == WakeKind::Steer {
        let task = wake
            .task
            .map(|t| format!(" task=\"{t}\""))
            .unwrap_or_default();
        return format!("<qm:supervisor from=\"{supervisor}\"{task}>{text}</qm:supervisor>");
    }
    let attrs = match wake.kind {
        WakeKind::JobExited => wake
            .payload
            .get("job")
            .and_then(serde_json::Value::as_str)
            .map(|j| format!(" job=\"{j}\""))
            .unwrap_or_default(),
        WakeKind::WaitOver | WakeKind::BlockedBy => wake
            .payload
            .get("other")
            .and_then(serde_json::Value::as_str)
            .map(|t| format!(" task=\"{t}\""))
            .unwrap_or_default(),
        _ => String::new(),
    };
    format!(
        "<qm:app event=\"{}\"{attrs}>{text}</qm:app>",
        wake.kind.as_str()
    )
}
