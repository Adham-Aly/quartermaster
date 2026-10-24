//! Each agent's context occupancy, current turn and compactions, held in memory for the UI.
//!
//! [`ContextMonitor`] watches every session through a passive tap on its handle
//! ([`qm_engine::SessionHandle::observed`]). It writes no messages, events or rows and never
//! touches a turn loop, so nothing it holds can reach a model or change when anything runs.
//! [`ContextPart`] puts its view into every snapshot.

use crate::api::{ApiPart, Handled};
use jiff::Timestamp;
use qm_engine::{
    AutocompactPoint, CallUsage, ContextBasis, ContextReading, EngineEvent, EventTap,
    ReportedUsage, TurnOutcome, TurnUsage,
};
use qm_types::BoxFuture;
use qm_types::api::{
    AgentContextView, AgentRef, Command, CompactingView, CompactionNote, ContextBasisView,
    ContextOccupancy, NoteOutcome, PointView, Snapshot, TurnTokens,
};
use qm_types::clock::Clock;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// How many finished compactions each agent keeps for the chat.
const NOTES_KEPT: usize = 3;

/// Every agent's context, in memory.
pub struct ContextMonitor {
    inner: Mutex<HashMap<AgentRef, AgentContext>>,
    clock: Arc<dyn Clock>,
    on_change: OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

impl fmt::Debug for ContextMonitor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContextMonitor").finish_non_exhaustive()
    }
}

impl ContextMonitor {
    pub fn new(clock: Arc<dyn Clock>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(HashMap::new()),
            clock,
            on_change: OnceLock::new(),
        })
    }

    /// Sets what runs (outside the lock) whenever an agent's view changed: the UI's rebuild
    /// signal. Set once, after the conversation exists; changes before that just wait for the
    /// next snapshot.
    pub fn notify_with(&self, on_change: Arc<dyn Fn() + Send + Sync>) {
        if self.on_change.set(on_change).is_err() {
            tracing::warn!("the context monitor's change signal was set twice; the first stays");
        }
    }

    /// The tap for `agent`'s session handle.
    pub fn tap(self: &Arc<Self>, agent: AgentRef) -> EventTap {
        let monitor = Arc::clone(self);
        Arc::new(move |event| monitor.observe(agent, event))
    }

    fn observe(&self, agent: AgentRef, event: &EngineEvent) {
        let now = self.clock.now();
        let changed = {
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            apply(inner.entry(agent).or_default(), event, now)
        };
        if changed && let Some(on_change) = self.on_change.get() {
            on_change();
        }
    }

    /// Every agent's view; agents never seen are left out (the UI says "no reading").
    pub fn view(&self) -> Vec<AgentContextView> {
        let inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.iter().map(|(agent, c)| c.view(*agent)).collect()
    }

    /// Forgets the agents `keep` says no to.
    pub fn retain(&self, keep: impl Fn(&AgentRef) -> bool) {
        let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        inner.retain(|agent, _| keep(agent));
    }
}

/// One agent's context as the monitor last saw it.
#[derive(Debug, Default)]
struct AgentContext {
    reading: Option<ContextReading>,
    observed_at: Option<Timestamp>,
    after_compaction: bool,
    /// Why `reading`'s tokens are unknown, when the monitor knows better than the reading.
    unknown_why: Option<String>,
    stale: Option<String>,
    live: Option<LiveTurn>,
    last: Option<(TurnUsage, Timestamp)>,
    compacting: Option<CompactingView>,
    notes: VecDeque<CompactionNote>,
}

#[derive(Debug)]
struct LiveTurn {
    started: Timestamp,
    sums: CallUsage,
    calls: u32,
}

impl AgentContext {
    fn tokens(&self) -> Option<u64> {
        self.reading.as_ref().and_then(|r| r.tokens)
    }

    fn reports(&self) -> ReportedUsage {
        self.reading
            .as_ref()
            .map_or(ReportedUsage::NONE, |r| r.reports)
    }

