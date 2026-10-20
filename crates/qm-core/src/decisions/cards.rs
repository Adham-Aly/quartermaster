//! The decision card and the stuck notice.
//!
//! ```text
//! ⚑ Decision · T-0047 fix(ci): flaky upload test · api-server · high · opus-5-5
//! Held by the app: 3 repair attempts used. The upload test still fails after each fix …   (dim)
//! Supervisor: the timeout moved from 30s to 31s after the last change, …                  (text)
//!
//!   1 · Retry with my guidance  2 · Try a premium model  3 · Discard the work
//! ```
//!
//! The stored title is `qm_policy::holds::card_title` ("Decision · T-0047 fix(ci): flaky upload
//! test · api-server · high · opus-5-5", as the stuck notice stores it too); [`split_title`] takes
//! it apart again into the card's title and its dim note. [`face`] is what the performer gives
//! `HeldService` (the lines); [`decision_card`] builds the whole card.

use qm_policy::holds::reason_line;
use qm_store::held::{DecisionPayload, HeldPayload, HeldRow, HoldReason};
use qm_types::api::{
    Card, CardAction, CardButton, CardKind, CardLine, HeldAnswer, HeldChoice, Styled, Tone,
};
use qm_types::domain::HeldStatus;

use crate::held::{HeldCard, HeldCardBuilder};

/// The prefix `card_title` writes, which the card's heading already shows.
const TITLE_PREFIX: &str = "Decision · ";
/// The separator between the title and the project, tier and model.
const SEP: &str = " · ";

/// Takes a stored decision title apart: ("T-0047 fix(ci): flaky upload test", Some("· api-server
/// · high · opus-5-5")). A title not written by `card_title` stays whole, with no note.
pub fn split_title(stored: &str) -> (String, Option<String>) {
    let Some(rest) = stored.strip_prefix(TITLE_PREFIX) else {
        return (stored.to_owned(), None);
    };
    let parts: Vec<&str> = rest.rsplitn(4, SEP).collect();
    if let [model, tier, project, title] = parts.as_slice() {
        let note = if project.is_empty() {
            format!("· {tier} · {model}")
        } else {
            format!("· {project} · {tier} · {model}")
        };
        (title.to_string(), Some(note))
    } else {
        (rest.to_owned(), None)
    }
}

/// The decision's payload, if `row` is a decision.
fn decision(row: &HeldRow) -> Option<&DecisionPayload> {
    match &row.payload {
        HeldPayload::Decision(d) => Some(d),
        _ => None,
    }
}

/// The card's body: the reason line with the app's detail (dim), the supervisor's summary when
/// present (text), and an empty line before the buttons.
pub fn lines(payload: &DecisionPayload) -> Vec<CardLine> {
    let reason = reason_line(payload.reason);
    // The stuck notice's reason line already says what its detail would.
    let detail = if payload.reason == HoldReason::Stuck {
        ""
    } else {
        payload.detail.trim()
    };
    let first = if detail.is_empty() {
        reason
    } else if detail.ends_with('.') {
        format!("{reason} {detail}")
    } else {
        format!("{reason} {detail}.")
    };
    let mut out = vec![CardLine::new(0, vec![Styled::new(first, Tone::Dim)])];
    if let Some(summary) = payload
        .summary
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out.push(CardLine::new(
            0,
            vec![Styled::new(format!("Supervisor: {summary}"), Tone::Text)],
        ));
    }
    out.push(CardLine::new(0, Vec::new()));
    out
}

/// What the performer gives `HeldService` for a decision row: the dim note and the lines (the
/// heading, title and numbered buttons are `HeldService`'s).
pub fn face(row: &HeldRow) -> HeldCard {
    let Some(payload) = decision(row) else {
        return HeldCard::default();
    };
    let (_, note) = split_title(&row.title);
    HeldCard {
        note,
        lines: lines(payload),
        buttons: Vec::new(),
    }
}

/// Whether the row is a stuck notice (Continue, Drop task, Discard), drawn as a notice.
pub fn is_stuck_notice(row: &HeldRow) -> bool {
    row.choices.contains(&HeldChoice::Continue)
}

/// The whole card for a decision or a stuck notice, with its answered text once answered: the
/// chosen label.
pub fn decision_card(row: &HeldRow) -> Card {
    let stuck = is_stuck_notice(row);
    let mut card = if stuck {
        Card::new(CardKind::Notice, "Stuck")
    } else {
        Card::new(CardKind::Decision, "Decision")
    };
    let (title, note) = split_title(&row.title);
    card.held = Some(row.id);
    card.title = Some(title);
    card.note = note;
    card.lines = decision(row).map(lines).unwrap_or_default();
    card.buttons = row
        .choices
        .iter()
        .zip('1'..='9')
        .map(|(choice, key)| {
            CardButton::new(
                format!("{key} · {}", choice.label()),
                key,
                CardAction::Answer {
                    answer: HeldAnswer::Choose {
                        choice: choice.clone(),
                        words: None,
                    },
                },
            )
        })
        .collect();
    card.answered = match row.status {
        HeldStatus::Pending => None,
        HeldStatus::Answered => Some(match &row.answer {
            Some(HeldAnswer::Choose { choice, .. }) => choice.label().to_owned(),
            _ => "Answered".to_owned(),
        }),
        HeldStatus::Superseded => Some("No longer applies".to_owned()),
        HeldStatus::Approved => Some("Approved".to_owned()),
        HeldStatus::Rejected => Some("Rejected".to_owned()),
    };
    card
}

/// The decision card and stuck notice, registered with `HeldService` for `HeldKind::Decision`.
/// `facts` is the hold's `task.held` payload; the note comes from it when it names the project,
/// tier and model, else from the stored title.
#[derive(Clone, Copy, Debug, Default)]
pub struct DecisionCards;

impl HeldCardBuilder for DecisionCards {
    fn card(&self, row: &HeldRow, facts: &serde_json::Value) -> Card {
        let mut card = decision_card(row);
        let field = |k: &str| facts.get(k).and_then(serde_json::Value::as_str);
        if let (Some(tier), Some(model)) = (field("tier"), field("model")) {
            card.note = Some(match field("project").filter(|p| !p.is_empty()) {
                Some(project) => format!("· {project} · {tier} · {model}"),
                None => format!("· {tier} · {model}"),
            });
        }
        card
    }
}
