//! The counters behind the orchestrators' limits.
//!
//! The engine layer counts model calls and runs the turn clock itself on both engines, because
//! neither engine's own turn limit fits. The core feeds a turn's events into a [`TurnMeter`] and
//! checks the [`TurnCounters`] against the limits in `qm-policy` (5 minutes per model call with
//! the clock paused while tools run, 20 minutes per turn, 40 model calls, 8,192 output tokens per
//! message with thinking included). Crewmates have none of these limits.

use crate::event::EngineEvent;
use std::time::{Duration, Instant};

pub use qm_types::rules::TurnCounters;

/// Measures one turn from its events. Pure: time comes in as arguments.
#[derive(Clone, Debug)]
pub struct TurnMeter {
    turn_started: Instant,
    model_calls: u32,
    /// Call-clock time banked before the current running stretch.
    call_banked: Duration,
    /// When the current running stretch began; `None` while a tool runs.
    running_since: Option<Instant>,
    tools_running: u32,
    largest_message: u64,
    output_tokens: u64,
}

impl TurnMeter {
    /// The turn starts now; the call clock starts with it.
    pub fn start(now: Instant) -> Self {
        Self {
            turn_started: now,
            model_calls: 0,
            call_banked: Duration::ZERO,
            running_since: Some(now),
            tools_running: 0,
            largest_message: 0,
            output_tokens: 0,
        }
    }

    /// Takes one event into account: model calls start and finish, and our tool calls pause the
    /// call clock. Other events change nothing.
    pub fn observe(&mut self, event: &EngineEvent, now: Instant) {
        match event {
            EngineEvent::ModelCallStarted { .. } => {
                self.model_calls += 1;
                self.call_banked = Duration::ZERO;
                if self.tools_running == 0 {
                    self.running_since = Some(now);
                }
            }
            EngineEvent::ModelCallFinished { usage, .. } => {
                self.largest_message = self.largest_message.max(usage.output_tokens);
                self.output_tokens += usage.output_tokens;
            }
            EngineEvent::ToolCall { .. } => {
                if self.tools_running == 0
                    && let Some(since) = self.running_since.take()
                {
                    self.call_banked += now.saturating_duration_since(since);
                }
                self.tools_running += 1;
            }
            _ => {}
        }
    }

    /// The core sent a tool's result back; the call clock resumes once no tool is running.
    pub fn tool_finished(&mut self, now: Instant) {
        self.tools_running = self.tools_running.saturating_sub(1);
        if self.tools_running == 0 && self.running_since.is_none() {
            self.running_since = Some(now);
        }
    }

    pub fn counters(&self, now: Instant) -> TurnCounters {
        let running = self
            .running_since
            .map_or(Duration::ZERO, |since| now.saturating_duration_since(since));
        TurnCounters {
            model_calls: self.model_calls,
            call_elapsed: self.call_banked + running,
            turn_elapsed: now.saturating_duration_since(self.turn_started),
            largest_message_output_tokens: self.largest_message,
            output_tokens: self.output_tokens,
        }
    }
}
