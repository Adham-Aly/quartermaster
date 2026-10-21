//! Steps 2 and 3 of a turn: recording and stamping it, assembling its input, and driving the engine
//! through it.

use super::deadline::next_deadline;
use super::dispatch::run_tool;
use super::effects::effect_line;
use super::input::assemble;
use super::{CLAIM_LIMIT, Control, INTERRUPT_GRACE, LoopRole, OpenReason, TurnLoop, close_reason};
use crate::turn_io::{SeatError, SeatInput};
use jiff::Timestamp;
use qm_engine::{
    EngineEvent, FailureKind, RateLimitStatus, StopReason, ToolResult, TurnInput, TurnMeter,
    TurnOutcome, TurnUsage,
};
use qm_policy::action_rule::{Attempt, Effect};
use qm_policy::limits::{self, LimitHit};
use qm_store::turns::{NewTurn, RecordedAction};
use qm_store::wakes::{self, ClaimedWake};
use qm_tools::handler::{Caller, ToolCx, ToolFailure, TurnCx};
use qm_types::domain::StartedBy;
use qm_types::error::Refusal;
use qm_types::ids::{AgentId, MessageId, TurnId};
use qm_types::rules::TurnCounters;
use std::sync::Arc;
use std::time::Instant;

/// What starts a turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Start {
    /// The next message of yours in the queue.
    User,
    /// The pending wakes.
    Wakes,
    /// The engine started a turn nobody asked for: an app-event turn with no wakes.
    Adopted,
}

/// Who stopped an engine turn early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Interruption {
    /// `Esc`.
    You,
    /// The core, for a limit.
    Limit,
    /// The app is quitting.
    Shutdown,
}

/// How one engine turn ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    Completed,
    Interrupted {
        by: Interruption,
    },
    /// `kind` is the engine's; `None` when the session went away or the turn couldn't be
    /// prepared.
    Failed {
        reason: String,
        kind: Option<FailureKind>,
    },
}

/// Everything one engine turn (the first attempt or the retry) came to.
#[derive(Clone, Debug)]
pub(crate) struct EngineTurnEnd {
    pub outcome: Ended,
    /// Set when the core stopped the turn for a limit, with the counters at that moment.
    pub limit: Option<(LimitHit, TurnCounters)>,
    /// What the counted calls did, in call order.
    pub effects: Vec<Effect>,
    /// The counted calls, for `turns.effective_actions`.
    pub actions: Vec<RecordedAction>,
    pub final_text: String,
    pub usage: TurnUsage,
    /// The account hit its usage limit (`RateLimit { Rejected }` or `FailureKind::AccountLimit`),
    /// with its reset time when known.
    pub account_limited: bool,
    pub account_resets_at: Option<Timestamp>,
    pub end_conversation: bool,
    /// The session closed during the turn: why the next `open` happens.
    pub session_lost: Option<OpenReason>,
    /// The input reached the model (`TurnStarted` came).
    pub delivered: bool,
}

impl EngineTurnEnd {
    fn new() -> Self {
        Self {
            outcome: Ended::Completed,
            limit: None,
            effects: Vec::new(),
            actions: Vec::new(),
            final_text: String::new(),
            usage: TurnUsage::default(),
            account_limited: false,
            account_resets_at: None,
            end_conversation: false,
            session_lost: None,
            delivered: false,
        }
    }

    /// The session went away mid-turn; the failure names whose session it was.
    fn lost(&mut self, stopping: Option<Interruption>, reason: String, who: LoopRole) {
        self.outcome = match stopping {
            Some(Interruption::Shutdown) => Ended::Interrupted {
                by: Interruption::Shutdown,
            },
            _ if self.limit.is_some() => Ended::Interrupted {
                by: Interruption::Limit,
            },
            _ => Ended::Failed {
                reason: format!("{} session closed: {reason}", who.possessive()),
                kind: None,
            },
        };
        self.session_lost = Some(OpenReason::Closed { reason });
    }
}

/// The facts of the turn being run, shared by its attempts.
#[derive(Clone, Debug)]
pub(crate) struct TurnCtx {
    pub agent: AgentId,
    pub turn: TurnId,
    pub started_by: StartedBy,
    pub message: Option<MessageId>,
    /// Your message's text, quoted by the notice; empty on a wake turn.
    pub message_text: String,
    /// The first claimed wake's `short`, for "A turn after {short} stopped".
    pub wake_short: Option<String>,
}

