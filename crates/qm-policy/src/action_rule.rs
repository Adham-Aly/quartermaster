//! The effective-action rule.
//!
//! "What counts as an effective action: only a successful call that changed something: a reply,
//! question or acknowledgement shown to you; work started, steered, stopped or settled; a task or
//! setting changed; a schedule added or cancelled. Lookups, and refused or failed calls, never
//! count. The app never checks whether the action fits the turn, or whether an answer is right."
//!
//! - "Not every turn needs a reply. … If the Quartermaster wrote no words, the app writes its own
//!   one-line acknowledgement from what actually happened, never from the model's text: 'Started:
//!   C-0048', 'Steered: C-0042', or a plain 'Got it'."
//! - "A turn with no action gets one retry. In the retry only `reply`, `ask_user` and
//!   `acknowledge` work, and every tool that changes work or settings returns a refusal as text.
//!   If that does nothing too, you get one notice that quotes your message, with two choices: Ask
//!   again and Leave it."
//! - "An acknowledgement on its own satisfies the rule."
//! - "the check, the retry and the notice apply only to turns you started."

use qm_types::domain::StartedBy;
use qm_types::error::Refusal;
use qm_types::ids::TaskId;
use qm_types::rules::ToolClass;

/// How a call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CallOutcome {
    Succeeded,
    Refused,
    Failed,
}

/// Whether a call counts as an effective action.
pub fn counts(class: ToolClass, outcome: CallOutcome) -> bool {
    outcome == CallOutcome::Succeeded && class != ToolClass::Read
}

/// Which attempt of a user turn is running.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attempt {
    First,
    /// The one retry, where only `Speak` tools work.
    Retry,
}

/// The retry's gate: "every tool that changes work or settings returns a refusal as text".
pub fn check_allowed_in(attempt: Attempt, class: ToolClass, tool: &str) -> Result<(), Refusal> {
    match (attempt, class) {
        (Attempt::Retry, ToolClass::Act) => Err(Refusal::new(format!(
            "{tool} isn't available now; answer the user with reply, ask_user or acknowledge"
        ))),
        _ => Ok(()),
    }
}

/// What an effective action did, for the app's own acknowledgement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Effect {
    /// A reply, question or acknowledgement shown to you.
    Shown,
    Started(TaskId),
    Steered(TaskId),
    Stopped(TaskId),
    /// Finished work closed (`close_task`).
    Closed(TaskId),
    Settled(TaskId),
    TaskChanged(TaskId),
    SettingChanged,
    ScheduleAdded,
    ScheduleCancelled,
}

impl Effect {
    fn line(&self) -> Option<String> {
        match self {
            Self::Started(t) => Some(format!("Started: {t}")),
            Self::Steered(t) => Some(format!("Steered: {t}")),
            Self::Stopped(t) => Some(format!("Stopped: {t}")),
            Self::Closed(t) => Some(format!("Closed: {t}")),
            Self::Settled(t) => Some(format!("Settled: {t}")),
            Self::TaskChanged(t) => Some(format!("Changed: {t}")),
            Self::Shown | Self::SettingChanged | Self::ScheduleAdded | Self::ScheduleCancelled => {
                None
            }
        }
    }
}

/// The two choices on the notice.
pub const NOTICE_CHOICES: [&str; 2] = ["Ask again", "Leave it"];

/// What the app does when a turn ends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnEnd {
    /// Nothing more to do.
    Done,
    /// The rule is satisfied but nothing was shown: the app shows its own acknowledgement.
    AppAcknowledgement(String),
    /// No action: run the one retry.
    Retry,
    /// No action after the retry: one notice quoting your message, with [`NOTICE_CHOICES`].
    Notice { quote: String },
}

/// The app's one-line acknowledgement, written from what happened, never from the model's text.
///
/// Several effects in one turn join their lines with " · ".
pub fn app_acknowledgement(effects: &[Effect]) -> String {
    let lines: Vec<String> = effects.iter().filter_map(Effect::line).collect();
    if lines.is_empty() {
        "Got it".to_owned()
    } else {
        lines.join(" · ")
    }
}

/// The rule at the end of a turn. `effects` are the turn's effective actions (calls that
/// [`counts`]); `message` is your message, quoted by the notice.
pub fn end_of_turn(
    started_by: StartedBy,
    attempt: Attempt,
    effects: &[Effect],
    message: &str,
) -> TurnEnd {
    if !started_by.is_user() {
        return TurnEnd::Done;
    }
    if effects.is_empty() {
        return match attempt {
            Attempt::First => TurnEnd::Retry,
            Attempt::Retry => TurnEnd::Notice {
                quote: message.to_owned(),
            },
        };
    }
    if effects.contains(&Effect::Shown) {
        TurnEnd::Done
    } else {
        TurnEnd::AppAcknowledgement(app_acknowledgement(effects))
    }
}

/// "At most about 40 characters" for an acknowledgement.
pub const MAX_ACKNOWLEDGEMENT_CHARS: usize = 40;

/// `acknowledge`: a few words, never empty, at most about 40 characters.
pub fn check_acknowledgement(text: &str) -> Result<(), Refusal> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Refusal::new("an acknowledgement can't be empty"));
    }
    let n = text.chars().count();
    if n > MAX_ACKNOWLEDGEMENT_CHARS {
        return Err(Refusal::new(format!(
            "an acknowledgement is at most {MAX_ACKNOWLEDGEMENT_CHARS} characters (this one is {n}); use reply for a full answer"
        )));
    }
    Ok(())
}

/// `reply`: not empty.
pub fn check_reply(text: &str) -> Result<(), Refusal> {
    if text.trim().is_empty() {
        Err(Refusal::new("a reply can't be empty"))
    } else {
        Ok(())
    }
}
