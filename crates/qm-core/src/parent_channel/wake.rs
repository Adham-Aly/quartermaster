//! The Quartermaster's wake for each parent-channel row. Pure.
//!
//! - fact row: `SM-02 · C-0193: T-0045 (build) PR #231 is ready at 9c0d2e4.`, short
//!   `SM-02 · T-0045 pr_ready`;
//! - judgment row: `SM-02 on C-0193 (add a dark mode toggle to settings): <summary>. Recommends:
//!   <…>. Needs the user: <…>. Tasks: T-0045 build ci_passed PR #231.`, short
//!   `SM-02 reported C-0193`.
//!
//! Every payload is `{"text", "short", "correlation"}`; text is cut at [`WAKE_TEXT_CHARS`].

use super::facts::{Facts, Outcome, OutcomeType};
use crate::tasks::cut_chars;
use qm_types::ids::{CorrelationId, SecondmateId};

/// The longest wake text, in characters, before the cut note.
pub const WAKE_TEXT_CHARS: usize = 2_000;
/// What a cut wake text ends with.
pub const CUT_NOTE: &str = "… read it all with task_status";
/// How much of the request a judgment wake quotes.
pub const REQUEST_QUOTE_CHARS: usize = 80;

/// A wake's text, short header and correlation, as one payload.
pub fn payload(text: &str, short: &str, corr: CorrelationId) -> serde_json::Value {
    serde_json::json!({
        "text": cut_chars(text, WAKE_TEXT_CHARS, CUT_NOTE),
        "short": short,
        "correlation": corr,
    })
}

/// One fact as a sentence: `T-0045 (build) PR #231 is ready at 9c0d2e4.`
pub fn fact_sentence(o: &Outcome) -> String {
    let who = format!("{} ({})", o.task, o.task_kind);
    let at = o
        .sha
        .as_deref()
        .map(|s| format!(" at {s}"))
        .unwrap_or_default();
    let pr =
        o.pr.map_or_else(|| "its pull request".to_owned(), |n| format!("PR #{n}"));
    match o.kind {
        OutcomeType::Started => format!("{who} started."),
        OutcomeType::Reported => match o.detail.as_deref() {
            Some(state) => format!("{who} reported {}.", state.replace('_', " ")),
            None => format!("{who} reported."),
        },
        OutcomeType::Held => format!("{who} is held for approval."),
        OutcomeType::Stuck => format!("{who} is stuck."),
        OutcomeType::Landed => format!("{who} landed."),
        OutcomeType::Closed => match o.detail.as_deref() {
            Some(outcome) => format!("{who} closed ({outcome})."),
            None => format!("{who} closed."),
        },
        OutcomeType::Failed => format!("{who} failed."),
        OutcomeType::PrReady => format!("{who} {pr} is ready{at}."),
        OutcomeType::CiPassed => format!("{who} checks passed{at}."),
        OutcomeType::CiFailed => format!("{who} checks failed{at}."),
        OutcomeType::Merged => format!("{who} {pr} merged{at}."),
    }
}

/// A fact row's wake: `(text, short)`.
pub fn fact_wake(secondmate: SecondmateId, corr: CorrelationId, o: &Outcome) -> (String, String) {
    (
        format!("{secondmate} · {corr}: {}", fact_sentence(o)),
        format!("{secondmate} · {} {}", o.task, o.kind.as_str()),
    )
}

/// The snapshot's tasks on one line: `T-0045 build ci_passed PR #231, T-0046 fix running`.
pub fn tasks_line(facts: &Facts) -> String {
    if facts.tasks.is_empty() {
        return "none yet".to_owned();
    }
    facts
        .tasks
        .iter()
        .map(|t| {
            let pr = t.pr.map(|n| format!(" PR #{n}")).unwrap_or_default();
            format!("{} {} {}{pr}", t.id, t.kind, t.state)
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn sentence(text: &str) -> String {
    let t = text.trim();
    if t.ends_with(['.', '!', '?', '…']) {
        t.to_owned()
    } else {
        format!("{t}.")
    }
}

/// A judgment row's wake: `(text, short)`.
pub fn judgment_wake(
    secondmate: SecondmateId,
    corr: CorrelationId,
    request: &str,
    summary: &str,
    recommendation: Option<&str>,
    needs_user: Option<&str>,
    facts: &Facts,
) -> (String, String) {
    let request = request.split_whitespace().collect::<Vec<_>>().join(" ");
    let request = cut_chars(&request, REQUEST_QUOTE_CHARS.saturating_sub(1), "…");
    let text = format!(
        "{secondmate} on {corr} ({request}): {} Recommends: {} Needs the user: {} Tasks: {}.",
        sentence(summary),
        recommendation.map_or_else(|| "—.".to_owned(), sentence),
        needs_user.map_or_else(|| "—.".to_owned(), sentence),
        tasks_line(facts),
    );
    (text, format!("{secondmate} reported {corr}"))
}
