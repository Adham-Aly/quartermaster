//! Startup recovery for the seat.
//!
//! 1. Every turn still `running` (the app stopped mid-turn) becomes `interrupted`, and its wakes go
//!    back in the inbox, so a crash never loses one (at-least-once delivery).
//! 2. Turns cut short by a stop or a quit, by kind: a **wake turn** runs again (its wakes are back
//!    in the inbox, and the turn that takes them carries the "app restarted" note); a **user
//!    turn** does not, and gets the failed notice "The app quit before this turn finished" with Ask
//!    again and Leave it. Each is then marked recovered, so it is handled once.
//! 3. Messages no turn has taken (queued when the app stopped) run, oldest first.
//! 4. The pending wakes are claimed by the first turn, which starts at once if one of them wakes
//!    the seat by itself; an intervention alone rides on the next turn.

use super::input::restart_note;
use super::{TurnLoop, local_hhmm};
use crate::reconcile::wakes_on_its_own;
use crate::turn_io::{Notice, SeatError};
use qm_types::domain::StartedBy;
use qm_types::ids::AgentId;

/// Whether a turn of yours cut short by quitting runs again on the next launch (no; it gets the
/// notice instead).
pub const RERUN_USER_TURNS_AFTER_QUIT: bool = false;

/// The notice's reason for a turn of yours cut short by quitting.
pub const QUIT_REASON: &str = "The app quit before this turn finished";

impl TurnLoop {
    pub(super) async fn recover(&mut self) -> Result<(), SeatError> {
        let db = self.deps.db.clone();
        let Some(agent) = self.own_agent().await? else {
            // First run: nothing was ever started.
            return Ok(());
        };
        self.agent = Some(agent);
        let at = self.deps.clock.now();

        let stopped = db
            .call(move |conn| qm_store::turns::interrupt_running(conn, agent, at))
            .await?;
        if !stopped.is_empty() {
            tracing::info!(
                turns = stopped.len(),
                "seat turns interrupted by the app stopping"
            );
        }

        let cut = db
            .call(move |conn| qm_store::turns::cut_short(conn, agent))
            .await?;
        let mut wake_turn_cut = false;
        let mut shown = false;
        for t in cut {
            match t.started_by {
                StartedBy::AppEvent => wake_turn_cut = true,
                StartedBy::User { message } if RERUN_USER_TURNS_AFTER_QUIT => {
                    self.queue.push_user(message);
                }
                StartedBy::User { message } => {
                    let notice = Notice::Failed {
                        reason: QUIT_REASON.to_owned(),
                    };
                    if let Err(e) = self.deps.chat.notice(agent, t.turn, message, notice).await {
                        // Left unrecovered, so the next launch tries again.
                        tracing::warn!(error = %e, "couldn't show the quit notice");
                        continue;
                    }
                    shown = true;
                }
            }
            let turn = t.turn;
            db.call(move |conn| qm_store::turns::mark_recovered(conn, turn))
                .await?;
        }
        if shown {
            self.deps.chat.changed();
        }
        if wake_turn_cut {
            self.restart_note = Some(restart_note(&local_hhmm(at)));
        }

        match self.deps.chat.waiting_messages(agent).await {
            Ok(waiting) => {
                for message in waiting {
                    self.queue.push_user(message);
                }
            }
            Err(e) => tracing::warn!(error = %e, "couldn't read the messages waiting for a turn"),
        }

        self.catch_up_wakes(agent).await
    }

    /// Startup, or a turn that left wakes behind: the pending wakes are ready now if one of them
    /// wakes the seat by itself; an intervention alone waits for the next turn.
    pub(super) async fn catch_up_wakes(&mut self, agent: AgentId) -> Result<(), SeatError> {
        if self.pending_wake_wakes(agent).await? {
            self.queue.assume_wakes(self.deps.clock.instant());
        }
        Ok(())
    }

    /// Whether one of `agent`'s pending wakes starts a turn by itself (not only interventions).
    async fn pending_wake_wakes(&self, agent: AgentId) -> Result<bool, SeatError> {
        let kinds = self
            .deps
            .db
            .call(move |conn| qm_store::wakes::pending_kinds(conn, agent))
            .await?;
        Ok(kinds.into_iter().any(wakes_on_its_own))
    }

    /// The same for the seat's own agent; true when it isn't known yet, so nothing is held back.
    pub(super) async fn a_pending_wake_wakes(&self) -> Result<bool, SeatError> {
        match self.agent {
            Some(agent) => self.pending_wake_wakes(agent).await,
            None => Ok(true),
        }
    }
}