/// What woke the loop during an engine turn.
enum TurnWake {
    Event(Option<EngineEvent>),
    Input(Option<SeatInput>),
    Timer,
}

impl TurnLoop {
    /// Runs one turn from start to commit.
    pub(super) async fn run_turn(&mut self, start: Start) -> Result<(), SeatError> {
        // A model you changed while the seat sat idle: the new session takes this turn. (A turn
        // the engine already started is never moved.)
        if start != Start::Adopted {
            self.between_turns().await;
        }
        let Some((mut cx, claimed)) = self.begin(start).await? else {
            return Ok(());
        };
        // Step 2.3: publish.
        self.turn_running = true;
        self.running_message = cx.message;
        self.running_turn = Some(cx.turn);
        self.publish();

        if start == Start::Adopted {
            let end = self.drive(&cx, None, Attempt::First).await;
            return self.conclude(cx, end).await;
        }

        // Step 2.4: assemble.
        let prepared = self.prepare(&mut cx).await;
        let context = match prepared {
            Ok(context) => context,
            Err(e) => {
                let mut end = EngineTurnEnd::new();
                end.outcome = Ended::Failed {
                    reason: format!("the turn couldn't be prepared: {e}"),
                    kind: None,
                };
                return self.conclude(cx, end).await;
            }
        };
        let restart = if claimed.is_empty() {
            None
        } else {
            self.restart_note.take()
        };
        let message = cx.message.map(|_| cx.message_text.clone());
        let preamble = self.session.as_mut().and_then(|l| l.preamble.take());
        let input = assemble(
            preamble.as_deref(),
            &context,
            restart.as_deref(),
            &claimed,
            message.as_deref(),
        );

        // Step 3: the engine runs it.
        let mut end = self.drive(&cx, Some(input), Attempt::First).await;

        // The input never reached the model because the session couldn't be resumed: open a new
        // session and send the same input again, once.
        if let Some(OpenReason::ResumeFailed { .. }) = &end.session_lost
            && !end.delivered
            && !self.shutting_down
        {
            let why = end.session_lost.take().unwrap_or(OpenReason::Startup);
            self.lose_session(why);
            if self.ensure_session().await {
                let preamble = self.session.as_mut().and_then(|l| l.preamble.take());
                let input = assemble(
                    preamble.as_deref(),
                    &context,
                    restart.as_deref(),
                    &claimed,
                    message.as_deref(),
                );
                end = self.drive(&cx, Some(input), Attempt::First).await;
            } else {
                end.outcome = Ended::Failed {
                    reason: format!(
                        "{} session couldn't be resumed or reopened",
                        self.who.possessive()
                    ),
                    kind: None,
                };
            }
        }
        self.conclude(cx, end).await
    }

    /// Steps 2.1–2.2: picks the input, records and stamps the turn, claims its wakes. `None` when
    /// no turn runs after all (no session, or no wakes left).
    async fn begin(
        &mut self,
        start: Start,
    ) -> Result<Option<(TurnCtx, Vec<ClaimedWake>)>, SeatError> {
        // A lazily opened session (a secondmate's) isn't opened for a signal with no wake behind
        // it, such as the catch-up at startup.
        if self.lazy_open
            && self.session.is_none()
            && start == Start::Wakes
            && let Some(agent) = self.agent
        {
            let pending = self
                .deps
                .db
                .call(move |conn| qm_store::turns::pending_wake_count(conn, agent))
                .await?;
            if pending == 0 {
                self.queue.wakes_taken();
                return Ok(None);
            }
        }
        if start != Start::Adopted && !self.ensure_session().await {
            return Ok(None);
        }
        let Some(live) = self.session.as_ref() else {
            return Ok(None);
        };
        let agent = live.agent;
        let model = live.model.clone();
        let db = self.deps.db.clone();

        if start == Start::Wakes {
            let pending = db
                .call(move |conn| qm_store::turns::pending_wake_count(conn, agent))
                .await?;
            if pending == 0 {
                // A signal for a wake another path already consumed.
                self.queue.wakes_taken();
                return Ok(None);
            }
        }

        // Step 2: pick the input and stamp who started it.
        let started_by = match start {
            Start::User => {
                let Some(message) = self.queue.take_user() else {
                    return Ok(None);
                };
                // A request to run a message can arrive twice (startup recovery found it
                // waiting, and its own signal came too, possibly after its turn): the store says
                // whether it is still owed a turn.
                let owed = db
                    .call(move |conn| qm_store::messages::owed_turn(conn, message))
                    .await?;
                if !owed {
                    tracing::debug!(%message, "a message that already had its turn; not run again");
                    return Ok(None);
                }
                // A message of yours takes every pending wake into its turn.
                self.queue.wakes_taken();
                StartedBy::User { message }
            }
            Start::Wakes => {
                self.queue.wakes_taken();
                StartedBy::AppEvent
            }
            Start::Adopted => StartedBy::AppEvent,
        };
        let at = self.deps.clock.now();
        let new = NewTurn {
            agent,
            started_by,
            model,
            at,
        };
        let turn = db
            .call(move |conn| qm_store::turns::start_turn(conn, &new))
            .await?;
        let claimed: Vec<ClaimedWake> = if start == Start::Adopted {
            Vec::new()
        } else {
            db.call(move |conn| wakes::claim_wakes(conn, agent, turn, at, CLAIM_LIMIT))
                .await?
        };
        if claimed.len() >= CLAIM_LIMIT as usize {
            // More may wait behind these: another turn takes them as soon as this one ends.
            self.catch_up_wakes(agent).await?;
        }

        let cx = TurnCtx {
            agent,
            turn,
            started_by,
            message: started_by.message(),
            message_text: String::new(),
            wake_short: claimed.first().and_then(|w| {
                w.payload
                    .get("short")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned)
            }),
        };

