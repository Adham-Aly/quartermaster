//! The watchdogs.
//!
//! "Crewmate turns have no limits of ours. Two watchdogs with fixed timings catch a crewmate that
//! has gone wrong. The timings aren't settings. Every step goes to the crewmate's supervisor …
//! Secondmates are idle by default, so the watchdogs never apply to a secondmate itself."
//!
//! **Finished without reporting** ([`IdleLadder`]): "If the turn ended with no `report` call, its
//! final text is kept as its report. A clear report wakes the supervisor at once. Otherwise:
//! 10 min: one nudge to the crewmate, once per idle spell. Skipped if its last turn failed at the
//! provider, since the retry handles that. 20 min: wakes its supervisor once, with three options:
//! steer, cancel or discard. 50 min: wakes it once more, if the first wake was ignored (30 minutes
//! with no supervisor action on that task). 80 min: a 'stuck' notice to you, with Continue, Drop
//! task or Discard. That's the last step."
//!
//! **Gone quiet mid-turn** ([`QuietLadder`]): "The turn is still running, but the engine has sent
//! nothing. 10 min: a line in the activity feed. 20 min: wakes its supervisor once. 2 h: a turn
//! running two hours in total wakes it once more. A turn is never interrupted or killed
//! automatically. Only the supervisor decides to interrupt."
//!
//! **Signs of life pause both clocks** ([`SignsOfLife`]): "A running background job (for up to 2
//! hours) · a message waiting to be delivered to the crewmate · a question waiting on you · a
//! declared wait (`declare_wait`, `ask_decision`) · an account cut-off."
//!
//! Both ladders are plain state driven by a [`Clock`]: the core calls `observe` whenever a sign of
//! life changes, `poll` to collect the steps that are due, and sleeps with
//! `clock.sleep_until(ladder.next_deadline(..))` in between.
//!
//! Readings of the rules:
//! - Whether signs of life pause the 2-hour turn total. "Signs of life pause both clocks" is read
//!   as pausing every clock of both watchdogs, the 2-hour total included.
//! - How often the quiet ladder's steps repeat. The activity line comes once per silent spell;
//!   the supervisor wake and the 2-hour wake come once per turn.
//! - Whether the stuck notice also requires the wakes to have been ignored. It doesn't: it comes
//!   after 80 minutes flat.

use qm_types::clock::Clock;
use qm_types::domain::Role;
use std::time::{Duration, Instant};

const MIN: u64 = 60;

/// Idle ladder: one nudge.
pub const IDLE_NUDGE: Duration = Duration::from_secs(10 * MIN);
/// Idle ladder: wake the supervisor.
pub const IDLE_WAKE: Duration = Duration::from_secs(20 * MIN);
/// Idle ladder: wake it once more if the first wake was ignored.
pub const IDLE_WAKE_AGAIN: Duration = Duration::from_secs(50 * MIN);
/// Idle ladder: the stuck notice to you.
pub const IDLE_STUCK: Duration = Duration::from_secs(80 * MIN);
/// Quiet ladder: a line in the activity feed.
pub const QUIET_LINE: Duration = Duration::from_secs(10 * MIN);
/// Quiet ladder: wake the supervisor.
pub const QUIET_WAKE: Duration = Duration::from_secs(20 * MIN);
/// Quiet ladder: a turn running two hours in total.
pub const LONG_TURN: Duration = Duration::from_secs(120 * MIN);
/// A running background job is a sign of life for up to two hours.
pub const JOB_SIGN_OF_LIFE: Duration = Duration::from_secs(120 * MIN);

/// The watchdogs watch crewmates only.
pub const fn applies_to(role: Role) -> bool {
    matches!(role, Role::Crewmate)
}

/// The signs of life the core knows about right now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "four independent facts, any of which pauses the clocks"
)]
pub struct SignsOfLife {
    /// When the most recently started background job that is still running began.
    pub newest_running_job: Option<Instant>,
    /// A message is waiting to be delivered to the crewmate.
    pub message_pending: bool,
    /// A question is waiting on you.
    pub question_waiting: bool,
    /// `declare_wait` or `ask_decision` is in force.
    pub declared_wait: bool,
    /// The crewmate's account is cut off until it resets.
    pub account_cut_off: bool,
}

