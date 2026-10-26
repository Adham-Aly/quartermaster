//! Which of the user's messages a brief or a routed request carries, and what is left out of it.
//!
//! By default it is the message that started the orchestrator's turn. The orchestrator may instead
//! name an earlier message (`words_message`): the app checks it is an actual user message in that
//! orchestrator's own conversation. It may also name whole passages to leave out (`words_cut`),
//! which `qm_policy::user_words` checks and applies as a pure deletion. The passages are kept with
//! the trimmed words, so what was left out stays on record.

use qm_store::StoreError;
use qm_types::error::Refusal;
use qm_types::ids::{AgentId, MessageId};
use rusqlite::Connection;

/// The words chosen for a brief or a request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct ChosenWords {
    pub text: String,
    /// The message they came from.
    pub message: Option<MessageId>,
    /// The passages left out of it; empty when `text` is the message as it was.
    pub cuts: Vec<String>,
}

/// The words of `chosen` (checked as `agent`'s own user message) or else of `turn_message`, with
/// `cuts` left out. `None` when there is no message at all and nothing to cut, so the caller keeps
/// its own fallback (words inherited from earlier work, or none).
pub(crate) fn choose(
    conn: &Connection,
    agent: AgentId,
    chosen: Option<MessageId>,
    turn_message: Option<MessageId>,
    cuts: &[String],
) -> Result<Result<Option<ChosenWords>, Refusal>, StoreError> {
    let (text, message) = if let Some(message) = chosen {
        match qm_store::user_message_sources::words_of(conn, agent, message)? {
            Ok(text) => (text, message),
            Err(r) => return Ok(Err(r)),
        }
    } else if let Some(message) = turn_message {
        (
            qm_store::messages::text(conn, message)?.unwrap_or_default(),
            message,
        )
    } else if cuts.is_empty() {
        return Ok(Ok(None));
    } else {
        return Ok(Err(Refusal::new(
            "words_cut needs the user's message on this turn; name the message with words_message",
        )));
    };
    if cuts.is_empty() {
        return Ok(Ok(Some(ChosenWords {
            text,
            message: Some(message),
            cuts: Vec::new(),
        })));
    }
    Ok(qm_policy::user_words::trim(&text, cuts).map(|trimmed| {
        Some(ChosenWords {
            text: trimmed,
            message: Some(message),
            cuts: cuts.iter().map(|c| c.trim().to_owned()).collect(),
        })
    }))
}
