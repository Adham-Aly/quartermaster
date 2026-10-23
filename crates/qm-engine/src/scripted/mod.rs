//! The scripted engine.
//!
//! A fake model that plays back scripted responses (text, tool calls, thinking, built-in tool use,
//! stalls, jobs, provider errors with retries, account limits, crashes with resume, end of
//! conversation and compaction) through the real engine layer, so whole flows run
//! deterministically, offline, in seconds: a task from `start_crewmate` to merged and closed, a
//! review round, a held decision, a watchdog escalation. It implements [`crate::Engine`] with
//! `EngineKind::Scripted` and is compiled into the binary so the developer seat and `quartermaster
//! dev session` can drive the real `quartermaster` with it.
//!
//! Its sessions speak the same events as the real engines, in the same order. All waiting goes
//! through the given [`Clock`]. The same script and the same inputs give the same event stream
//! every time.
//!
//! The format is in `script`. Sessions take the script's `[[session]]` entries in order of
//! `open`, each the first unused entry whose filters (`role`, `agent`, `model`) match. A session's
//! reference is `scripted-<agent>-<n>`, `n` the entry's number from 1; resuming it (a later
//! `open` with that reference) continues the same entry at its next turn, or closes with
//! `ResumeFailed` when the entry says `resumable = false`.

mod script;
mod session;
#[cfg(test)]
mod tests;

pub use script::{Script, ScriptError};

use crate::engine::{DetectedModel, Engine, EngineError, UsageReading};
use crate::session::{SessionHandle, session_channel};
use crate::spec::{AccountRef, SessionSpec};
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::domain::EngineKind;
use qm_types::ids::ModelId;
use std::fmt;
use std::sync::{Arc, Mutex};

/// The scripted engine.
pub struct ScriptedEngine {
    script: Arc<Script>,
    clock: Arc<dyn Clock>,
    state: Arc<Mutex<Progress>>,
}

/// Which script sessions are taken, and each one's next turn (shared with resumed sessions).
#[derive(Debug)]
pub(crate) struct Progress {
    pub(crate) taken: Vec<bool>,
    pub(crate) next_turn: Vec<usize>,
}

impl ScriptedEngine {
    pub fn new(script: Script, clock: Arc<dyn Clock>) -> Self {
        let n = script.sessions.len();
        Self {
            script: Arc::new(script),
            clock,
            state: Arc::new(Mutex::new(Progress {
                taken: vec![false; n],
                next_turn: vec![0; n],
            })),
        }
    }
}

impl fmt::Debug for ScriptedEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ScriptedEngine")
            .field("script", &self.script.name)
            .finish_non_exhaustive()
    }
}

impl Engine for ScriptedEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Scripted
    }

    fn open(&self, spec: SessionSpec) -> Result<SessionHandle, EngineError> {
        let agent = spec.agent.to_string();
        let index = if let Some(reference) = &spec.resume {
            resumed_index(reference.as_str(), &agent).filter(|&i| i < self.script.sessions.len())
        } else {
            let mut progress = lock(&self.state);
            let found = self.script.sessions.iter().enumerate().position(|(i, s)| {
                !progress.taken[i]
                    && s.role.is_none_or(|r| r == spec.role)
                    && s.agent.as_deref().is_none_or(|a| a == agent)
                    && s.model.as_deref().is_none_or(|m| m == spec.model.as_str())
            });
            let Some(i) = found else {
                return Err(EngineError::Unsupported(format!(
                    "the script {} has no session left for {agent}",
                    self.script.name
                )));
            };
            progress.taken[i] = true;
            Some(i)
        };
        let (handle, side) = session_channel();
        let session = session::Session::new(
            side,
            spec,
            Arc::clone(&self.script),
            index,
            Arc::clone(&self.clock),
            Arc::clone(&self.state),
        );
        tokio::spawn(session.run());
        Ok(handle)
    }

    fn models<'a>(
        &'a self,
        _account: &'a AccountRef,
    ) -> BoxFuture<'a, Result<Vec<DetectedModel>, EngineError>> {
        Box::pin(async move {
            self.script
                .models
                .iter()
                .map(|m| {
                    Ok(DetectedModel {
                        id: ModelId::new(m.clone()).map_err(|e| {
                            EngineError::Unsupported(format!("the script's model {m:?}: {e}"))
                        })?,
                        display_name: None,
                        efforts: Vec::new(),
                        image_input: Some(false),
                        context_window: None,
                    })
                })
                .collect()
        })
    }

    fn usage<'a>(
        &'a self,
        _account: &'a AccountRef,
    ) -> BoxFuture<'a, Result<Option<UsageReading>, EngineError>> {
        Box::pin(async { Ok(None) })
    }
}

/// The script entry a `scripted-<agent>-<n>` reference names.
fn resumed_index(reference: &str, agent: &str) -> Option<usize> {
    let n: usize = reference
        .strip_prefix("scripted-")?
        .strip_prefix(agent)?
        .strip_prefix('-')?
        .parse()
        .ok()?;
    n.checked_sub(1)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}
