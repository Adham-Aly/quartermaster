//! The provider engine: `qm_engine::Engine` for every account that isn't a Claude subscription.
//!
//! There is no process per agent: a session is a Tokio task (`crate::session`) that talks
//! HTTP to the provider, runs the built-in file tools in-process and keeps its transcript in a
//! JSONL file in the agent's `session/` folder. So `open` answers at once with `SessionReady`,
//! the engine never reports `Suspended` and `idle_shutdown` doesn't apply.
//!
//! **Keys the engine owns** (a `spec.env` naming one, or one of
//! `qm_types::env::NEVER_PASSED_TO_AGENTS`, is refused as unsupported, except a key the engine sets
//! named with the value it sets, as the push fence's `GIT_TERMINAL_PROMPT=0`): the environment of
//! the only processes it starts, the built-in `bash` tool's, built from scratch: `PATH`, `HOME`,
//! `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `LANG`, `LC_ALL`, `LC_CTYPE` copied from the app's own
//! environment at startup, and `GIT_TERMINAL_PROMPT=0`, `PAGER=cat`, `GIT_PAGER=cat`; then
//! `spec.env`. No provider key or token ever reaches it.

use crate::adapters::{self, CredentialKind, WireApi};
use crate::builtin::{BuiltinCx, BuiltinTools, BuiltinToolsFactory};
use crate::helpers::HelperTools;
use crate::http::HttpClient;
use crate::model::ModelProfiles;
use crate::session;
use jiff::Timestamp;
use qm_auth::{AuthError, Credential, CredentialRef, Secret, TokenKeeper};
use qm_engine::supervisor::Supervisor;
use qm_engine::{
    AccountRef, Builtins, DetectedModel, Engine, EngineError, SessionHandle, SessionSpec,
    SystemPrompt, UsageReading, UsageWindow, session_channel,
};
use qm_types::BoxFuture;
use qm_types::clock::Clock;
use qm_types::config::Config;
use qm_types::domain::{EngineKind, Provider};
use qm_types::env::NEVER_PASSED_TO_AGENTS;
use qm_types::ids::AccountId;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::watch;

/// Copied from the app's environment into `bash`'s, when present there.
pub const COPIED_ENV_KEYS: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE",
];
/// Set by the engine itself in `bash`'s environment.
pub const SET_ENV: &[(&str, &str)] = &[
    ("GIT_TERMINAL_PROMPT", "0"),
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
];

/// Everything the provider engine is built from.
pub struct ProviderEngineConfig {
    /// The stored logins, refreshed before they expire and saved in place, for every OAuth account
    /// (ChatGPT, Copilot, Kimi, xAI); it records logins that stopped working.
    pub tokens: Arc<TokenKeeper>,
    /// Handed to the built-in tools for `bash` jobs.
    pub supervisor: Arc<dyn Supervisor>,
    pub clock: Arc<dyn Clock>,
    /// Model profiles: the catalog in the app.
    pub models: Arc<dyn ModelProfiles>,
    /// `None`: specs asking for the file tools are refused as unsupported.
    pub builtins: Option<Arc<dyn BuiltinToolsFactory>>,
    /// The app's environment at startup.
    pub inherited_env: BTreeMap<String, String>,
    /// The app's settings, as they change: the autocompact window is read at every compaction
    /// check, so a change applies at the next check.
    pub settings: watch::Receiver<Config>,
    /// `quartermaster/<version>`.
    pub user_agent: String,
}

impl fmt::Debug for ProviderEngineConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderEngineConfig")
            .field("tokens", &self.tokens)
            .field("models", &self.models)
            .field("builtins", &self.builtins.is_some())
            .field("user_agent", &self.user_agent)
            .finish_non_exhaustive()
    }
}

/// The provider engine.
#[derive(Debug)]
pub struct ProviderEngine {
    shared: Arc<Shared>,
}

/// What every session of one engine shares.
pub(crate) struct Shared {
    pub(crate) tokens: Arc<TokenKeeper>,
    pub(crate) supervisor: Arc<dyn Supervisor>,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) models: Arc<dyn ModelProfiles>,
    pub(crate) builtins: Option<Arc<dyn BuiltinToolsFactory>>,
    pub(crate) inherited_env: BTreeMap<String, String>,
    pub(crate) settings: watch::Receiver<Config>,
    pub(crate) user_agent: String,
    pub(crate) http: HttpClient,
    /// The latest usage headers per account, for `Engine::usage`.
    usage: Mutex<HashMap<AccountId, UsageReading>>,
}

