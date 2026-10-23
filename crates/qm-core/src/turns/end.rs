//! Steps 4 and 5 of a turn: checking it (limits, failures, the effective-action rule with its retry
//! and notice) and committing it.

use super::engine_turn::{Ended, EngineTurnEnd, Interruption, TurnCtx};
use super::input::retry_input;
use super::{OpenReason, TurnLoop, local_hhmm};
use crate::turn_io::{IdleOutcome, Notice, SeatError};
use qm_engine::FailureKind;
use qm_policy::action_rule::{
    Attempt, Effect, MAX_ACKNOWLEDGEMENT_CHARS, TurnEnd, app_acknowledgement, end_of_turn,
};
use qm_policy::limits::LimitHit;
use qm_store::turns::{
    FinishedTurn, LimitRecord, REASON_APP_QUIT, RecordedAction, SeatEvent, TurnTotals,
};
use qm_types::domain::TurnStatus;
use qm_types::rules::TurnCounters;

/// The tick beside your message after a turn you interrupted that showed nothing and did nothing.
pub const INTERRUPTED_TICK: &str = "Interrupted";

/// Fits the app's acknowledgement into `max` characters: when the joined effects run longer, keep
/// the leading items that fit and add ` +N` for the rest ("Started: T-0048 · Steered: T-0042 +1"),
/// so the tick always fits the header rule.
pub fn fit_acknowledgement(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let items: Vec<&str> = text.split(" · ").collect();
    for keep in (1..items.len()).rev() {
        let candidate = format!("{} +{}", items[..keep].join(" · "), items.len() - keep);
        if candidate.chars().count() <= max {
            return candidate;
        }
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// What a turn is committed as.
#[derive(Clone, Debug)]
struct Decision {
    status: TurnStatus,
    reason: Option<String>,
    detail: Option<String>,
    limit: Option<LimitRecord>,
    /// The turn's wakes go back in the inbox (a quit, or an input that never reached the model).
    release_wakes: bool,
}

impl Decision {
    fn completed(reason: Option<&str>) -> Self {
        Self {
            status: TurnStatus::Completed,
            reason: reason.map(str::to_owned),
            detail: None,
            limit: None,
            release_wakes: false,
        }
    }

    fn quit() -> Self {
        Self {
            status: TurnStatus::Interrupted,
            reason: Some(REASON_APP_QUIT.to_owned()),
            detail: None,
            limit: None,
            release_wakes: true,
        }
    }
}

/// What every attempt of a turn added up to.
#[derive(Clone, Debug, Default)]
struct Totals {
    effects: Vec<Effect>,
    actions: Vec<RecordedAction>,
    usage: TurnTotals,
    close_outs: Vec<String>,
    delivered: bool,
    end_conversation: bool,
    account_limited: bool,
    account_resets_at: Option<jiff::Timestamp>,
}

impl Totals {
    fn absorb(&mut self, end: &EngineTurnEnd) {
        self.effects.extend(end.effects.iter().cloned());
        self.actions.extend(end.actions.iter().cloned());
        let u = &end.usage;
        self.usage.model_calls += u.model_calls;
        self.usage.input_tokens += u.input_tokens;
        self.usage.output_tokens += u.output_tokens;
        self.usage.thinking_tokens += u.thinking_tokens;
        self.usage.cache_read_tokens += u.cache_read_tokens;
        self.usage.cache_write_tokens += u.cache_write_tokens;
        self.usage.cost_usd = match (self.usage.cost_usd, u.cost_usd) {
            (Some(a), Some(b)) => Some(a + b),
            (a, b) => a.or(b),
        };
        if !end.final_text.trim().is_empty() {
            self.close_outs.push(end.final_text.clone());
        }
        self.delivered |= end.delivered;
        self.end_conversation |= end.end_conversation;
        self.account_limited |= end.account_limited;
        if end.account_resets_at.is_some() {
            self.account_resets_at = end.account_resets_at;
        }
    }

    fn shown(&self) -> bool {
        self.effects.contains(&Effect::Shown)
    }
}

impl TurnLoop {
    /// Checks a turn whose first engine turn has ended, runs the retry if the rule asks for it,
    /// shows what the turn needs shown, and commits it.
    pub(super) async fn conclude(
        &mut self,
        cx: TurnCtx,
        first: EngineTurnEnd,
    ) -> Result<(), SeatError> {
        let mut totals = Totals::default();
        let mut retried = false;
        let mut attempt = Attempt::First;
        let mut end = first;
        let decision = loop {
            totals.absorb(&end);
            if let Some(why) = end.session_lost.take() {
                self.lose_session(why);
            }
            match &end.outcome {
                Ended::Completed => {
                    match end_of_turn(cx.started_by, attempt, &end.effects, &cx.message_text) {
                        TurnEnd::Done => break Decision::completed(None),
                        TurnEnd::AppAcknowledgement(text) => {
                            self.app_acknowledgement(&cx, &text).await;
                            break Decision::completed(Some("app_acknowledgement"));
                        }
                        TurnEnd::Notice { .. } => {
                            self.notice(&cx, Notice::NoAction).await;
                            break Decision::completed(Some("no_action"));
                        }
                        TurnEnd::Retry if self.ending.is_some() => break Decision::quit(),
                        TurnEnd::Retry if self.session.is_none() || totals.end_conversation => {
                            let reason = format!(
                                "{} session closed before it answered",
                                self.who.possessive()
                            );
                            break self
                                .fail(&cx, &totals, None, &reason, "session_closed")
                                .await;
                        }
                        TurnEnd::Retry => {
                            let turn = cx.turn;
                            self.deps
                                .db
                                .call(move |conn| qm_store::turns::mark_retried(conn, turn))
                                .await?;
                            retried = true;
                            attempt = Attempt::Retry;
                            end = self.drive(&cx, Some(retry_input()), Attempt::Retry).await;
                        }
                    }
                }
                Ended::Interrupted {
                    by: Interruption::You,
                } => {
                    self.interrupted_by_you(&cx, &totals).await;
                    break Decision {
                        status: TurnStatus::Interrupted,
                        reason: Some("interrupted".to_owned()),
                        detail: None,
                        limit: None,
                        release_wakes: !totals.delivered,
                    };
                }
                Ended::Interrupted {
                    by: Interruption::Shutdown,
                } => break Decision::quit(),
                Ended::Interrupted {
                    by: Interruption::Limit,
                } => {
                    let (hit, counters) = end
                        .limit
                        .unwrap_or((LimitHit::TurnTime, TurnCounters::default()));
                    let code = format!("limit_{}", limit_name(hit));
                    let mut decision = self.fail(&cx, &totals, None, hit.reason(), &code).await;
                    decision.limit = Some(limit_record(hit, &counters));
                    break decision;
                }
                Ended::Failed { reason, kind } => {
                    let code = match kind {
                        Some(k) => format!("failed_{}", failure_name(*k)),
                        None if self.session.is_none() => "session_closed".to_owned(),
                        None => "failed".to_owned(),
                    };
                    let reason = reason.clone();
                    break self.fail(&cx, &totals, *kind, &reason, &code).await;
                }
            }
        };
        self.commit(&cx, decision, totals, retried).await
    }

    /// The failure path: on your turn the notice under your message when nothing was shown, else a
    /// line from the app; on a wake turn a line from the app.
    async fn fail(
        &mut self,
        cx: &TurnCtx,
        totals: &Totals,
        kind: Option<FailureKind>,
        reason: &str,
        code: &str,
    ) -> Decision {
        let reason = if totals.account_limited || kind == Some(FailureKind::AccountLimit) {
            match totals.account_resets_at {
                Some(at) => format!(
                    "The account running the Quartermaster reached its usage limit; it resets at {}.",
                    local_hhmm(at)
                ),
                None => "The account running the Quartermaster reached its usage limit.".to_owned(),
            }
        } else {
            reason.to_owned()
        };
        if cx.message.is_some() {
            if totals.shown() {
                let text = format!("The turn stopped early: {reason}");
                self.app_note(cx, &text).await;
            } else {
                self.notice(
                    cx,
                    Notice::Failed {
                        reason: reason.clone(),
                    },
                )
                .await;
            }
        } else {
            let short = cx.wake_short.as_deref().unwrap_or("an app event");
            let text = format!("A turn after {short} stopped: {reason}");
            self.app_note(cx, &text).await;
        }
        Decision {
            status: TurnStatus::Failed,
            reason: Some(code.to_owned()),
            detail: Some(reason),
            limit: None,
            release_wakes: !totals.delivered,
        }
    }

    /// `Esc`: no retry, no notice; on your turn, if nothing was shown, the app's tick from what was
    /// done so far, or "Interrupted".
    async fn interrupted_by_you(&mut self, cx: &TurnCtx, totals: &Totals) {
        if cx.message.is_none() || totals.shown() {
            return;
        }
        let text = if totals.effects.is_empty() {
            INTERRUPTED_TICK.to_owned()
        } else {
            app_acknowledgement(&totals.effects)
        };
        self.app_acknowledgement(cx, &text).await;
    }

    async fn app_acknowledgement(&self, cx: &TurnCtx, text: &str) {
        let Some(message) = cx.message else {
            return;
        };
        let text = fit_acknowledgement(text, MAX_ACKNOWLEDGEMENT_CHARS);
        if let Err(e) = self
            .deps
            .chat
            .app_acknowledgement(cx.agent, cx.turn, message, &text)
            .await
        {
            tracing::warn!(error = %e, "couldn't show the app's acknowledgement");
        }
    }

    async fn notice(&self, cx: &TurnCtx, notice: Notice) {
        let Some(message) = cx.message else {
            return;
        };
        if let Err(e) = self
            .deps
            .chat
            .notice(cx.agent, cx.turn, message, notice)
            .await
        {
            tracing::warn!(error = %e, "couldn't show the notice");
        }
    }

    async fn app_note(&self, cx: &TurnCtx, text: &str) {
        if let Err(e) = self.deps.chat.app_note(cx.agent, Some(cx.turn), text).await {
            tracing::warn!(error = %e, "couldn't show the app's note");
        }
    }

    /// Commits a wake turn that had nothing to deliver.
    pub(super) async fn commit_quiet(
        &mut self,
        cx: &TurnCtx,
        reason: &str,
    ) -> Result<(), SeatError> {
        self.commit(
            cx,
            Decision::completed(Some(reason)),
            Totals::default(),
            false,
        )
        .await
    }

    /// Step 5: the turn, its events and its wakes in one transaction; then the chat, the status,
    /// and the quiet moment between turns.
    async fn commit(
        &mut self,
        cx: &TurnCtx,
        decision: Decision,
        totals: Totals,
        retried: bool,
    ) -> Result<(), SeatError> {
        let model = self.session.as_ref().and_then(|l| l.model.clone());
        let end_conversation = totals.end_conversation;
        let finished = FinishedTurn {
            turn: cx.turn,
            agent: cx.agent,
            status: decision.status,
            reason: decision.reason,
            detail: decision.detail,
            actions: totals.actions,
            retried,
            usage: totals.usage,
            model,
            close_outs: totals.close_outs,
            limit: decision.limit,
            release_wakes: decision.release_wakes,
            at: self.deps.clock.now(),
        };
        self.deps
            .db
            .call(move |conn| qm_store::turns::finish_turn(conn, &finished))
            .await?;
        self.turn_running = false;
        self.running_message = None;
        self.running_turn = None;
        self.deps.chat.changed();
        self.publish();

        if self.ending.is_some() {
            return Ok(());
        }
        if end_conversation {
            // The app records it and opens a new session.
            let event = SeatEvent {
                kind: qm_store::turns::kind::END_CONVERSATION,
                agent: cx.agent,
                turn: Some(cx.turn),
                summary: "the Quartermaster ended its conversation; a new session opens".to_owned(),
                payload: serde_json::json!({}),
                at: self.deps.clock.now(),
            };
            self.deps
                .db
                .call(move |conn| qm_store::turns::record_event(conn, &event))
                .await?;
            self.forget_session().await;
            self.next_open = OpenReason::Closed {
                reason: "EndConversation".to_owned(),
            };
            self.ensure_session().await;
            return Ok(());
        }
        if self.session.is_none() {
            self.ensure_session().await;
            return Ok(());
        }
        self.between_turns().await;
        Ok(())
    }

    /// The quiet moment between turns, never during one: a pending change of the seat's model.
    /// Runs after each turn and again just before the next, so a change made while the seat sat
    /// idle applies before that turn rather than after it.
    pub(super) async fn between_turns(&mut self) {
        let Some(live) = self.session.as_ref() else {
            return;
        };
        let outcome = self.deps.seat.idle(live.agent, &live.handle).await;
        match outcome {
            Ok(IdleOutcome::Unchanged) => {}
            Ok(IdleOutcome::Reopen(why)) => {
                self.close_session().await;
                self.next_open = why;
                self.ensure_session().await;
            }
            Err(e) => tracing::warn!(error = %e, "the seat's idle check failed"),
        }
    }
}

fn limit_name(hit: LimitHit) -> &'static str {
    match hit {
        LimitHit::CallTime => "call_time",
        LimitHit::TurnTime => "turn_time",
        LimitHit::ModelCalls => "model_calls",
        LimitHit::OutputTokens => "output_tokens",
    }
}

fn failure_name(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Provider => "provider",
        FailureKind::AccountLimit => "account_limit",
        FailureKind::ProcessExited => "process_exited",
        FailureKind::Protocol => "protocol",
        FailureKind::Internal => "internal",
    }
}

fn limit_record(hit: LimitHit, c: &TurnCounters) -> LimitRecord {
    LimitRecord {
        limit: limit_name(hit).to_owned(),
        reason: hit.reason().to_owned(),
        counters: serde_json::json!({
            "model_calls": c.model_calls,
            "call_elapsed_ms": u64::try_from(c.call_elapsed.as_millis()).unwrap_or(u64::MAX),
            "turn_elapsed_ms": u64::try_from(c.turn_elapsed.as_millis()).unwrap_or(u64::MAX),
            "largest_message_output_tokens": c.largest_message_output_tokens,
            "output_tokens": c.output_tokens,
        }),
    }
}
