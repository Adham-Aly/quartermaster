//! Step 2 of a turn: the input the model reads.
//!
//! Pure. The blocks, in order: the session preamble (a new session's summary, taken once), the
//! fresh context block, the restart note when the app restarted mid-turn, every wake in one block,
//! and your message last, unmarked and authoritative.

use qm_engine::{InputBlock, TurnInput};
use qm_policy::markers::neutralise;
use qm_store::wakes::ClaimedWake;
use qm_types::domain::WakeKind;

/// What the retry of a turn that did nothing tells the model.
pub const RETRY_TEXT: &str = "<qm:app event=\"no_action\">Your last turn ended without showing the user anything or taking any action. Answer the user's message now with reply, ask_user or acknowledge. Tools that change work or settings are refused in this turn.</qm:app>";

/// A turn's input.
pub fn assemble(
    preamble: Option<&str>,
    context: &str,
    restart: Option<&str>,
    wakes: &[ClaimedWake],
    message: Option<&str>,
) -> TurnInput {
    let mut blocks = Vec::with_capacity(5);
    let mut push = |text: String| {
        if !text.trim().is_empty() {
            blocks.push(InputBlock::Text { text });
        }
    };
    if let Some(p) = preamble {
        push(p.to_owned());
    }
    push(context.to_owned());
    if let Some(r) = restart {
        push(r.to_owned());
    }
    if !wakes.is_empty() {
        let rendered: Vec<String> = wakes.iter().map(render_wake).collect();
        push(rendered.join("\n"));
    }
    if let Some(m) = message {
        // Stored neutralised already; neutralising again changes nothing and keeps a marker out
        // even if a row were written another way.
        push(neutralise(m));
    }
    TurnInput { blocks }
}

/// One wake as the model reads it: `<qm:app event="checks" task="C-0042">…</qm:app>`.
///
/// The payload convention every wake producer follows: `"text"` is what the agent reads, `"short"`
/// the chat header's words. A payload without `"text"` renders as compact JSON.
pub fn render_wake(wake: &ClaimedWake) -> String {
    let text = match wake.payload.get("text").and_then(serde_json::Value::as_str) {
        Some(t) => t.to_owned(),
        None => serde_json::to_string(&wake.payload).unwrap_or_default(),
    };
    // A request routed into a secondmate's scope, marked as from its supervisor. The correlation ID
    // is the app's own.
    if wake.kind == WakeKind::Routed {
        let corr = wake
            .payload
            .get("correlation")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        return format!(
            "<qm:supervisor from=\"quartermaster\" corr=\"{corr}\">{}</qm:supervisor>",
            neutralise(&text)
        );
    }
    let task = wake
        .task
        .map(|t| format!(" task=\"{t}\""))
        .unwrap_or_default();
    format!(
        "<qm:app event=\"{}\"{task}>{}</qm:app>",
        wake.kind.as_str(),
        neutralise(&text)
    )
}

/// The retry's input: one block.
pub fn retry_input() -> TurnInput {
    TurnInput::text(RETRY_TEXT)
}

/// The note the turn after a restart carries, before the wakes it takes; `at` is a local `HH:MM`.
pub fn restart_note(at: &str) -> String {
    format!(
        "<qm:app event=\"restarted\">The app restarted at {at}; your last turn was cut short.</qm:app>"
    )
}