        if start == Start::Wakes && claimed.is_empty() {
            self.commit_quiet(&cx, qm_store::turns::NO_WAKES).await?;
            return Ok(None);
        }
        Ok(Some((cx, claimed)))
    }

    /// Your message's text and the context block.
    async fn prepare(&mut self, cx: &mut TurnCtx) -> Result<String, SeatError> {
        if let Some(message) = cx.message {
            cx.message_text = self.deps.chat.message_text(message).await?;
        }
        self.deps.context.context_block(cx.agent, cx.turn).await
    }

    /// Drives one engine turn to its end. `input` is `None` for a turn the engine already
    /// started.
    pub(super) async fn drive(
        &mut self,
        cx: &TurnCtx,
        input: Option<TurnInput>,
        attempt: Attempt,
    ) -> EngineTurnEnd {
        let mut end = EngineTurnEnd::new();
        let who = self.who;
        let clock = Arc::clone(&self.deps.clock);
        let Some(live) = self.session.as_ref() else {
            end.lost(None, "no session was open".to_owned(), who);
            return end;
        };
        match input {
            Some(input) => {
                if live.handle.start_turn(input).is_err() {
                    // The engine has already let go of the session. What it said last is still
                    // waiting to be read, often a `SessionClosed { ResumeFailed }` sent as it
                    // opened: the loop below reads it, so the turn is reopened and resent rather
                    // than failed for a closed channel.
                    tracing::debug!(who = %self.who, "the session was gone before the turn started");
                }
            }
            None => end.delivered = true,
        }
        // The turn's clocks run from the moment it was asked for.
        let mut meter = TurnMeter::start(clock.instant());
        let mut stopping: Option<(Interruption, Instant)> = None;

        loop {
            let now = clock.instant();
            let deadline = match stopping {
                Some((_, grace)) => grace,
                // Tools run inline, so the call clock is always running while the loop waits.
                None => next_deadline(&meter.counters(now), true, now),
            };
            let sleep = clock.sleep_until(deadline);
            let Some(live) = self.session.as_mut() else {
                end.lost(stopping.map(|s| s.0), "no session was open".to_owned(), who);
                return end;
            };
            let wake = tokio::select! {
                biased;
                ev = live.handle.next_event() => TurnWake::Event(ev),
                input = self.inbox.inputs.recv() => TurnWake::Input(input),
                () = sleep => TurnWake::Timer,
            };
            let now = clock.instant();
            match wake {
                TurnWake::Input(input) => self.input_during_turn(input, &mut stopping, now),
                TurnWake::Timer => {
                    if let Some((why, grace)) = stopping
                        && now >= grace
                    {
                        end.usage.model_calls = meter.counters(now).model_calls;
                        end.lost(
                            Some(why),
                            "it didn't stop within 15 seconds of being interrupted".to_owned(),
                            who,
                        );
                        return end;
                    }
                }
                TurnWake::Event(None) => {
                    end.usage.model_calls = meter.counters(now).model_calls;
                    end.lost(
                        stopping.map(|s| s.0),
                        "the engine stopped without closing the session".to_owned(),
                        who,
                    );
                    return end;
                }
                TurnWake::Event(Some(event)) => {
                    meter.observe(&event, now);
                    if let Some(done) = self
                        .turn_event(cx, attempt, event, &mut end, &mut meter, stopping)
                        .await
                    {
                        if done == Finished::Lost {
                            end.usage.model_calls = meter.counters(clock.instant()).model_calls;
                        }
                        return end;
                    }
                }
            }
            if stopping.is_none() {
                stopping = self.check_limits(&mut end, &meter, clock.instant());
            }
        }
    }

    /// An input while a turn runs: your message queues, a wake updates the window, `Esc`
    /// interrupts, `Shutdown` interrupts for the quit.
    fn input_during_turn(
        &mut self,
        input: Option<SeatInput>,
        stopping: &mut Option<(Interruption, Instant)>,
        now: Instant,
    ) {
        let why = match self.take_input(input) {
            Control::Nothing => return,
            Control::Interrupt => Interruption::You,
            Control::Shutdown => Interruption::Shutdown,
        };
        if let Some((current, _)) = stopping {
            // Already stopping; a quit still turns it into the quit.
            if why == Interruption::Shutdown {
                *current = Interruption::Shutdown;
            }
            return;
        }
        self.send_interrupt();
        *stopping = Some((why, now + INTERRUPT_GRACE));
    }

    /// Checks the limits after an event or a deadline; a hit interrupts the turn.
    fn check_limits(
        &self,
        end: &mut EngineTurnEnd,
        meter: &TurnMeter,
        now: Instant,
    ) -> Option<(Interruption, Instant)> {
        let counters = meter.counters(now);
        let hit = end
            .limit
            .map(|(hit, _)| hit)
            .or_else(|| limits::check(self.who.role(), &counters).err())?;
        end.limit.get_or_insert((hit, counters));
        tracing::info!(limit = ?hit, "a seat turn reached a limit");
        self.send_interrupt();
        Some((Interruption::Limit, now + INTERRUPT_GRACE))
    }

    /// Handles one event of a running engine turn; `Some` when the engine turn is over.
    async fn turn_event(
        &mut self,
        cx: &TurnCtx,
        attempt: Attempt,
        event: EngineEvent,
        end: &mut EngineTurnEnd,
        meter: &mut TurnMeter,
        stopping: Option<(Interruption, Instant)>,
    ) -> Option<Finished> {
        let stopping_why = stopping.map(|s| s.0);
        match event {
            EngineEvent::SessionReady {
                session,
                resumed,
                init,
            } => self.session_ready(&session, resumed, &init).await,
            EngineEvent::TurnStarted => end.delivered = true,
            EngineEvent::ModelCallStarted { call } => {
                tracing::debug!(call, "seat model call started");
            }
            // The Quartermaster's text is never shown; `final_text` carries it.
            EngineEvent::TextLine { .. } => {}
            EngineEvent::ToolCall { call, name, args } => {
                let result = self
                    .tool_call(cx, attempt, name, args, end, stopping.is_some())
                    .await;
                let sent = self
                    .session
                    .as_ref()
                    .is_some_and(|l| l.handle.tool_result(call, result).is_ok());
                meter.tool_finished(self.deps.clock.instant());
                if !sent {
                    end.lost(stopping_why, "the engine stopped".to_owned(), self.who);
                    return Some(Finished::Lost);
                }
            }
            EngineEvent::ModelCallFinished { usage, stop, .. } => {
                if stop == StopReason::MaxTokens && end.limit.is_none() {
                    let counters = meter.counters(self.deps.clock.instant());
                    end.limit = Some((LimitHit::OutputTokens, counters));
                }
                tracing::debug!(
                    output_tokens = usage.output_tokens,
                    ?stop,
                    "seat model call finished"
                );
            }
            EngineEvent::Retrying {
                attempt,
                max_attempts,
                error,
                delay_ms,
            } => {
                tracing::info!(attempt, max_attempts, kind = ?error.kind, delay_ms, "seat model call retrying");
            }
            EngineEvent::RateLimit { info } => {
                if info.status == RateLimitStatus::Rejected {
                    end.account_limited = true;
                    end.account_resets_at = info.resets_at;
                }
            }
            EngineEvent::Compacted {
                automatic,
                tokens_before,
                tokens_after,
            } => {
                if let Err(e) = self
                    .record_compacted(Some(cx.turn), automatic, tokens_before, tokens_after)
                    .await
                {
                    tracing::warn!(error = %e, "couldn't record the seat's compaction");
                }
            }
            EngineEvent::EndConversationCalled => end.end_conversation = true,
            EngineEvent::TurnEnded {
                outcome,
                usage,
                final_text,
            } => {
                end.usage = usage;
                end.final_text = final_text;
                end.outcome = ended_from(outcome, stopping_why, end);
                return Some(Finished::Ended);
            }
            EngineEvent::SessionClosed { reason } => {
                let text = close_reason(&reason);
                if let qm_engine::CloseReason::ResumeFailed { message } = &reason
                    && !end.delivered
                {
                    end.outcome = Ended::Failed {
                        reason: format!("{} session closed: {text}", self.who.possessive()),
                        kind: None,
                    };
                    end.session_lost = Some(OpenReason::ResumeFailed {
                        message: message.clone(),
                    });
                } else {
                    end.lost(stopping_why, text, self.who);
                }
                return Some(Finished::Lost);
            }
            EngineEvent::Suspended => tracing::debug!("seat session suspended mid-turn"),
            other => tracing::debug!(event = other.name(), "seat event ignored"),
        }
        None
    }

    /// Runs one of our tools for the turn, with the turn's `StartedBy` in `Caller::turn` (so
    /// overrides are refused on turns you didn't start), and records what it did. While
    /// the turn is being stopped nothing more is dispatched.
    async fn tool_call(
        &self,
        cx: &TurnCtx,
        attempt: Attempt,
        name: String,
        args: serde_json::Value,
        end: &mut EngineTurnEnd,
        stopping: bool,
    ) -> ToolResult {
        if stopping {
            return ToolResult::error(
                ToolFailure::Refused(Refusal::new("the turn is being stopped")).render(),
            );
        }
        let caller = Caller {
            agent: cx.agent,
            role: self.who.role(),
            turn: Some(TurnCx {
                turn: cx.turn,
                started_by: cx.started_by,
            }),
            task: None,
            secondmate: self.who.secondmate(),
        };
        let tool_cx = ToolCx {
            caller: &caller,
            clock: &*self.deps.clock,
            host: &*self.deps.host,
        };
        let ran = run_tool(&self.deps.dispatcher, &tool_cx, attempt, &name, args).await;
        if ran.counted {
            end.actions.push(RecordedAction {
                effect: ran.effect.as_ref().and_then(effect_line),
                tool: name,
            });
        }
        if let Some(effect) = ran.effect {
            end.effects.push(effect);
        }
        ran.result
    }

    /// Asks the engine to stop the running turn.
    fn send_interrupt(&self) {
        if let Some(live) = self.session.as_ref()
            && live.handle.interrupt().is_err()
        {
            tracing::warn!("the seat's session is gone; it couldn't be interrupted");
        }
    }
}