impl SignsOfLife {
    /// Whether anything pauses the clocks at `now`.
    pub fn alive(&self, now: Instant) -> bool {
        self.message_pending
            || self.question_waiting
            || self.declared_wait
            || self.account_cut_off
            || self
                .newest_running_job
                .is_some_and(|start| now.saturating_duration_since(start) < JOB_SIGN_OF_LIFE)
    }

    /// When the job's sign of life runs out, if that is still ahead.
    fn job_expiry(&self, now: Instant) -> Option<Instant> {
        self.newest_running_job
            .map(|s| s + JOB_SIGN_OF_LIFE)
            .filter(|e| *e > now)
    }
}

/// A clock that pauses while there are signs of life.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PausableClock {
    started: Instant,
    paused_total: Duration,
    paused_at: Option<Instant>,
}

impl PausableClock {
    fn new(now: Instant) -> Self {
        Self {
            started: now,
            paused_total: Duration::ZERO,
            paused_at: None,
        }
    }

    fn elapsed(&self, now: Instant) -> Duration {
        let end = self.paused_at.unwrap_or(now);
        end.saturating_duration_since(self.started)
            .saturating_sub(self.paused_total)
    }

    fn set_paused(&mut self, paused: bool, now: Instant) {
        match (paused, self.paused_at) {
            (true, None) => self.paused_at = Some(now),
            (false, Some(at)) => {
                self.paused_total += now.saturating_duration_since(at);
                self.paused_at = None;
            }
            _ => {}
        }
    }

    /// When `elapsed` reaches `at`, if running; `None` while paused.
    fn deadline(&self, at: Duration, now: Instant) -> Option<Instant> {
        if self.paused_at.is_some() {
            return None;
        }
        let left = at.saturating_sub(self.elapsed(now));
        Some(now + left)
    }
}

fn earliest(a: Option<Instant>, b: Option<Instant>) -> Option<Instant> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (x, None) => x,
        (None, y) => y,
    }
}

/// A step of the idle ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdleStep {
    /// One nudge to the crewmate.
    Nudge,
    /// Wake the supervisor with steer, cancel or discard.
    WakeSupervisor,
    /// Wake it once more: the first wake was ignored.
    WakeSupervisorAgain,
    /// The stuck notice to you (Continue, Drop task, Discard; see `holds::stuck_choices`). The
    /// last step.
    StuckNotice,
}

/// "Finished without reporting": one idle spell, from the end of a turn with no `report` call
/// until the crewmate's next turn starts (then a new ladder). A clear report starts no ladder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IdleLadder {
    clock: PausableClock,
    skip_nudge: bool,
    supervisor_acted: bool,
    /// The next step to consider; steps pass strictly in order.
    stage: IdleStage,
}

/// How far the idle ladder has got: the next step not yet passed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum IdleStage {
    Nudge,
    Wake,
    WakeAgain,
    Stuck,
    Done,
}

impl IdleStage {
    const fn at(self) -> Option<Duration> {
        match self {
            Self::Nudge => Some(IDLE_NUDGE),
            Self::Wake => Some(IDLE_WAKE),
            Self::WakeAgain => Some(IDLE_WAKE_AGAIN),
            Self::Stuck => Some(IDLE_STUCK),
            Self::Done => None,
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Nudge => Self::Wake,
            Self::Wake => Self::WakeAgain,
            Self::WakeAgain => Self::Stuck,
            Self::Stuck | Self::Done => Self::Done,
        }
    }
}

impl IdleLadder {
    /// Starts when the turn ends without a `report` call. `last_turn_failed_at_provider` skips the
    /// nudge, "since the retry handles that".
    pub fn start(clock: &dyn Clock, last_turn_failed_at_provider: bool) -> Self {
        Self {
            clock: PausableClock::new(clock.instant()),
            skip_nudge: last_turn_failed_at_provider,
            supervisor_acted: false,
            stage: IdleStage::Nudge,
        }
    }

    /// Updates the pause from the current signs of life.
    pub fn observe(&mut self, clock: &dyn Clock, signs: &SignsOfLife) {
        let now = clock.instant();
        self.clock.set_paused(signs.alive(now), now);
    }

    /// The supervisor acted on this task after the first wake, so the second wake is skipped.
    pub fn supervisor_acted(&mut self) {
        if self.stage > IdleStage::Wake {
            self.supervisor_acted = true;
        }
    }

