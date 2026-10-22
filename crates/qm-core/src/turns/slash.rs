//! Your slash commands, run between turns in the order you sent them (see [`super::queue`]).
//!
//! - **`/clear`** closes the session and opens a new one with an empty conversation: no summary
//!   of earlier turns, now or in any later session (`seat.cleared` marks where the summaries start
//!   again). The chat keeps its history on screen, with a line saying the context was cleared.
//! - **`/compact`** asks the engine to compact the conversation now. The engine runs it as a turn
//!   of its own (rule 7 of `qm_engine::EngineEvent`'s order); the loop drives that turn here,
//!   shows the seat as working meanwhile (so `Esc` stops it), and says in the chat how it went.

use super::{INTERRUPT_GRACE, TurnLoop, reopen_reason};
use crate::turn_io::{OpenReason, SeatError, SeatInput};
use qm_engine::{EngineEvent, TurnOutcome};
use qm_types::api::SlashCommand;
use qm_types::ids::AgentId;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The longest a compaction may run before the loop stops it.
pub const COMPACT_LIMIT: Duration = Duration::from_mins(10);

/// What the chat says after `/clear`.
pub const CLEARED_NOTE: &str =
    "Context cleared: the Quartermaster starts over in a new session with an empty conversation.";
/// What the chat says when there was nothing to compact.
pub const NOTHING_TO_COMPACT: &str =
    "Nothing to compact yet: the Quartermaster's conversation is still short.";
/// What the chat says when you stopped a compaction.
pub const COMPACT_STOPPED: &str = "Compaction stopped; the Quartermaster's context is unchanged.";

/// How a compaction turn ended.
enum Compaction {
    /// The engine compacted, with the sizes it reported.
    Done {
        before: Option<u64>,
        after: Option<u64>,
    },
    Nothing,
    Stopped,
    Failed(String),
}

/// What woke the loop during a compaction.
enum Wake {
    Event(Option<EngineEvent>),
    Input(Option<SeatInput>),
    Timer,
}

impl TurnLoop {
    /// Runs one slash command of yours.
    pub(super) async fn run_slash(&mut self, command: SlashCommand) -> Result<(), SeatError> {
        match command {
            SlashCommand::Clear => self.clear().await,
            SlashCommand::Compact => self.compact().await,
            other => {
                tracing::warn!(?other, "a slash command this build doesn't run");
                Ok(())
            }
        }
    }

    /// The seat's agent row: from the live session, or the store.
    async fn slash_agent(&mut self) -> Result<Option<AgentId>, SeatError> {
        if self.agent.is_none() {
            self.agent = self.own_agent().await?;
        }
        Ok(self.agent)
    }

    /// A line from the app in the chat, and a fresh snapshot.
    async fn slash_note(&self, agent: AgentId, text: &str) {
        match self.deps.chat.app_note(agent, None, text).await {
            Ok(_) => self.deps.chat.changed(),
            Err(e) => tracing::warn!(error = %e, "couldn't show a slash command's outcome"),
        }
    }

    /// `/clear`: a new session with an empty conversation.
    async fn clear(&mut self) -> Result<(), SeatError> {
        let Some(agent) = self.slash_agent().await? else {
            return Ok(());
        };
        self.forget_session().await;
        self.lose_session(OpenReason::Cleared);
        let at = self.deps.clock.now();
        let summary = format!("You cleared {} context", self.who.possessive());
        self.deps
            .db
            .call(move |conn| qm_store::turns::record_cleared(conn, agent, &summary, at))
            .await?;
        // After the marker: it records the newest turn it follows, which forgetting would erase.
        let forget_tasks = self.who == super::LoopRole::Quartermaster;
        let forgot = self
            .deps
            .db
            .call(move |conn| {
                qm_store::history::forget_conversation(conn, agent, i64::MAX)?;
                if forget_tasks {
                    qm_store::history::forget_closed_task_details(conn)?;
                }
                Ok(())
            })
            .await;
        if let Err(e) = forgot {
            tracing::warn!(who = %self.who, error = %e, "couldn't forget the cleared conversation");
        } else if forget_tasks {
            self.notify_forgotten_outcomes().await?;
        }
        tracing::info!(who = %self.who, "context cleared; opening a new session");
        self.slash_note(agent, CLEARED_NOTE).await;
        // A new session now, so the next message doesn't wait for it; if none can open, this
        // says why in the chat, and the next input tries again.
        self.ensure_session().await;
        Ok(())
    }

    /// `/compact`: the engine compacts the conversation now.
    async fn compact(&mut self) -> Result<(), SeatError> {
        // A model you changed meanwhile applies first, as before a turn.
        self.between_turns().await;
        if !self.ensure_session().await {
            return Ok(());
        }
        // Snapshot the boundary before asking the engine, excluding queued inputs that its
        // summary will not represent (including ones already stored while the last turn ran).
        let Some(live) = self.session.as_ref() else {
            return Ok(());
        };
        let agent = live.agent;
        let before = self.compaction_boundary(agent).await?;
        if live.handle.compact().is_err() {
            // The engine let go of the session; what it said last is read below.
            tracing::debug!(who = %self.who, "the session was gone before the compaction");
        }
        self.turn_running = true;
        self.running_message = None;
        self.publish();
        self.deps.chat.changed();
        let how = self.drive_compaction(before).await?;
        self.turn_running = false;
        self.publish();
        let note = match how {
            Compaction::Done { before, after } => compacted_note(before, after),
            Compaction::Nothing => NOTHING_TO_COMPACT.to_owned(),
            Compaction::Stopped => COMPACT_STOPPED.to_owned(),
            Compaction::Failed(reason) => {
                format!("Couldn't compact the Quartermaster's context: {reason}")
            }
        };
        self.slash_note(agent, &note).await;
        if self.session.is_none() && !self.shutting_down {
            self.ensure_session().await;
        }
        Ok(())
    }

