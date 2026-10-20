//! `present_hold`: the supervisor's view on a held or stuck task's decision card, in two or three
//! sentences. Only the pending row's `summary` changes; the app-written title, reason and choices
//! never do. Calling it again replaces the summary while the card is pending.

use super::DecisionService;
use crate::verification::levels::check_direct;
use qm_policy::markers::neutralise;
use qm_store::decisions as store;
use qm_store::tasks as task_store;
use qm_tools::defs::Held;
use qm_tools::handler::{Caller, ToolFailure};
use qm_types::domain::TaskStatus;
use qm_types::error::Refusal;
use qm_types::ids::TaskId;

/// The longest summary (matches `descriptions/present_hold.md`; the handler checks it).
pub use qm_tools::handlers::verification::MAX_HOLD_SUMMARY_CHARS;
/// `events.kind` of the supervisor's summary.
pub const PRESENTED_EVENT: &str = "task.hold_presented";
/// What `present_hold` tells the supervisor.
pub const PRESENTED_NOTE: &str =
    "The user has the card. The app carries out their choice and wakes you with the outcome.";

impl DecisionService {
    /// `present_hold`, after the handler's checks (summary not blank, at most
    /// [`MAX_HOLD_SUMMARY_CHARS`]).
    pub async fn present_hold(
        &self,
        caller: &Caller,
        task: TaskId,
        summary: String,
    ) -> Result<Held, ToolFailure> {
        let caller = *caller;
        let summary = neutralise(summary.trim());
        let at = self.clock().now();
        let outcome = self
            .db()
            .call(move |conn| {
                let tx = conn.transaction()?;
                let Some(row) = task_store::load(&tx, task)? else {
                    return Ok(Err(Refusal::new(format!("{task} doesn't exist"))));
                };
                if let Err(r) = check_direct(&tx, &caller, &row)? {
                    return Ok(Err(r));
                }
                if !matches!(row.status, TaskStatus::Held | TaskStatus::Stuck) {
                    return Ok(Err(Refusal::new(format!(
                        "{task} isn't held (it's {}); present_hold is only for held or stuck tasks",
                        row.status
                    ))));
                }
                let Some(held) = store::pending_for(&tx, task)? else {
                    return Ok(Err(Refusal::new(format!(
                        "{task} has no decision card waiting for the user"
                    ))));
                };
                if !store::set_summary(&tx, held, &summary)? {
                    return Ok(Err(Refusal::new(
                        "the card was answered meanwhile; there's nothing to present",
                    )));
                }
                task_store::event(
                    &tx,
                    PRESENTED_EVENT,
                    task,
                    Some(caller.agent),
                    &format!("{task}'s decision card has the supervisor's view"),
                    serde_json::json!({ "held": held }),
                    at,
                )?;
                tx.commit()?;
                Ok(Ok(held))
            })
            .await
            .map_err(|e| ToolFailure::Failed(e.to_string()))?;
        let held = outcome?;
        self.changed();
        Ok(Held {
            held_action: held,
            note: PRESENTED_NOTE.to_owned(),
        })
    }
}
