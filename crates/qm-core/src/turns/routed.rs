//! Supervisor requests join a secondmate's running turn without interrupting its work.

use super::TurnLoop;
use super::engine_turn::{EngineTurnEnd, TurnCtx};
use super::input::assemble;
use qm_store::{StoreError, inbox};
use qm_types::domain::WakeKind;
use qm_types::ids::{TurnId, WakeId};
use rusqlite::{Connection, params};
use tokio::sync::oneshot;

impl TurnLoop {
    /// Only routed requests bypass the next-turn wake window. Keep the turn's original authority
    /// and leave ordinary wakes (including ride-along interventions) in the inbox.
    pub(super) async fn inject_routed_wakes(&mut self, cx: &TurnCtx, end: &mut EngineTurnEnd) {
        if self.who.secondmate().is_none()
            || self.session.is_none()
            || !end.delivered
            || end.end_conversation
        {
            // Before TurnStarted, a resume can still fail. Its replacement resends only the
            // initial input, so leave new requests pending until the engine accepts the turn.
            // An ending conversation likewise leaves new requests for its replacement session.
            return;
        }
        let agent = cx.agent;
        let turn = cx.turn;
        let at = self.deps.clock.now();
        let (attempted_tx, attempted_rx) = oneshot::channel();
        let claimed = self
            .deps
            .db
            .call(move |conn| {
                let ids = inbox::pending_ids(conn, agent, &[WakeKind::Routed])?;
                let _ = attempted_tx.send(ids.clone());
                inbox::claim_ids(conn, &ids, turn, at)
            })
            .await;
        let claimed = match claimed {
            Ok(claimed) => claimed,
            Err(error) => {
                // A mirror flush can fail after the memory claim commits. Retain just this
                // attempt's pending IDs; release is guarded by this turn and unconsumed state.
                end.undelivered_wakes
                    .extend(attempted_rx.await.unwrap_or_default());
                tracing::warn!(%error, "couldn't claim routed requests for the running turn");
                self.queue.note_wake(self.deps.clock.instant());
                return;
            }
        };
        if claimed.is_empty() {
            return;
        }
        // The durable claim records delivery just like the wakes in the turn's initial input;
        // finish_turn consumes it, and startup recovery releases it after a crash.
        let input = assemble(None, "", None, &claimed, None);
        if self
            .session
            .as_ref()
            .is_none_or(|live| live.handle.inject(input).is_err())
        {
            // Don't release every wake of this turn: some already reached the model. These IDs
            // must be released before commit can consume anything, even if the channel closed
            // after TurnStarted. A failed database release then fails the commit, not the wake.
            end.undelivered_wakes.extend(claimed.iter().map(|w| w.id));
            self.queue.note_wake(self.deps.clock.instant());
            tracing::warn!(%turn, "the session was gone; routed requests will be retried");
        }
    }
}

/// Release only live inputs not sent after a claim or command failure. The caller must do this
/// before finishing the turn, and must not finish it if this fails. Successful deliveries keep
/// their original claims.
pub(super) fn release_undelivered(
    conn: &Connection,
    turn: TurnId,
    ids: &[WakeId],
) -> Result<(), StoreError> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "UPDATE wakes SET delivered_turn_id = NULL, delivered_at = NULL
         WHERE id = ?1 AND delivered_turn_id = ?2 AND consumed_at IS NULL",
    )?;
    for id in ids {
        stmt.execute(params![id, turn])?;
    }
    Ok(())
}
