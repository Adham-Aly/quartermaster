//! Terminal histories must go away, but an ordinary model/effort reopen can still resume.
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
    assert!(SessionStore::resume(id.as_str()).is_ok());
    handle.forget().unwrap();
    closed(&mut handle).await;
    assert!(SessionStore::resume(id.as_str()).is_err());
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
async fn ordinary_close_remains_resumable_but_abandoning_the_handle_forgets() {
    let engine = engine();
    let mut handle = engine.open(spec()).unwrap();
    let id = ready(&mut handle).await;
    handle.close().unwrap();
    closed(&mut handle).await;
    assert!(SessionStore::resume(id.as_str()).is_ok());
    let mut resume = spec();
    resume.resume = Some(id.clone());
    let mut handle = engine.open(resume).unwrap();
    ready(&mut handle).await;
    drop(handle);
    // No timer or external state: let the session observe its disconnected command channel.
    for _ in 0..1000 {
        if SessionStore::resume(id.as_str()).is_err() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("abandoned session still holds its history");
}
