//! Driving one helper child: its task reads every event the child session sends, from the first
//! turn until the session has closed.
//!
//! ```text
//! StartTurn(prompt)
//! ModelCallStarted       the model has seen every job exit before this call
//! ModelCallFinished      usage added to the crewmate's; the call limit interrupts the child
//! JobStarted/Finished    running jobs tracked; an exit after the turn's last call is unseen
//! TurnEnded, limit hit   an error answer with its last message
//! TurnEnded Completed    unseen exits → a new turn telling them
//!                        jobs running → wait for an exit (a new turn), at most HELPER_JOB_WAIT
//!                        else → its final text is the answer
//! TurnEnded Failed       an error answer
//! the call dropped       Forget: the child stops its jobs; drained until SessionClosed
//! ```

use super::{HELPER_JOB_WAIT, HELPER_MAX_MODEL_CALLS, lock};
use qm_engine::{CallUsage, EngineEvent, SessionHandle, TurnInput, TurnOutcome, TurnUsage};
use qm_types::clock::{Clock, Sleep};
use qm_types::ids::JobLabel;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};
use tokio::sync::{OwnedSemaphorePermit, oneshot};

/// What the call returns.
#[derive(Debug)]
pub(super) struct Answer {
    pub(super) text: String,
    pub(super) is_error: bool,
}

/// One child and what its driver needs.
pub(super) struct Child {
    pub(super) handle: SessionHandle,
    pub(super) prompt: String,
    /// The crewmate's helper usage, added to as each call finishes.
    pub(super) usage: Arc<Mutex<TurnUsage>>,
    pub(super) clock: Arc<dyn Clock>,
    /// Resolves when the call's future is dropped.
    pub(super) cancel: oneshot::Receiver<()>,
    pub(super) answer: oneshot::Sender<Answer>,
    /// The concurrency slot, held until the child has closed.
    pub(super) slot: OwnedSemaphorePermit,
}

/// The driver's state.
struct Drive {
    handle: SessionHandle,
    answer: Option<oneshot::Sender<Answer>>,
    usage: Arc<Mutex<TurnUsage>>,
    clock: Arc<dyn Clock>,
    /// Close sent: everything after is drained.
    closing: bool,
    calls: u32,
    /// The call limit interrupted the child.
    limited: bool,
    /// Running jobs and their commands.
    jobs: BTreeMap<JobLabel, String>,
    /// Exits the model hasn't had a call to see.
    unseen: Vec<String>,
    /// Between turns, waiting for a job to exit.
    waiting: Option<Sleep>,
    last_text: String,
}

/// Runs one child to its end (see the module docs).
pub(super) async fn drive(child: Child) {
    let Child {
        handle,
        prompt,
        usage,
        clock,
        mut cancel,
        answer,
        slot,
    } = child;
    let mut d = Drive {
        handle,
        answer: Some(answer),
        usage,
        clock,
        closing: false,
        calls: 0,
        limited: false,
        jobs: BTreeMap::new(),
        unseen: Vec::new(),
        waiting: None,
        last_text: String::new(),
    };
    if d.handle.start_turn(TurnInput::text(prompt)).is_err() {
        d.finish(error("the helper's session ended before it started".into()));
    }
    let mut cancelled = false;
    loop {
        tokio::select! {
            _ = &mut cancel, if !cancelled => {
                // The call was dropped: nobody waits for the answer any more.
                cancelled = true;
                d.answer = None;
                d.close();
            }
            () = wait(&mut d.waiting) => {
                d.waiting = None;
                d.jobs_outlasted_the_wait();
            }
            event = d.handle.next_event() => {
                let Some(event) = event else { break };
                if d.on_event(event) {
                    break;
                }
            }
        }
    }
    drop(slot);
}

async fn wait(sleep: &mut Option<Sleep>) {
    match sleep {
        Some(s) => s.await,
        None => std::future::pending().await,
    }
}

impl Drive {
    /// One event; `true` once the session has closed.
    fn on_event(&mut self, event: EngineEvent) -> bool {
        match event {
            EngineEvent::ModelCallStarted { .. } => self.unseen.clear(),
            EngineEvent::ModelCallFinished { usage, .. } => self.call_finished(&usage),
            EngineEvent::JobStarted { job, command, .. } => {
                self.jobs.insert(job, command);
            }
            EngineEvent::JobFinished {
                job,
                exit_code,
                tail,
                ..
            } => self.job_finished(&job, exit_code, &tail),
            EngineEvent::TurnEnded {
                outcome,
                final_text,
                ..
            } => self.turn_ended(outcome, final_text),
            EngineEvent::SessionClosed { reason } => {
                self.finish(error(format!("the helper's session closed: {reason:?}")));
                return true;
            }
            _ => {}
        }
        false
    }