impl fmt::Debug for Shared {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared")
            .field("models", &self.models)
            .field("user_agent", &self.user_agent)
            .finish_non_exhaustive()
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    // Plain data behind these locks; a panic elsewhere leaves nothing half-done.
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl ProviderEngine {
    pub fn new(config: ProviderEngineConfig) -> Self {
        let http = HttpClient::new(&config.user_agent);
        Self {
            shared: Arc::new(Shared {
                tokens: config.tokens,
                supervisor: config.supervisor,
                clock: config.clock,
                models: config.models,
                builtins: config.builtins,
                inherited_env: config.inherited_env,
                settings: config.settings,
                user_agent: config.user_agent,
                http,
                usage: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Checks a spec against what this engine can run.
    fn check_spec(&self, spec: &SessionSpec) -> Result<(), EngineError> {
        if let SystemPrompt::AppendToClaudeCode { .. } = spec.prompt {
            return Err(EngineError::Unsupported(
                "the provider engine takes a whole system prompt; Claude Code's preset exists \
                 only on the Claude engine"
                    .into(),
            ));
        }
        match spec.builtins {
            Builtins::ClaudeCode => {
                return Err(EngineError::Unsupported(
                    "Claude Code's built-in tools exist only on the Claude engine".into(),
                ));
            }
            Builtins::ProviderFileTools { .. } if self.shared.builtins.is_none() => {
                return Err(EngineError::Unsupported(
                    "this build has no file tools for the provider engine".into(),
                ));
            }
            _ => {}
        }
        if !adapters::provider_supported(spec.account.provider) {
            return Err(EngineError::Unsupported(format!(
                "the provider engine can't serve {} accounts yet",
                spec.account.provider
            )));
        }
        for (key, value) in &spec.env {
            // A key the engine sets itself may be named with the same value (the crewmate's push
            // fence sets GIT_TERMINAL_PROMPT=0 on every engine).
            let owned = COPIED_ENV_KEYS.contains(&key.as_str())
                || SET_ENV.iter().any(|(k, v)| k == key && v != value)
                || NEVER_PASSED_TO_AGENTS.contains(&key.as_str());
            if owned {
                return Err(EngineError::Unsupported(format!(
                    "the session's environment may not set {key}: the provider engine owns it"
                )));
            }
        }
        Ok(())
    }

    /// `bash`'s environment, from scratch (see the module docs).
    fn bash_env(&self, spec: &SessionSpec) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        for key in COPIED_ENV_KEYS {
            if let Some(value) = self.shared.inherited_env.get(*key) {
                env.insert((*key).to_owned(), value.clone());
            }
        }
        for (k, v) in SET_ENV {
            env.insert((*k).to_owned(), (*v).to_owned());
        }
        env.extend(spec.env.clone());
        env
    }
}

impl Engine for ProviderEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::Provider
    }

    fn open(&self, spec: SessionSpec) -> Result<SessionHandle, EngineError> {
        self.check_spec(&spec)?;
        let seed = rand::random();
        let builtins: Option<Arc<dyn BuiltinTools>> = match (&spec.builtins, &self.shared.builtins)
        {
            (
                Builtins::ProviderFileTools {
                    write_roots,
                    scratchpad,
                },
                Some(factory),
            ) => {
                let jobs_dir = spec.session_dir.parent().map_or_else(
                    || spec.session_dir.join("jobs"),
                    |task_dir| task_dir.join("jobs"),
                );
                let cx = BuiltinCx {
                    agent: spec.agent,
                    cwd: spec.cwd.clone(),
                    write_roots: write_roots.iter().chain(scratchpad).cloned().collect(),
                    jobs_dir,
                    env: self.bash_env(&spec),
                    supervisor: Arc::clone(&self.shared.supervisor),
                    clock: Arc::clone(&self.shared.clock),
                };
                let tools = factory.create(cx.clone());
                // A crewmate also gets `agent`, helper agents.
                Some(HelperTools::wrap(
                    tools,
                    Arc::clone(&self.shared),
                    Arc::clone(factory),
                    cx,
                    &spec,
                    seed,
                ))
            }
            _ => None,
        };
        let (handle, side) = session_channel();
        tokio::spawn(session::supervise(
            Arc::clone(&self.shared),
            spec,
            side,
            builtins,
            seed,
        ));
        Ok(handle)
    }

    fn models<'a>(
        &'a self,
        account: &'a AccountRef,
    ) -> BoxFuture<'a, Result<Vec<DetectedModel>, EngineError>> {
        Box::pin(async move {
            if !adapters::provider_supported(account.provider) {
                return Err(EngineError::Unsupported(format!(
                    "the provider engine can't serve {} accounts yet",
                    account.provider
                )));
            }
            // The account's own list isn't read here (Sign in with ChatGPT tokens have no listing
            // call at all). Saying so, rather than answering with the profiles' list as if the
            // account had sent it (or an empty list, which left every provider but ChatGPT with
            // no models), lets the core record the catalog's list as the stand-in (if a read
            // fails, the last list read is used, or failing that the built-in catalog).
            Err(EngineError::Unsupported(format!(
                "{} accounts' own model lists aren't read; the catalog's list is used",
                account.provider
            )))
        })
    }