    /// Reads the session's events until the compaction turn ends.
    async fn drive_compaction(
        &mut self,
        before: super::CompactionBoundary,
    ) -> Result<Compaction, SeatError> {
        let clock = Arc::clone(&self.deps.clock);
        let limit = clock.instant() + COMPACT_LIMIT;
        let mut stopping: Option<Instant> = None;
        let mut compacted: Option<(Option<u64>, Option<u64>)> = None;
        loop {
            let Some(live) = self.session.as_mut() else {
                return Ok(Compaction::Failed("no session was open".to_owned()));
            };
            let sleep = clock.sleep_until(stopping.unwrap_or(limit));
            let wake = tokio::select! {
                biased;
                ev = live.handle.next_event() => Wake::Event(ev),
                input = self.inbox.inputs.recv() => Wake::Input(input),
                () = sleep => Wake::Timer,
            };
            match wake {
                Wake::Input(input) => {
                    if !matches!(self.take_input(input), super::Control::Nothing)
                        && stopping.is_none()
                    {
                        self.stop_compaction(&mut stopping);
                    }
                }
                Wake::Timer => {
                    if stopping.is_some() {
                        let reason = "it didn't stop within 15 seconds of being interrupted";
                        self.lose_session(OpenReason::Closed {
                            reason: reason.to_owned(),
                        });
                        return Ok(Compaction::Failed(format!("its session closed: {reason}.")));
                    }
                    tracing::warn!(who = %self.who, "a compaction ran too long; stopping it");
                    self.stop_compaction(&mut stopping);
                }
                Wake::Event(None) => {
                    let reason = "the engine stopped without closing the session";
                    self.lose_session(OpenReason::Closed {
                        reason: reason.to_owned(),
                    });
                    return Ok(Compaction::Failed(format!("its session closed: {reason}.")));
                }
                Wake::Event(Some(event)) => match event {
                    EngineEvent::SessionReady {
                        session,
                        resumed,
                        init,
                    } => self.session_ready(&session, resumed, &init).await,
                    EngineEvent::Compacted {
                        automatic,
                        tokens_before,
                        tokens_after,
                    } => {
                        self.record_compacted_before(
                            None,
                            Some(before.clone()),
                            automatic,
                            tokens_before,
                            tokens_after,
                        )
                        .await?;
                        compacted = Some((tokens_before, tokens_after));
                    }
                    EngineEvent::TurnEnded { outcome, .. } => {
                        return Ok(match (outcome, compacted) {
                            (_, Some((before, after))) => Compaction::Done { before, after },
                            (TurnOutcome::Completed, None) => Compaction::Nothing,
                            (TurnOutcome::Interrupted, None) => Compaction::Stopped,
                            (TurnOutcome::Failed { failure }, None) => {
                                Compaction::Failed(failure.message)
                            }
                        });
                    }
                    EngineEvent::SessionClosed { reason } => {
                        let why = reopen_reason(reason);
                        let words = match &why {
                            OpenReason::ResumeFailed { message } => message.clone(),
                            OpenReason::Closed { reason } => reason.clone(),
                            _ => "it closed".to_owned(),
                        };
                        self.lose_session(why);
                        return Ok(Compaction::Failed(format!("its session closed: {words}.")));
                    }
                    other => {
                        tracing::debug!(event = other.name(), "seat event during a compaction");
                    }
                },
            }
        }
    }

    /// `Esc`, the quit or the time limit: interrupt the compaction, and give it the grace period.
    fn stop_compaction(&mut self, stopping: &mut Option<Instant>) {
        if let Some(live) = self.session.as_ref()
            && live.handle.interrupt().is_err()
        {
            tracing::debug!("the session was gone when the compaction was stopped");
        }
        *stopping = Some(self.deps.clock.instant() + INTERRUPT_GRACE);
    }
}

/// "Compacted the Quartermaster's context (12,400 → 2,100 tokens)."
fn compacted_note(before: Option<u64>, after: Option<u64>) -> String {
    match (before, after) {
        (Some(before), Some(after)) => format!(
            "Compacted the Quartermaster's context ({} → {} tokens).",
            thousands(before),
            thousands(after)
        ),
        _ => "Compacted the Quartermaster's context.".to_owned(),
    }
}

/// `12400` as `12,400`.
fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_read_well() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_142), "1,142");
        assert_eq!(thousands(1_234_567), "1,234,567");
        assert_eq!(
            compacted_note(Some(12_400), Some(2_100)),
            "Compacted the Quartermaster's context (12,400 → 2,100 tokens)."
        );
        assert_eq!(
            compacted_note(None, None),
            "Compacted the Quartermaster's context."
        );
    }
}