    fn call_finished(&mut self, call: &CallUsage) {
        {
            let mut usage = lock(&self.usage);
            usage.model_calls += 1;
            usage.input_tokens += call.input_tokens;
            usage.output_tokens += call.output_tokens;
            usage.thinking_tokens += call.thinking_tokens;
            usage.cache_read_tokens += call.cache_read_tokens;
            usage.cache_write_tokens += call.cache_write_tokens;
        }
        self.calls += 1;
        if self.calls >= HELPER_MAX_MODEL_CALLS && !self.limited && !self.closing {
            self.limited = true;
            let _ = self.handle.interrupt();
        }
    }

    fn job_finished(&mut self, job: &JobLabel, exit_code: Option<i32>, tail: &str) {
        let command = self.jobs.remove(job).unwrap_or_default();
        if self.closing {
            return;
        }
        self.unseen.push(exit_text(job, &command, exit_code, tail));
        if self.waiting.take().is_some() {
            // Between turns: wake the child with it.
            self.wake();
        }
    }

    fn turn_ended(&mut self, outcome: TurnOutcome, final_text: String) {
        if self.closing {
            return;
        }
        if !final_text.trim().is_empty() {
            self.last_text = final_text;
        }
        if self.limited {
            // However the interrupted turn ended (a call may have failed meanwhile).
            self.finish(self.limit_answer());
            return;
        }
        match outcome {
            TurnOutcome::Completed if !self.unseen.is_empty() => self.wake(),
            TurnOutcome::Completed if !self.jobs.is_empty() => {
                self.waiting = Some(self.clock.sleep(HELPER_JOB_WAIT));
            }
            TurnOutcome::Completed if self.last_text.trim().is_empty() => self.finish(Answer {
                text: "(the helper finished without a final message)".into(),
                is_error: false,
            }),
            TurnOutcome::Completed => self.finish(Answer {
                text: self.last_text.clone(),
                is_error: false,
            }),
            // Only the call limit interrupts a child, and that was handled above.
            TurnOutcome::Interrupted => self.finish(error("the helper was interrupted".into())),
            TurnOutcome::Failed { failure } => {
                let mut text = format!("the helper failed: {}", failure.message);
                if !self.last_text.trim().is_empty() {
                    let _ = write!(text, "\nIts last message:\n{}", self.last_text);
                }
                self.finish(error(text));
            }
        }
    }

    /// A new turn telling the child which jobs exited.
    fn wake(&mut self) {
        let text = std::mem::take(&mut self.unseen).join("\n\n");
        if self.handle.start_turn(TurnInput::text(text)).is_err() {
            self.finish(error("the helper's session ended while it waited".into()));
        }
    }

    fn jobs_outlasted_the_wait(&mut self) {
        let jobs: Vec<&str> = self.jobs.keys().map(JobLabel::as_str).collect();
        let mut text = self.last_text.clone();
        let _ = write!(
            text,
            "\n\n(The helper's background jobs {} were still running after {} minutes and were \
             stopped.)",
            jobs.join(", "),
            HELPER_JOB_WAIT.as_secs() / 60
        );
        self.finish(Answer {
            text: text.trim_start().to_owned(),
            is_error: false,
        });
    }

    fn limit_answer(&self) -> Answer {
        let mut text = format!("the helper was stopped after {HELPER_MAX_MODEL_CALLS} model calls");
        if !self.last_text.trim().is_empty() {
            let _ = write!(text, "; its last message:\n{}", self.last_text);
        }
        error(text)
    }

    /// Sends the answer (once) and closes the child.
    fn finish(&mut self, answer: Answer) {
        if let Some(tx) = self.answer.take() {
            let _ = tx.send(answer);
        }
        self.close();
    }

    /// Forgets the child: it stops its turn and jobs, drops its transcript, then sends `SessionClosed`.
    fn close(&mut self) {
        if !self.closing {
            self.closing = true;
            self.waiting = None;
            let _ = self.handle.forget();
        }
    }
}

fn error(text: String) -> Answer {
    Answer {
        text,
        is_error: true,
    }
}

/// "`cargo test` (J1) exited with code 0." and its last lines.
fn exit_text(job: &JobLabel, command: &str, exit_code: Option<i32>, tail: &str) -> String {
    let how = match exit_code {
        Some(code) => format!("exited with code {code}"),
        None => "was stopped or ended without an exit code".to_owned(),
    };
    let mut text = format!("Background job {} (`{command}`) {how}.", job.as_str());
    let tail = tail.trim_end();
    if !tail.is_empty() {
        let _ = write!(text, "\nLast lines:\n{tail}");
    }
    text
}