    fn end_compacting(&mut self) -> bool {
        self.compacting.take().is_some()
    }

    fn note(&mut self, note: CompactionNote) {
        if self.notes.len() == NOTES_KEPT {
            self.notes.pop_front();
        }
        self.notes.push_back(note);
    }

    fn took_ms(&self, now: Timestamp) -> Option<u64> {
        self.compacting
            .map(|c| u64::try_from(now.duration_since(c.since).as_millis()).unwrap_or(0))
    }

    /// A compaction ended well: note it, and show the engine's after-size as an estimate until
    /// the next model call replaces it.
    fn compacted(
        &mut self,
        automatic: bool,
        before: Option<u64>,
        after: Option<u64>,
        now: Timestamp,
    ) {
        let before = before.or(self.compacting.and_then(|k| k.was));
        let note = CompactionNote::new(now, automatic, NoteOutcome::Done)
            .with_sizes(before, after, true)
            .with_took_ms(self.took_ms(now));
        self.note(note);
        self.end_compacting();
        if let Some(r) = &mut self.reading {
            r.tokens = after;
            r.estimate = true;
        }
        self.observed_at = Some(now);
        self.after_compaction = true;
        self.stale = None;
        self.unknown_why = after
            .is_none()
            .then(|| "compacted; the size shows after the next model call".to_owned());
    }

    fn view(&self, agent: AgentRef) -> AgentContextView {
        let mut occ = ContextOccupancy::default();
        if let Some(r) = &self.reading {
            occ.tokens = r.tokens;
            occ.estimate = r.estimate;
            occ.point = match &r.autocompact {
                AutocompactPoint::At { tokens } => PointView::At(*tokens),
                AutocompactPoint::Off => PointView::Off,
                AutocompactPoint::Unknown { why } => PointView::Unknown(why.clone()),
            };
            occ.window = r.window;
            occ.model.clone_from(&r.model);
            occ.basis = if self.after_compaction {
                ContextBasisView::AfterCompaction
            } else {
                match r.basis {
                    ContextBasis::LastCall => ContextBasisView::LastCall,
                    ContextBasis::SessionStart => ContextBasisView::SessionStart,
                }
            };
            occ.observed_at = self.observed_at;
            if r.tokens.is_none() {
                occ.unknown_why = Some(
                    self.unknown_why
                        .clone()
                        .unwrap_or_else(|| "waiting for the first model call".to_owned()),
                );
            }
        } else {
            occ.unknown_why = Some(
                self.unknown_why
                    .clone()
                    .unwrap_or_else(|| "no reading since the app started".to_owned()),
            );
        }
        occ.stale.clone_from(&self.stale);
        AgentContextView::new(agent, occ)
            .with_turn(self.turn())
            .with_compacting(self.compacting)
            .with_notes(self.notes.iter().cloned().collect())
    }

    fn turn(&self) -> Option<TurnTokens> {
        let reports = self.reports();
        let category = |reported: bool, n: u64| (reported || n > 0).then_some(n);
        if let Some(live) = &self.live {
            let s = &live.sums;
            return Some(
                TurnTokens::new(true, s.input_tokens, s.output_tokens, live.calls)
                    .with_categories(
                        category(reports.thinking, s.thinking_tokens),
                        category(reports.cache_read, s.cache_read_tokens),
                        category(reports.cache_write, s.cache_write_tokens),
                    )
                    .with_at(Some(live.started)),
            );
        }
        let (u, at) = self.last.as_ref()?;
        Some(
            TurnTokens::new(false, u.input_tokens, u.output_tokens, u.model_calls)
                .with_categories(
                    category(reports.thinking, u.thinking_tokens),
                    category(reports.cache_read, u.cache_read_tokens),
                    category(reports.cache_write, u.cache_write_tokens),
                )
                .with_cost(u.cost_usd.map(|c| format!("${c:.2}")))
                .with_at(Some(*at)),
        )
    }
}

