//! The pump: fact rows from the event log.
//!
//! Every task fact is already an event written in the transaction that records it, so the pump
//! needs no hook in anyone's code: it waits for a nudge or [`PUMP_TICK`] on the injected clock,
//! reads the [`FACT_KINDS`](super::facts::FACT_KINDS) events after its watermark on tasks that
//! belong to a request, and writes one row (with its event and the Quartermaster's wake) per event
//! that maps to an outcome, each in its own transaction.
//!
//! The watermark starts at the newest event any row came from. Before any fact row exists it
//! starts at the first routed request (nothing on a request's task can come before it), or, with
//! no request at all, at the newest event, so history isn't replayed. It moves past every event
//! read, mapped or not. A crash
//! between reading and committing re-reads the same events; the row is deduplicated by its
//! `source_event`, so nothing goes up twice. A burst of facts wakes the Quartermaster once per row,
//! and the seat's two-second batching turns them into one turn.

use super::{PUMP_BATCH, ParentChannel, RETRY_PAUSE, facts};
use crate::secondmates::event_kind::ROUTED;
use qm_store::StoreError;
use qm_store::parent_channel as store;
use qm_types::ids::EventId;
use std::time::Duration;
use tokio::sync::watch;

/// How long the pump waits without a nudge.
pub const PUMP_TICK: Duration = Duration::from_secs(1);

impl ParentChannel {
    /// The pump, until `shutdown` turns true (or its sender goes away).
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut after = loop {
            match self.start_watermark().await {
                Ok(after) => break after,
                Err(e) => {
                    tracing::error!(error = %e, "the parent channel couldn't read its watermark");
                    tokio::select! {
                        () = self.clock().sleep(RETRY_PAUSE) => {}
                        r = shutdown.changed() => if r.is_err() || *shutdown.borrow() { return; },
                    }
                }
            }
        };
        loop {
            if *shutdown.borrow() {
                return;
            }
            let pause = match self.catch_up(after).await {
                Ok(through) => {
                    after = through;
                    PUMP_TICK
                }
                Err((through, e)) => {
                    after = through;
                    tracing::warn!(error = %e, "the parent channel couldn't send a fact up");
                    RETRY_PAUSE
                }
            };
            tokio::select! {
                () = self.nudged().notified() => {}
                () = self.clock().sleep(pause) => {}
                r = shutdown.changed() => if r.is_err() || *shutdown.borrow() { return; },
            }
        }
    }

    /// Where the pump starts: the newest event a row came from. Before any fact row exists, the
    /// first routed request (no task belongs to a request before it, so everything after it is
    /// unsent); without any request, the newest event. Starting at the newest event while requests
    /// exist would drop a fact committed just before a crash, when the pump hadn't yet written its
    /// first row.
    pub async fn start_watermark(&self) -> Result<Option<EventId>, StoreError> {
        self.db()
            .call(|conn| match store::watermark(conn)? {
                Some(w) => Ok(Some(w)),
                None => match store::first_event_of(conn, ROUTED)? {
                    Some(first) => Ok(Some(first)),
                    None => store::newest_event(conn),
                },
            })
            .await
    }

    /// Reads and sends up every fact after `after`; returns the new watermark. On a store
    /// failure, the watermark stops before the event that failed (so it is retried) and the
    /// error comes back with it.
    pub async fn catch_up(
        &self,
        mut after: Option<EventId>,
    ) -> Result<Option<EventId>, (Option<EventId>, StoreError)> {
        loop {
            let scan = self
                .db()
                .call(move |conn| store::events_after(conn, after, facts::FACT_KINDS, PUMP_BATCH))
                .await
                .map_err(|e| (after, e))?;
            let full = scan.rows.len() >= PUMP_BATCH;
            for event in scan.rows {
                let id = event.id;
                self.record_event(event).await.map_err(|e| (after, e))?;
                after = Some(id);
            }
            if scan.through.is_some() {
                after = after.max(scan.through);
            }
            if !full {
                return Ok(after);
            }
        }
    }
}