    /// The steps due now, in order; each fires at most once.
    pub fn poll(&mut self, clock: &dyn Clock, signs: &SignsOfLife) -> Vec<IdleStep> {
        self.observe(clock, signs);
        let elapsed = self.clock.elapsed(clock.instant());
        let mut due = Vec::new();
        while let Some(at) = self.stage.at()
            && elapsed >= at
        {
            let step = match self.stage {
                IdleStage::Nudge if self.skip_nudge => None,
                IdleStage::Nudge => Some(IdleStep::Nudge),
                IdleStage::Wake => Some(IdleStep::WakeSupervisor),
                IdleStage::WakeAgain if self.supervisor_acted => None,
                IdleStage::WakeAgain => Some(IdleStep::WakeSupervisorAgain),
                IdleStage::Stuck => Some(IdleStep::StuckNotice),
                IdleStage::Done => None,
            };
            due.extend(step);
            self.stage = self.stage.next();
        }
        due
    }

    /// When to poll next: the next step's deadline, or when a job's sign of life runs out.
    /// `None` when the ladder is finished, or paused with nothing that ends the pause on time.
    pub fn next_deadline(&self, clock: &dyn Clock, signs: &SignsOfLife) -> Option<Instant> {
        let now = clock.instant();
        let next = self.stage.at()?;
        let step = if signs.alive(now) {
            None
        } else {
            self.clock.deadline(next, now)
        };
        earliest(step, signs.job_expiry(now))
    }

    pub fn finished(&self) -> bool {
        self.stage == IdleStage::Done
    }
}

/// A step of the quiet ladder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuietStep {
    /// A line in the activity feed.
    ActivityLine,
    /// Wake the supervisor once.
    WakeSupervisor,
    /// The turn has run two hours in total: wake it once more.
    LongTurn,
}

/// "Gone quiet mid-turn": one running turn. Never interrupts anything; it only reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuietLadder {
    silence: PausableClock,
    turn: PausableClock,
    line_this_spell: bool,
    woke: bool,
    long_turn: bool,
}

impl QuietLadder {
    /// Starts when the turn starts.
    pub fn start(clock: &dyn Clock) -> Self {
        let now = clock.instant();
        Self {
            silence: PausableClock::new(now),
            turn: PausableClock::new(now),
            line_this_spell: false,
            woke: false,
            long_turn: false,
        }
    }

    /// The engine sent something (text, a tool call, `Activity`, …): the silence starts over.
    pub fn on_engine_event(&mut self, clock: &dyn Clock) {
        let now = clock.instant();
        let paused = self.silence.paused_at.is_some();
        self.silence = PausableClock::new(now);
        self.silence.set_paused(paused, now);
        self.line_this_spell = false;
    }

    /// Updates the pause from the current signs of life.
    pub fn observe(&mut self, clock: &dyn Clock, signs: &SignsOfLife) {
        let now = clock.instant();
        let alive = signs.alive(now);
        self.silence.set_paused(alive, now);
        self.turn.set_paused(alive, now);
    }

    /// The steps due now, in order.
    pub fn poll(&mut self, clock: &dyn Clock, signs: &SignsOfLife) -> Vec<QuietStep> {
        self.observe(clock, signs);
        let now = clock.instant();
        let quiet = self.silence.elapsed(now);
        let mut due = Vec::new();
        if !self.line_this_spell && quiet >= QUIET_LINE {
            self.line_this_spell = true;
            due.push(QuietStep::ActivityLine);
        }
        if !self.woke && quiet >= QUIET_WAKE {
            self.woke = true;
            due.push(QuietStep::WakeSupervisor);
        }
        if !self.long_turn && self.turn.elapsed(now) >= LONG_TURN {
            self.long_turn = true;
            due.push(QuietStep::LongTurn);
        }
        due
    }

    /// When to poll next (see [`IdleLadder::next_deadline`]).
    pub fn next_deadline(&self, clock: &dyn Clock, signs: &SignsOfLife) -> Option<Instant> {
        let now = clock.instant();
        let job = signs.job_expiry(now);
        if signs.alive(now) {
            return job;
        }
        let line = (!self.line_this_spell)
            .then(|| self.silence.deadline(QUIET_LINE, now))
            .flatten();
        let wake = (!self.woke)
            .then(|| self.silence.deadline(QUIET_WAKE, now))
            .flatten();
        let long = (!self.long_turn)
            .then(|| self.turn.deadline(LONG_TURN, now))
            .flatten();
        earliest(earliest(earliest(line, wake), long), job)
    }
}