/// How an engine turn ended, from the engine's outcome and what the core asked for.
fn ended_from(
    outcome: TurnOutcome,
    stopping: Option<Interruption>,
    end: &mut EngineTurnEnd,
) -> Ended {
    match outcome {
        // A limit the core caught fails the turn even if the engine finished it (a message that
        // hits the cap fails the turn).
        TurnOutcome::Completed if end.limit.is_some() => Ended::Interrupted {
            by: Interruption::Limit,
        },
        TurnOutcome::Completed => Ended::Completed,
        TurnOutcome::Interrupted => match stopping {
            Some(by) => Ended::Interrupted { by },
            None => Ended::Failed {
                reason: "the engine stopped the turn".to_owned(),
                kind: None,
            },
        },
        TurnOutcome::Failed { failure } => match stopping {
            Some(Interruption::Shutdown) => Ended::Interrupted {
                by: Interruption::Shutdown,
            },
            _ if end.limit.is_some() => Ended::Interrupted {
                by: Interruption::Limit,
            },
            _ => {
                if failure.kind == FailureKind::AccountLimit {
                    end.account_limited = true;
                }
                Ended::Failed {
                    reason: failure.message,
                    kind: Some(failure.kind),
                }
            }
        },
    }
}

/// How the event handler finished an engine turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Finished {
    Ended,
    Lost,
}