/// Applies one event; true when the view changed.
fn apply(c: &mut AgentContext, event: &EngineEvent, now: Timestamp) -> bool {
    match event {
        EngineEvent::SessionReady { resumed, .. } => {
            c.stale = None;
            if !resumed {
                c.reading = None;
                c.observed_at = None;
                c.after_compaction = false;
                c.unknown_why = Some("new session; waiting for its first reading".to_owned());
            }
            c.end_compacting();
            true
        }
        EngineEvent::ContextObserved { reading } => {
            c.reading = Some(reading.clone());
            c.observed_at = Some(now);
            c.after_compaction = false;
            c.unknown_why = None;
            c.stale = None;
            // A fresh reading means any compaction we saw start is over, even unreported.
            c.end_compacting();
            true
        }
        EngineEvent::TurnStarted => {
            c.live = Some(LiveTurn {
                started: now,
                sums: CallUsage::default(),
                calls: 0,
            });
            true
        }
        EngineEvent::ModelCallFinished { usage, .. } => {
            let Some(live) = &mut c.live else {
                return false;
            };
            let s = &mut live.sums;
            s.input_tokens += usage.input_tokens;
            s.output_tokens += usage.output_tokens;
            s.thinking_tokens += usage.thinking_tokens;
            s.cache_read_tokens += usage.cache_read_tokens;
            s.cache_write_tokens += usage.cache_write_tokens;
            live.calls += 1;
            true
        }
        EngineEvent::TurnEnded { outcome, usage, .. } => {
            c.live = None;
            // A `/compact` turn makes no model calls: keep the last real turn.
            if usage.model_calls > 0 {
                c.last = Some((*usage, now));
            }
            if let Some(compacting) = c.compacting {
                if let TurnOutcome::Failed { failure } = outcome {
                    let note = CompactionNote::new(
                        now,
                        compacting.automatic,
                        NoteOutcome::Failed(failure.message.clone()),
                    )
                    .with_took_ms(c.took_ms(now));
                    c.note(note);
                }
                c.end_compacting();
            }
            true
        }
        EngineEvent::CompactionStarted { automatic } => {
            c.compacting = Some(CompactingView::new(*automatic, now, c.tokens()));
            true
        }
        EngineEvent::Compacted {
            automatic,
            tokens_before,
            tokens_after,
        } => {
            c.compacted(*automatic, *tokens_before, *tokens_after, now);
            true
        }
        EngineEvent::CompactionFailed { automatic, message } => {
            let note = CompactionNote::new(now, *automatic, NoteOutcome::Failed(message.clone()))
                .with_took_ms(c.took_ms(now));
            c.note(note);
            c.end_compacting();
            true
        }
        EngineEvent::SessionClosed { .. } => {
            c.stale = Some("session closed".to_owned());
            c.live = None;
            c.end_compacting();
            true
        }
        _ => false,
    }
}

/// Puts the monitor's view into every snapshot. Register it after the crew and secondmates parts
/// (it reads the crew list they fill).
pub struct ContextPart {
    monitor: Arc<ContextMonitor>,
}

impl ContextPart {
    pub fn new(monitor: Arc<ContextMonitor>) -> Self {
        Self { monitor }
    }
}

impl fmt::Debug for ContextPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContextPart").finish_non_exhaustive()
    }
}

impl ApiPart for ContextPart {
    fn handle<'a>(&'a self, _command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        None
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let shown: Vec<AgentRef> = snapshot.crew.iter().map(|row| row.agent).collect();
            // An empty crew means it couldn't be read: keep everything rather than forget.
            if !shown.is_empty() {
                self.monitor.retain(|agent| {
                    matches!(agent, AgentRef::Quartermaster) || shown.contains(agent)
                });
            }
            snapshot.context = self.monitor.view();
        })
    }
}