    fn usage<'a>(
        &'a self,
        account: &'a AccountRef,
    ) -> BoxFuture<'a, Result<Option<UsageReading>, EngineError>> {
        Box::pin(async move { Ok(lock(&self.shared.usage).get(&account.id).cloned()) })
    }
}

impl Shared {
    /// The account's credential, refreshed first (through the token keeper) when it is about to
    /// expire, or when `refused` is the token a request just had refused (a 401). Errors are
    /// plain sentences that never carry a secret.
    pub(crate) async fn credential(
        &self,
        account: &AccountRef,
        refused: Option<&Secret>,
    ) -> Result<(Secret, CredentialKind), String> {
        let key = credential_key(account)?;
        let credential = self
            .tokens
            .fresh(account.provider, &key, refused)
            .await
            .map_err(|e| login_problem(&e))?;
        Ok(split(account.provider, credential))
    }

    /// Records that the account's login was refused even after a refresh, so routing leaves the
    /// account out until it is connected again.
    pub(crate) fn login_refused(&self, account: &AccountRef, problem: &str) {
        if let Ok(key) = credential_key(account) {
            self.tokens.health().mark(&key, problem);
        }
    }

    /// Remembers an account's usage headers.
    pub(crate) fn record_usage(
        &self,
        account: AccountId,
        windows: Vec<UsageWindow>,
        now: Timestamp,
    ) {
        if windows.is_empty() {
            return;
        }
        lock(&self.usage).insert(
            account,
            UsageReading {
                windows,
                read_at: now,
            },
        );
    }
}

/// The account's credential key, or the sentence saying why it has none.
fn credential_key(account: &AccountRef) -> Result<CredentialRef, String> {
    let key = account.credential.as_deref().ok_or_else(|| {
        "the account has no stored login; connect it in Settings ▸ Accounts".to_owned()
    })?;
    CredentialRef::new(key).map_err(|_| format!("the account's credential key {key:?} isn't valid"))
}

/// A token keeper error as the sentence a failed turn shows.
fn login_problem(error: &AuthError) -> String {
    if error.needs_reconnect() {
        format!("{error}; connect the account again in Settings ▸ Accounts")
    } else {
        error.to_string()
    }
}

/// A stored credential and the kind of request it makes. Only a ChatGPT login with an account ID
/// is the Codex login; every other OAuth token is a plain bearer token.
fn split(provider: Provider, credential: Credential) -> (Secret, CredentialKind) {
    match credential {
        Credential::ApiKey { key } => (key, CredentialKind::ApiKey),
        Credential::OAuth {
            access_token,
            account_id,
            ..
        } => {
            let kind = match account_id {
                Some(account_id) if provider == Provider::ChatGpt => {
                    CredentialKind::Codex { account_id }
                }
                _ => CredentialKind::SignInWithChatGpt,
            };
            (access_token, kind)
        }
    }
}

/// The wire API an account's requests use (for the init report): the one its provider's adapter
/// speaks with the stored credential (an account without one is reported as if it had a key).
pub(crate) fn api_for(provider: Provider, kind: Option<&CredentialKind>) -> Option<WireApi> {
    adapters::adapter_for(provider, kind.unwrap_or(&CredentialKind::ApiKey)).map(|a| a.api())
}

/// The adapter family the init report names in `capabilities`.
pub(crate) const fn adapter_capability(api: WireApi) -> &'static str {
    match api {
        WireApi::OpenAiResponses | WireApi::OpenAiCodexResponses => "responses",
        WireApi::AnthropicMessages => "anthropic-messages",
        WireApi::OpenAiCompletions => "chat-completions",
        WireApi::GoogleGenerativeAi => "gemini",
    }
}

impl Shared {
    /// The credential's kind without refreshing (for the init report); `None` if there is none.
    pub(crate) fn credential_kind(&self, account: &AccountRef) -> Option<CredentialKind> {
        let key = CredentialRef::new(account.credential.as_deref()?).ok()?;
        let credential = self.tokens.store().get(&key).ok()??;
        Some(split(account.provider, credential).1)
    }
}
