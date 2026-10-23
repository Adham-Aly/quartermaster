//! The scripted engine's `agent_told` jobs.

use super::{Script, ScriptedEngine};
use crate::engine::Engine;
use crate::event::EngineEvent;
use crate::session::TurnInput;
use crate::spec::{AccountRef, SessionSpec, SystemPrompt};
use qm_types::clock::SystemClock;
use qm_types::domain::{Effort, Provider, Role};
use qm_types::ids::{AccountId, AgentId, JobLabel, ModelId};
use std::sync::Arc;
use std::time::Duration;

const SCRIPT: &str = r#"
name = "agent-told"
models = ["scripted-1"]

[[session]]
  [[session.turn]]
    [[session.turn.call]]
    steps = [
      { job = "helper:FAST", command = "FAST helper", finish_after_ms = 50, exit_code = 0, agent_told = true },
      { text = "launched." },
    ]
  [[session.turn]]
    [[session.turn.call]]
    steps = [{ text = "FAST is done." }]
"#;

/// A job the agent was told of, ending between turns, starts the next scripted turn by itself.
#[tokio::test]
async fn an_agent_told_job_ending_between_turns_starts_a_turn_by_itself() {
    let engine = ScriptedEngine::new(Script::from_toml(SCRIPT).unwrap(), Arc::new(SystemClock));
    let spec = SessionSpec::new(
        AgentId::new(5),
        Role::Crewmate,
        AccountRef {
            id: AccountId::new(1),
            provider: Provider::Claude,
            credential: None,
            base_url: None,
        },
        ModelId::new("scripted-1").unwrap(),
        Effort::Low,
        "/unused-test".into(),
        "/unused-test/session".into(),
        SystemPrompt::Replace {
            text: "test".into(),
        },
    );
    let mut handle = engine.open(spec).unwrap();
    handle.start_turn(TurnInput::text("go")).unwrap();
    let mut events = Vec::new();
    let mut ends = 0;
    while ends < 2 {
        let event = tokio::time::timeout(Duration::from_secs(5), handle.next_event())
            .await
            .unwrap_or_else(|_| panic!("timed out; so far: {events:?}"))
            .unwrap();
        match &event {
            EngineEvent::TurnEnded { .. } => ends += 1,
            EngineEvent::TurnStarted
            | EngineEvent::JobFinished { .. }
            | EngineEvent::TextLine { .. } => {}
            _ => continue,
        }
        events.push(event);
    }
    let finished = events
        .iter()
        .position(|e| {
            matches!(e, EngineEvent::JobFinished { job, exit_code: Some(0), agent_told: true, .. }
                if *job == JobLabel::helper("FAST").unwrap())
        })
        .unwrap_or_else(|| panic!("{events:?}"));
    assert_eq!(events[finished + 1], EngineEvent::TurnStarted, "{events:?}");
    assert_eq!(
        events[finished + 2],
        EngineEvent::TextLine {
            text: "FAST is done.".into()
        },
        "{events:?}"
    );
}
