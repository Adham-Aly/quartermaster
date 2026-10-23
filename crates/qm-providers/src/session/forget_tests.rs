//! Terminal histories must go away, but an ordinary model/effort reopen can still resume, and an
//! abandoned session's transcript file stays for the next run.
use super::*;
use crate::engine::{ProviderEngine, ProviderEngineConfig};
use qm_auth::{
    AuthError, Credential, CredentialRef, CredentialStore, LoginEndpoints, LoginHealth, TokenKeeper,
};
use qm_engine::{AccountRef, Engine, supervisor::OsSupervisor};
use qm_types::{
    clock::SystemClock,
    config::Config,
    domain::{Effort, Provider, Role},
    ids::{AccountId, AgentId},
};
use std::collections::BTreeMap;

#[derive(Debug)]
struct NoCredentials;
impl CredentialStore for NoCredentials {
    fn get(&self, _: &CredentialRef) -> Result<Option<Credential>, AuthError> {
        Ok(None)
    }
    fn put(&self, _: &CredentialRef, _: Credential) -> Result<(), AuthError> {
        panic!("test must not save credentials")
    }
    fn remove(&self, _: &CredentialRef) -> Result<(), AuthError> {
        panic!("test must not remove credentials")
    }
}
fn engine() -> ProviderEngine {
    let (_, settings) = tokio::sync::watch::channel(Config::default());
    ProviderEngine::new(ProviderEngineConfig {
        tokens: Arc::new(TokenKeeper::new(
            Arc::new(NoCredentials),
            LoginEndpoints::default(),
            Arc::new(LoginHealth::new()),
        )),
        supervisor: Arc::new(OsSupervisor::new(None)),
        clock: Arc::new(SystemClock),
        models: Arc::new(crate::model::BuiltinProfiles),
        builtins: None,
        inherited_env: BTreeMap::new(),
        settings,
        user_agent: "quartermaster-test".into(),
    })
}
fn spec() -> SessionSpec {
    SessionSpec::new(
        AgentId::new(1),
        Role::Crewmate,
        AccountRef {
            id: AccountId::new(1),
            provider: Provider::ChatGpt,
            credential: None,
            base_url: None,
        },
        ModelId::new("gpt-test").unwrap(),
        Effort::Medium,
        "/unused-test".into(),
        "/unused-test/session".into(),
        SystemPrompt::Replace {
            text: "test".into(),
        },
    )
}
async fn ready(handle: &mut qm_engine::SessionHandle) -> SessionRef {
    match handle.next_event().await.unwrap() {
        EngineEvent::SessionReady { session, .. } => session,
        other => panic!("expected ready, got {other:?}"),
    }
}
async fn closed(handle: &mut qm_engine::SessionHandle) {
    while let Some(event) = handle.next_event().await {
        if matches!(event, EngineEvent::SessionClosed { .. }) {
            return;
        }
    }
    panic!("no close event");
}

#[tokio::test]
async fn terminal_forget_erases_history_before_closed_and_prevents_resume() {
    let engine = engine();
    let mut handle = engine.open(spec()).unwrap();
    let id = ready(&mut handle).await;
    assert!(SessionStore::resume(id.as_str(), None).is_ok());
    handle.forget().unwrap();
    closed(&mut handle).await;
    assert!(SessionStore::resume(id.as_str(), None).is_err());
    let mut resume = spec();
    resume.resume = Some(id);
    let mut handle = engine.open(resume).unwrap();
    assert!(matches!(
        handle.next_event().await,
        Some(EngineEvent::SessionClosed {
            reason: CloseReason::ResumeFailed { .. }
        })
    ));
}

#[tokio::test]
async fn ordinary_close_remains_resumable_but_abandoning_the_handle_lets_go_of_memory() {
    let engine = engine();
    let mut handle = engine.open(spec()).unwrap();
    let id = ready(&mut handle).await;
    handle.close().unwrap();
    closed(&mut handle).await;
    assert!(SessionStore::resume(id.as_str(), None).is_ok());
    let mut resume = spec();
    resume.resume = Some(id.clone());
    let mut handle = engine.open(resume).unwrap();
    ready(&mut handle).await;
    drop(handle);
    // No timer or external state: let the session observe its disconnected command channel.
    for _ in 0..1000 {
        if SessionStore::resume(id.as_str(), None).is_err() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("abandoned session still holds its history");
}

#[tokio::test]
async fn an_abandoned_session_keeps_its_transcript_file_and_forget_deletes_it() {
    let dir = std::env::temp_dir().join(format!("qm-forget-disk-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let engine = engine();
    let mut handle = engine
        .open(spec().with_transcript_dir(dir.clone()))
        .unwrap();
    let id = ready(&mut handle).await;
    let file = session_store::transcript_path(&dir, id.as_str()).unwrap();
    assert!(
        file.is_file(),
        "the header is on disk once the session is ready"
    );
    drop(handle);
    for _ in 0..1000 {
        if SessionStore::resume(id.as_str(), None).is_err() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(
        SessionStore::resume(id.as_str(), None).is_err(),
        "memory let go"
    );
    assert!(file.is_file(), "an abandoned session keeps its file");

    let mut resume = spec().with_transcript_dir(dir.clone());
    resume.resume = Some(id.clone());
    let mut handle = engine.open(resume).unwrap();
    assert_eq!(ready(&mut handle).await, id, "it resumes from disk");
    handle.forget().unwrap();
    closed(&mut handle).await;
    assert!(!file.exists(), "forget deletes the file");
    let _ = std::fs::remove_dir_all(&dir);
}
