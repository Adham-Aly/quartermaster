//! The core's side of the Settings screens.
//!
//! [`SettingsPart`] is an [`ApiPart`]: it handles the settings `Command`s of `qm_types::api` and
//! fills `Snapshot.settings` on every snapshot.
//!
//! - **Connecting an account.** Subscription logins run through `qm_auth::login::Logins` in the
//!   background, showing the browser address or the device code in `SettingsView::login`;
//!   Claude is detected from your `claude` login instead. API keys are checked with
//!   `qm_auth::api_key` before anything is stored. A new account's credential goes into the
//!   credential store first, then its `accounts` row, then its models are read and its provider's
//!   existing tiers preserved (`routing::detect_account`), so the login screen can say what it found.
//! - **Reconnecting an account.** A login or key that reconnects an account whose login broke
//!   replaces its credential **in place**: the new credential is saved under the account's existing
//!   key (`TokenKeeper::save_in_place`, which clears its `LoginHealth` problem, so routing uses it
//!   again), and the account keeps its ID, label, placements and preferred mark; no "ChatGPT 2" is
//!   made. Which account a login reconnects:
//!   1. You pressed Reconnect on an account (`StartLogin`/`AddApiKey` with `account`): that
//!      account, if it is the same provider and kind (and, for a key, the same base URL), unless
//!      the login signed in as a different identity than the one it holds; then it is a new
//!      account and the broken one stays.
//!   2. Otherwise, for a subscription login: the active account of that provider holding the
//!      same identity (`qm_auth::identity`: the ChatGPT account ID, the xAI or Kimi subject;
//!      Copilot's is unknown), broken or not, since it is the same account signed in again.
//!   3. Otherwise, the one broken account of that provider whose identity doesn't conflict, if
//!      there is exactly one.
//!   4. Otherwise a new account. An API key is a new account unless you started it with
//!      Reconnect on the account (a key names no one, so nothing else can match it).
//! - **Settings in `config.toml`.** The pin and the Quartermaster's model and effort are saved through
//!   `config_file::save` and published on the config channel, so routing and the seat see them at
//!   once (the seat at its next idle moment).
//! - **Accounts and the tier board.** Labels, pausing, disconnecting, the preferred account,
//!   placements and the NOT IN USE list are each one transaction on the store thread
//!   (`qm_store::accounts`, `qm_store::models`); the next snapshot shows the change. A change
//!   that doesn't apply (a label already taken, an unknown account or model) is refused with a
//!   plain app note, and that snapshot puts the screens back as they were. Disconnecting forgets
//!   the account's credentials; a Claude account, which follows your `claude` login, is paused
//!   instead. What's listed under NOT IN USE is stored, so it survives a relaunch.
//!
//! Secrets: an API key arrives as `SecretText` and goes straight into a `qm_auth::Secret`; no
//! error message, log line, event or snapshot ever carries one.

use crate::api::{ApiPart, Conversation, Handled};
use crate::config_file;
use crate::projects::{check_workspace_folder, move_workspace, tilde, workspace_folder};
use crate::routing::{self, ModelCatalog};
use crate::turn_io::ChatLog;
use jiff::Timestamp;
use qm_auth::api_key::{self, CustomEndpoint};
use qm_auth::cancel::Cancel;
use qm_auth::login::{self, LoginEndpoints, LoginPrompt, Logins, SUBSCRIPTION_LOGINS};
use qm_auth::{AuthError, Credential, CredentialRef, TokenKeeper, identity};
use qm_engine::{Engine, UsageReading};
use qm_store::StoreError;
use qm_store::accounts::{self, AccountRow, ChangeError, NewAccount};
use qm_store::db::Db;
use qm_store::models;
use qm_types::BoxFuture;
use qm_types::api::{
    AccountStatus, Command, LoginMethod, LoginProvider, LoginStatus, LoginStep, LoginView,
    ModelPlacement, PinSetting, QuartermasterChoice, QuartermasterNow, SecretText, SettingsAccount,
    SettingsModel, SettingsView, Snapshot,
};
use qm_types::clock::Clock;
use qm_types::config::{AlwaysUse, AutocompactWindow, Config, QuartermasterModel};
use qm_types::data_dir::DataDir;
use qm_types::domain::{AccountKind, Effort, EngineKind, Provider};
use qm_types::ids::{AccountId, ModelId};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;
use tokio::sync::watch;

/// What the part needs.
pub struct SettingsDeps {
    pub db: Db,
    pub clock: Arc<dyn Clock>,
    pub data_dir: DataDir,
    /// Your home folder, for `~` in the personal folder's path (`HOME`, per Mac).
    pub home: Option<PathBuf>,
    /// The current settings; the part saves a change to `config.toml` and then sends it here.
    pub config: watch::Sender<Config>,
    /// The one token keeper over the credential store the provider engine reads too; new logins
    /// are saved through it, so their login health is cleared.
    pub tokens: Arc<TokenKeeper>,
    /// The engines, for reading a new account's models.
    pub engines: BTreeMap<EngineKind, Arc<dyn Engine>>,
    pub catalog: Arc<dyn ModelCatalog>,
    /// Signals the UI when a login moves on in the background.
    pub conversation: Arc<Conversation>,
    /// Every login's endpoints; `LoginEndpoints::default()` is the real providers'.
    pub endpoints: LoginEndpoints,
}

impl fmt::Debug for SettingsDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsDeps").finish_non_exhaustive()
    }
}

/// The Settings screens' commands and `Snapshot.settings`.
pub struct SettingsPart {
    inner: Arc<Inner>,
}

struct Inner {
    db: Db,
    clock: Arc<dyn Clock>,
    data_dir: DataDir,
    home: Option<PathBuf>,
    config: watch::Sender<Config>,
    tokens: Arc<TokenKeeper>,
    engines: BTreeMap<EngineKind, Arc<dyn Engine>>,
    catalog: Arc<dyn ModelCatalog>,
    conversation: Arc<Conversation>,
    endpoints: LoginEndpoints,
    state: Mutex<State>,
}

/// What lives only while the app runs.
#[derive(Default)]
struct State {
    login: Option<LoginState>,
}

struct LoginState {
    view: LoginView,
    /// When the code or the wait runs out.
    deadline: Option<Instant>,
    cancel: Cancel,
    /// Which login this is, so a cancelled one finishing late changes nothing.
    id: u64,
}

impl fmt::Debug for SettingsPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SettingsPart").finish_non_exhaustive()
    }
}

impl SettingsPart {
    pub fn new(deps: SettingsDeps) -> Self {
        Self {
            inner: Arc::new(Inner {
                db: deps.db,
                clock: deps.clock,
                data_dir: deps.data_dir,
                home: deps.home,
                config: deps.config,
                tokens: deps.tokens,
                engines: deps.engines,
                catalog: deps.catalog,
                conversation: deps.conversation,
                endpoints: deps.endpoints,
                state: Mutex::new(State::default()),
            }),
        }
    }
}

/// One change to the store, made in one transaction at the given time.
type StoreChange =
    Box<dyn FnOnce(&rusqlite::Connection, Timestamp) -> Result<(), ChangeError> + Send>;

/// The account and tier-board commands that are one store change each, with what the note says
/// Settings couldn't do if the store fails.
fn store_change(command: &Command) -> Option<(&'static str, StoreChange)> {
    Some(match command {
        Command::SetAccountLabel { account, label } => {
            let (account, label) = (*account, label.clone());
            (
                "rename the account",
                Box::new(move |c, _| accounts::set_label(c, account, &label)),
            )
        }
        &Command::PauseAccount { account } => (
            "pause the account",
            Box::new(move |c, _| accounts::set_paused(c, account, true)),
        ),
        &Command::ResumeAccount { account } => (
            "resume the account",
            Box::new(move |c, _| accounts::set_paused(c, account, false)),
        ),
        &Command::SetPreferredAccount { account } => (
            "change the preferred account",
            Box::new(move |c, _| accounts::set_preferred(c, account)),
        ),
        Command::PlaceModel {
            model,
            from,
            effort,
            to,
        } => {
            let (model, from, effort, to) = (model.clone(), *from, *effort, *to);
            (
                "place the model in a tier",
                Box::new(move |c, at| models::place_model(c, &model, from, effort, to, at)),
            )
        }
        Command::RemovePlacement {
            model,
            tier,
            effort,
        } => {
            let (model, tier, effort) = (model.clone(), *tier, *effort);
            (
                "take the model out of the tier",
                Box::new(move |c, at| models::remove_placement(c, &model, tier, effort, at)),
            )
        }
        Command::TakeOutOfUse { model } => {
            let model = model.clone();
            (
                "take the model out of use",
                Box::new(move |c, at| models::take_out_of_use(c, &model, at)),
            )
        }
        Command::SetPlacementEffort {
            model,
            tier,
            current,
            effort,
        } => {
            let (model, tier, current, effort) = (model.clone(), *tier, *current, *effort);
            (
                "change the placement's effort",
                Box::new(move |c, _| {
                    models::set_placement_effort(c, &model, tier, current, effort)
                }),
            )
        }
        Command::ListModel { model } => {
            let model = model.clone();
            (
                "list the model",
                Box::new(move |c, at| models::list_model(c, &model, at)),
            )
        }
        _ => return None,
    })
}

impl ApiPart for SettingsPart {
    fn handle<'a>(&'a self, command: &'a Command) -> Option<BoxFuture<'a, Handled>> {
        let inner = &self.inner;
        if let Some((what, change)) = store_change(command) {
            return Some(Box::pin(async move { inner.change(what, change).await }));
        }
        Some(match command {
            Command::StartLogin { provider, account } => {
                let (provider, account) = (*provider, *account);
                Box::pin(async move { Arc::clone(inner).start_login(provider, account) })
            }
            Command::UseDetectedLogin { provider } => {
                let provider = *provider;
                Box::pin(async move { Arc::clone(inner).use_detected(provider) })
            }
            Command::OpenLoginPage { url } => Box::pin(async move { inner.open_page(url) }),
            Command::CancelLogin | Command::FinishLogin => {
                Box::pin(async move { inner.end_login() })
            }
            Command::AddApiKey {
                provider,
                key,
                base_url,
                models,
                account,
            } => {
                let key = NewKey {
                    provider: *provider,
                    key: key.clone(),
                    base_url: base_url.clone(),
                    models: models.clone(),
                    account: *account,
                };
                Box::pin(async move { Arc::clone(inner).add_key(key) })
            }
            Command::DisconnectAccount { account } => {
                let account = *account;
                Box::pin(async move { inner.disconnect(account).await })
            }
            Command::SetPin {
                on,
                model,
                fallback,
            } => {
                let (on, model, fallback) = (*on, model.clone(), fallback.clone());
                Box::pin(async move { inner.set_pin(on, model, fallback).await })
            }
            Command::SetQuartermasterModel { choice } => {
                let choice = choice.clone();
                Box::pin(async move { inner.set_quartermaster(choice).await })
            }
            Command::SetQuartermasterEffort { effort } => {
                let effort = *effort;
                Box::pin(async move { inner.set_quartermaster_effort(effort).await })
            }
            Command::SetAutocompactWindow { tokens } => {
                let tokens = *tokens;
                Box::pin(async move { inner.set_autocompact_window(tokens).await })
            }
            Command::SetWorkspaceFolder { folder } => {
                let folder = folder.clone();
                Box::pin(async move { inner.set_workspace_folder(folder).await })
            }
            _ => return None,
        })
    }

    fn fill<'a>(&'a self, snapshot: &'a mut Snapshot) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            match self.inner.view().await {
                Ok(view) => snapshot.settings = view,
                Err(e) => tracing::error!(error = %e, "couldn't read the settings for the UI"),
            }
        })
    }
}

impl Inner {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn changed(&self) {
        self.conversation.changed();
    }

    /// A plain line in the chat; the next snapshot puts the screens back as they were.
    async fn note(&self, text: &str) -> Handled {
        let agent = self.conversation.agent();
        if let Err(e) = self.conversation.app_note(agent, None, text).await {
            tracing::error!(error = %e, "couldn't show a settings note");
        }
        Handled::Changed
    }

    // ---- accounts and the tier board ----

    /// Makes one change in one transaction on the store thread; the next snapshot shows it. A
    /// refusal or a failure becomes a plain note and nothing is changed.
    async fn change(
        &self,
        what: &str,
        f: impl FnOnce(&rusqlite::Connection, Timestamp) -> Result<(), ChangeError> + Send + 'static,
    ) -> Handled {
        self.change_with(what, f).await;
        Handled::Changed
    }

    /// [`Inner::change`], handing back what the change returned when it was made.
    async fn change_with<T: Send + 'static>(
        &self,
        what: &str,
        f: impl FnOnce(&rusqlite::Connection, Timestamp) -> Result<T, ChangeError> + Send + 'static,
    ) -> Option<T> {
        let at = self.clock.now();
        let outcome = self
            .db
            .call(move |conn| {
                let tx = conn.transaction()?;
                match f(&tx, at) {
                    Ok(v) => {
                        tx.commit()?;
                        Ok(Ok(v))
                    }
                    Err(ChangeError::Refused(reason)) => Ok(Err(reason)),
                    Err(ChangeError::Store(e)) => Err(e),
                }
            })
            .await;
        match outcome {
            Ok(Ok(v)) => Some(v),
            Ok(Err(reason)) => {
                self.note(&reason).await;
                None
            }
            Err(e) => {
                self.note(&format!("Settings couldn't {what}: {e}")).await;
                None
            }
        }
    }

    /// Disconnects an account and forgets its credentials; a Claude account follows your
    /// `claude` login, so it's paused instead.
    async fn disconnect(&self, account: AccountId) -> Handled {
        let removed = self
            .change_with("disconnect the account", move |c, at| {
                let follows_cli = accounts::load_account(c, account)?
                    .is_some_and(|a| a.provider == Provider::Claude && a.removed_at.is_none());
                if follows_cli {
                    accounts::set_paused(c, account, true)?;
                    return Ok(None);
                }
                accounts::disconnect(c, account, at).map(Some)
            })
            .await
            .flatten();
        let Some(key) = removed.and_then(|row| row.credential_ref) else {
            return Handled::Changed;
        };
        match CredentialRef::new(key) {
            Ok(key) => {
                if let Err(e) = self.tokens.forget(&key) {
                    tracing::warn!(error = %e, "couldn't forget a disconnected account's credentials");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "a disconnected account had an invalid credential key");
            }
        }
        Handled::Changed
    }

    // ---- settings in config.toml ----

    /// Saves `change` applied to the current settings, then publishes them.
    async fn save_config(&self, change: impl FnOnce(&mut Config)) -> Result<(), String> {
        let mut config = self.config.borrow().clone();
        change(&mut config);
        let dir = self.data_dir.clone();
        let saved = config.clone();
        tokio::task::spawn_blocking(move || config_file::save(&dir, &saved))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        self.config.send_replace(config);
        Ok(())
    }

    async fn set_pin(
        &self,
        on: bool,
        model: Option<ModelId>,
        fallback: Option<ModelId>,
    ) -> Handled {
        let always_use = if on {
            let Some(model) = model else {
                return self.note("The pin needs a model to always use.").await;
            };
            Some(AlwaysUse { model, fallback })
        } else {
            None
        };
        match self
            .save_config(|c| c.routing.always_use = always_use)
            .await
        {
            Ok(()) => Handled::Changed,
            Err(e) => self.note(&format!("The pin couldn't be saved: {e}")).await,
        }
    }

    async fn set_quartermaster(&self, choice: QuartermasterChoice) -> Handled {
        let model = match choice {
            QuartermasterChoice::Model(m) => QuartermasterModel::Model(m),
            _ => QuartermasterModel::Auto,
        };
        match self.save_config(|c| c.quartermaster.model = model).await {
            Ok(()) => Handled::Changed,
            Err(e) => {
                self.note(&format!("The Quartermaster's model couldn't be saved: {e}"))
                    .await
            }
        }
    }

    async fn set_quartermaster_effort(&self, effort: Option<Effort>) -> Handled {
        match self.save_config(|c| c.quartermaster.effort = effort).await {
            Ok(()) => Handled::Changed,
            Err(e) => {
                self.note(&format!(
                    "The Quartermaster's effort couldn't be saved: {e}"
                ))
                .await
            }
        }
    }

    /// Sets the autocompact window (clamped and rounded to a step by `AutocompactWindow`); each
    /// agent applies it at its next idle moment.
    async fn set_autocompact_window(&self, tokens: u32) -> Handled {
        let window = AutocompactWindow::new(tokens);
        match self
            .save_config(|c| c.compaction.autocompact_window = window)
            .await
        {
            Ok(()) => Handled::Changed,
            Err(e) => {
                self.note(&format!("The autocompact window couldn't be saved: {e}"))
                    .await
            }
        }
    }

    /// Points the personal workspace at a folder you chose. The folder is checked, and the change
    /// refused while personal tasks are open; then `[workspace] folder` is saved. No files move.
    async fn set_workspace_folder(&self, folder: String) -> Handled {
        let checked = match check_workspace_folder(&folder, self.home.as_deref(), &self.data_dir) {
            Ok(path) => path,
            Err(why) => {
                return self
                    .note(&format!("The personal folder wasn't changed: {why}."))
                    .await;
            }
        };
        let moving = checked.clone();
        let moved = match self
            .db
            .call(move |conn| Ok(move_workspace(conn, &moving)))
            .await
        {
            Ok(moved) => moved,
            Err(e) => Err(e.to_string()),
        };
        if let Err(why) = moved {
            return self
                .note(&format!("The personal folder wasn't changed: {why}."))
                .await;
        }
        let saved = folder.trim().to_owned();
        match self.save_config(|c| c.workspace.folder = Some(saved)).await {
            Ok(()) => Handled::Changed,
            Err(e) => {
                self.note(&format!("The personal folder couldn't be saved: {e}"))
                    .await
            }
        }
    }

    // ---- logins ----

    /// Replaces any login in progress with a new one (reconnecting `account`, if set); returns its
    /// id and cancel switch.
    fn begin_login(&self, provider: Provider, account: Option<AccountId>) -> (u64, Cancel) {
        let name = login::display_name(provider);
        let mut state = self.lock();
        let id = state.login.as_ref().map_or(1, |l| l.id + 1);
        if let Some(old) = state.login.take() {
            old.cancel.cancel();
        }
        let cancel = Cancel::new();
        let mut view = LoginView::new(provider, name, waiting_for(provider));
        view.account = account;
        state.login = Some(LoginState {
            view,
            deadline: None,
            cancel: cancel.clone(),
            id,
        });
        (id, cancel)
    }

    /// Updates login `id`, if it is still the one on screen.
    fn update_login(&self, id: u64, f: impl FnOnce(&mut LoginState)) {
        {
            let mut state = self.lock();
            match state.login.as_mut() {
                Some(l) if l.id == id => f(l),
                _ => return,
            }
        }
        self.changed();
    }

    fn fail_login(&self, id: u64, reason: String) {
        self.update_login(id, |l| {
            l.view.status = LoginStatus::Failed { reason };
            l.deadline = None;
        });
    }

    fn start_login(self: Arc<Self>, provider: Provider, account: Option<AccountId>) -> Handled {
        if provider == Provider::Claude {
            return self.use_detected(provider);
        }
        let (id, cancel) = self.begin_login(provider, account);
        tokio::spawn(async move {
            let install_id = {
                let dir = self.data_dir.clone();
                match tokio::task::spawn_blocking(move || qm_auth::chatgpt::install_id(&dir)).await
                {
                    Ok(Ok(id)) => id,
                    Ok(Err(e)) => return self.fail_login(id, e.to_string()),
                    Err(e) => return self.fail_login(id, e.to_string()),
                }
            };
            let logins = Logins::new(self.endpoints.clone(), install_id);
            let session = match logins.start(provider).await {
                Ok(s) => s,
                Err(e) => return self.fail_login(id, e.to_string()),
            };
            let expires = session.expires_in();
            let browser_url = match session.prompt() {
                LoginPrompt::Browser { url } => Some(url.clone()),
                LoginPrompt::DeviceCode { .. } => None,
            };
            let step = match session.prompt() {
                LoginPrompt::Browser { url } => LoginStep::Browser { url: url.clone() },
                LoginPrompt::DeviceCode { url, code } => LoginStep::DeviceCode {
                    url: url.clone(),
                    code: code.clone(),
                },
            };
            self.update_login(id, |l| {
                l.view.step = step;
                l.deadline = Some(Instant::now() + expires);
            });
            // A browser login opens your browser itself.
            if let Some(url) = browser_url {
                open_in_browser(&url);
            }
            match session.finish(&cancel).await {
                Ok(credential) => {
                    let outcome = self
                        .connect(
                            provider,
                            AccountKind::Subscription,
                            None,
                            credential,
                            account,
                        )
                        .await;
                    self.finish_login(id, outcome);
                }
                Err(AuthError::Cancelled) => {}
                Err(e) => self.fail_login(id, e.to_string()),
            }
        });
        Handled::Changed
    }

    fn use_detected(self: Arc<Self>, provider: Provider) -> Handled {
        let (id, _cancel) = self.begin_login(provider, None);
        if provider != Provider::Claude {
            self.fail_login(
                id,
                format!(
                    "{} has no login on this Mac to use",
                    login::display_name(provider)
                ),
            );
            return Handled::Changed;
        }
        if !self.engines.contains_key(&EngineKind::Claude) {
            self.fail_login(
                id,
                "claude isn't installed on this Mac; install Claude Code and log in with your subscription".into(),
            );
            return Handled::Changed;
        }
        tokio::spawn(async move {
            let outcome = self.connect_claude().await;
            self.finish_login(id, outcome);
        });
        Handled::Changed
    }

    fn finish_login(&self, id: u64, outcome: Result<(u32, bool), String>) {
        match outcome {
            Ok((models, starter_tiers)) => self.update_login(id, |l| {
                l.view.status = LoginStatus::Connected {
                    models,
                    starter_tiers,
                };
                l.deadline = None;
            }),
            Err(reason) => self.fail_login(id, reason),
        }
    }

    /// Opens the address the login on screen shows, and nothing else.
    fn open_page(&self, url: &str) -> Handled {
        let shown = self
            .lock()
            .login
            .as_ref()
            .is_some_and(|l| match &l.view.step {
                LoginStep::Browser { url: u } | LoginStep::DeviceCode { url: u, .. } => u == url,
                _ => false,
            });
        if shown {
            open_in_browser(url);
        } else {
            tracing::warn!("asked to open an address no login shows; ignored");
        }
        Handled::Unchanged
    }

    fn end_login(&self) -> Handled {
        if let Some(login) = self.lock().login.take() {
            login.cancel.cancel();
        }
        Handled::Changed
    }

    fn add_key(self: Arc<Self>, key: NewKey) -> Handled {
        let (id, _cancel) = self.begin_login(key.provider, key.account);
        tokio::spawn(async move {
            let outcome = self.check_and_connect_key(key).await;
            self.finish_login(id, outcome);
        });
        Handled::Changed
    }

    async fn check_and_connect_key(&self, new: NewKey) -> Result<(u32, bool), String> {
        let NewKey {
            provider,
            key,
            base_url,
            models,
            account,
        } = new;
        let secret = api_key::normalise(key.expose()).map_err(|e| e.to_string())?;
        let http = qm_auth::http::client();
        // Reconnecting a custom endpoint without naming its address keeps the one it has.
        let base_url = match (base_url, account) {
            (None, Some(target)) if provider == Provider::Custom => self
                .db
                .call(move |conn| accounts::load_account(conn, target))
                .await
                .map_err(|e| e.to_string())?
                .and_then(|row| row.base_url),
            (base_url, _) => base_url,
        };
        let base_url = if provider == Provider::Custom {
            let raw = base_url.ok_or("a custom endpoint needs its base URL")?;
            let base = api_key::normalise_base_url(&raw).map_err(|e| e.to_string())?;
            let endpoint = CustomEndpoint {
                base_url: base.clone(),
                models,
            };
            api_key::check_custom(&http, &endpoint, Some(&secret))
                .await
                .map_err(|e| e.to_string())?;
            Some(base)
        } else {
            api_key::check(&http, &self.endpoints.api_keys, provider, &secret)
                .await
                .map_err(|e| e.to_string())?;
            None
        };
        self.connect(
            provider,
            AccountKind::ApiKey,
            base_url,
            api_key::credential(secret),
            account,
        )
        .await
    }

    /// Connects a login or a checked key: reconnects the account it belongs to in place (the
    /// module docs give the rule), or adds a new account.
    async fn connect(
        &self,
        provider: Provider,
        kind: AccountKind,
        base_url: Option<String>,
        credential: Credential,
        target: Option<AccountId>,
    ) -> Result<(u32, bool), String> {
        let active = self
            .db
            .call(|conn| accounts::list_active(conn))
            .await
            .map_err(|e| e.to_string())?;
        let new = Login {
            provider,
            kind,
            base_url: base_url.as_deref(),
            identity: identity::identity(provider, &credential),
        };
        match self.reconnects(&new, target, active)? {
            Some(row) => self.reconnect(row, credential).await,
            None => self.connect_new(provider, kind, base_url, credential).await,
        }
    }

    /// The account `new` reconnects, if any (the rule in the module docs).
    fn reconnects(
        &self,
        new: &Login<'_>,
        target: Option<AccountId>,
        active: Vec<AccountRow>,
    ) -> Result<Option<AccountRow>, String> {
        let who = new.identity.as_deref();
        if let Some(target) = target {
            let Some(row) = active.into_iter().find(|a| a.id == target) else {
                return Err("the account to reconnect isn't connected any more".into());
            };
            if row.provider != new.provider || row.kind != new.kind {
                return Err(format!(
                    "{} can't be reconnected with a {} {}",
                    row.label,
                    login::display_name(new.provider),
                    match new.kind {
                        AccountKind::Subscription => "login",
                        AccountKind::ApiKey => "key",
                    }
                ));
            }
            let other_endpoint =
                new.kind == AccountKind::ApiKey && row.base_url.as_deref() != new.base_url;
            let other_identity = identity::conflicts(who, self.identity_of(&row).as_deref());
            return Ok((!other_endpoint && !other_identity).then_some(row));
        }
        if new.kind == AccountKind::ApiKey {
            return Ok(None);
        }
        let same: Vec<AccountRow> = active
            .into_iter()
            .filter(|a| a.provider == new.provider && a.kind == new.kind)
            .collect();
        if let Some(id) = who
            && let Some(row) = same
                .iter()
                .find(|a| self.identity_of(a).as_deref() == Some(id))
        {
            return Ok(Some(row.clone()));
        }
        let mut broken = same.into_iter().filter(|a| {
            self.is_broken(a) && !identity::conflicts(who, self.identity_of(a).as_deref())
        });
        Ok(match (broken.next(), broken.next()) {
            (Some(only), None) => Some(only),
            _ => None,
        })
    }

    /// Who an account's stored login signed in as, when that can be known.
    fn identity_of(&self, row: &AccountRow) -> Option<String> {
        let key = CredentialRef::new(row.credential_ref.as_deref()?).ok()?;
        let stored = self.tokens.stored(&key).ok()??;
        identity::identity(row.provider, &stored)
    }

    /// Whether routing skips an account because its login stopped working.
    fn is_broken(&self, row: &AccountRow) -> bool {
        row.credential_ref
            .as_deref()
            .and_then(|k| CredentialRef::new(k).ok())
            .is_some_and(|key| !self.tokens.health().is_usable(&key))
    }

    /// Saves `credential` over the account's old one, under its key, so it keeps its ID, label,
    /// placements and preferred mark; then reads its models again.
    async fn reconnect(
        &self,
        mut row: AccountRow,
        credential: Credential,
    ) -> Result<(u32, bool), String> {
        let existing = row
            .credential_ref
            .as_deref()
            .and_then(|k| CredentialRef::new(k).ok());
        let (key, new_key) = match existing {
            Some(key) => (key, false),
            None => (self.new_key(row.provider)?, true),
        };
        self.tokens
            .save_in_place(&key, credential)
            .await
            .map_err(|e| e.to_string())?;
        if new_key {
            let (id, stored) = (row.id, key.as_str().to_owned());
            let pointed = self
                .db
                .call(
                    move |conn| match accounts::set_credential_ref(conn, id, &stored) {
                        Ok(()) => Ok(Ok(())),
                        Err(ChangeError::Refused(reason)) => Ok(Err(reason)),
                        Err(ChangeError::Store(e)) => Err(e),
                    },
                )
                .await
                .map_err(|e| e.to_string())
                .and_then(|r| r);
            if let Err(e) = pointed {
                let _ = self.tokens.forget(&key);
                return Err(e);
            }
            row.credential_ref = Some(key.as_str().to_owned());
        }
        tracing::info!(account = %row.id, provider = %row.provider, "reconnected an account");
        self.detect(&row).await
    }

    /// A credential key no account has yet.
    fn new_key(&self, provider: Provider) -> Result<CredentialRef, String> {
        CredentialRef::new(format!("{provider}:{}", self.clock.now().as_millisecond()))
            .map_err(|e| e.to_string())
    }

    /// Stores a new account's credential, then its row, then reads its models.
    async fn connect_new(
        &self,
        provider: Provider,
        kind: AccountKind,
        base_url: Option<String>,
        credential: Credential,
    ) -> Result<(u32, bool), String> {
        let at = self.clock.now();
        let key = self.new_key(provider)?;
        self.tokens
            .save_new(&key, credential)
            .map_err(|e| e.to_string())?;
        let new = NewAccount {
            provider,
            label: login::display_name(provider).to_owned(),
            kind,
            credential_ref: Some(key.as_str().to_owned()),
            base_url,
            at,
        };
        let row = match self.insert(new).await {
            Ok(row) => row,
            Err(e) => {
                let _ = self.tokens.forget(&key);
                return Err(e.to_string());
            }
        };
        self.detect(&row).await
    }

    /// Adds the Claude account that follows your `claude` login, once, and reads its models.
    async fn connect_claude(&self) -> Result<(u32, bool), String> {
        let row = follow_claude_login(&self.db, self.clock.as_ref(), None)
            .await
            .map_err(|e| e.to_string())?;
        self.detect(&row).await
    }

    /// Inserts an account under a label no active account has ("ChatGPT", then "ChatGPT 2").
    async fn insert(&self, new: NewAccount) -> Result<AccountRow, StoreError> {
        self.db.call(move |conn| insert_unique(conn, new)).await
    }

    /// Reads a new account's models without changing tier placements.
    async fn detect(&self, row: &AccountRow) -> Result<(u32, bool), String> {
        let engine = self.engines.get(&row.engine).ok_or_else(|| {
            format!(
                "the account was added, but the {} engine isn't running, so its models weren't read",
                row.engine
            )
        })?;
        let outcome = routing::detect_account(
            &self.db,
            engine.as_ref(),
            row,
            self.catalog.as_ref(),
            self.clock.as_ref(),
        )
        .await
        .map_err(|e| e.to_string())?;
        let id = row.id;
        let found = self
            .db
            .call(move |conn| models::list_models(conn, id))
            .await
            .map_err(|e| e.to_string())?
            .iter()
            .filter(|m| m.gone_at.is_none())
            .count();
        Ok((u32::try_from(found).unwrap_or(u32::MAX), outcome.seeded > 0))
    }

    // ---- the view ----

    async fn view(&self) -> Result<SettingsView, StoreError> {
        let rows = self
            .db
            .call(|conn| {
                let accounts = accounts::list_active(conn)?;
                let mut models = Vec::new();
                for a in &accounts {
                    models.push((a.label.clone(), models::list_models(conn, a.id)?));
                }
                let placements = models::list_placements(conn)?;
                let listed = models::listed_models(conn)?;
                let seat = qm_store::seat::quartermaster(conn)?;
                Ok((accounts, models, placements, listed, seat))
            })
            .await?;
        let (accounts, account_models, placements, listed, seat) = rows;
        let config = self.config.borrow().clone();
        let state = self.lock();

        let mut view = SettingsView::default();
        let health = self.tokens.health();
        view.accounts = accounts
            .iter()
            .map(|a| {
                let mut v = account_view(a);
                v.problem = a
                    .credential_ref
                    .as_deref()
                    .and_then(|k| CredentialRef::new(k).ok())
                    .and_then(|key| health.problem(&key));
                v
            })
            .collect();

        let mut seen: BTreeSet<ModelId> = BTreeSet::new();
        for (label, rows) in &account_models {
            for m in rows.iter().filter(|m| m.gone_at.is_none()) {
                let id = m.facts.canonical.clone();
                if !seen.insert(id.clone()) {
                    continue;
                }
                let name = m
                    .facts
                    .display_name
                    .clone()
                    .unwrap_or_else(|| m.facts.model.to_string());
                let mut model = SettingsModel::new(id.clone(), name, label.clone());
                model.placements = placements
                    .iter()
                    .filter(|p| p.canonical == id)
                    .map(|p| ModelPlacement::new(p.tier, p.effort_override))
                    .collect();
                model.supported.clone_from(&m.facts.efforts);
                model.new = m.is_new && model.placements.is_empty();
                model.listed = listed.contains(&id);
                view.models.push(model);
            }
        }

        view.pin = match &config.routing.always_use {
            Some(pin) => PinSetting::new(true, Some(pin.model.clone()), pin.fallback.clone()),
            None => PinSetting::default(),
        };
        let folder = workspace_folder(&config, &self.data_dir, self.home.as_deref());
        view.workspace_folder = tilde(&folder, self.home.as_deref());
        view.tier_efforts = config.tiers.clone();
        view.autocompact_window = config.compaction.autocompact_window;
        view.quartermaster = match &config.quartermaster.model {
            QuartermasterModel::Model(m) => QuartermasterChoice::Model(m.clone()),
            QuartermasterModel::Auto => QuartermasterChoice::Auto,
        };
        view.quartermaster_effort = config.quartermaster.effort;
        view.quartermaster_now =
            seat.and_then(|s| Some(QuartermasterNow::new(s.model?, s.effort?)));
        view.providers = SUBSCRIPTION_LOGINS
            .iter()
            .filter_map(|(provider, method)| {
                let method = match method {
                    login::LoginMethod::Browser => LoginMethod::Browser,
                    login::LoginMethod::DeviceCode => LoginMethod::DeviceCode,
                    login::LoginMethod::Detected => LoginMethod::Detected,
                    login::LoginMethod::ApiKey => return None,
                };
                let mut p = LoginProvider::new(*provider, login::display_name(*provider), method);
                p.connected = accounts
                    .iter()
                    .find(|a| a.provider == *provider)
                    .map(|a| format!("connected · {}", a.label));
                Some(p)
            })
            .collect();
        view.login = state.login.as_ref().map(|l| {
            let mut v = l.view.clone();
            v.expires_in = l
                .deadline
                .map(|d| d.saturating_duration_since(Instant::now()).as_secs());
            v
        });
        Ok(view)
    }
}

/// An API key to check and connect (`Command::AddApiKey`).
struct NewKey {
    provider: Provider,
    key: SecretText,
    base_url: Option<String>,
    models: Vec<String>,
    /// The account it reconnects, when you started it from that account.
    account: Option<AccountId>,
}

/// What a finished login or checked key is, for finding the account it reconnects.
struct Login<'a> {
    provider: Provider,
    kind: AccountKind,
    base_url: Option<&'a str>,
    /// Who it signed in as, when known (`qm_auth::identity`).
    identity: Option<String>,
}

/// Who a login waits for.
fn waiting_for(provider: Provider) -> String {
    match provider {
        Provider::Copilot => "GitHub".to_owned(),
        Provider::ChatGpt => "OpenAI".to_owned(),
        Provider::Xai => "xAI".to_owned(),
        Provider::Kimi => "Moonshot".to_owned(),
        other => login::display_name(other).to_owned(),
    }
}

/// Inserts `new` under a label no active account has ("ChatGPT", then "ChatGPT 2").
fn insert_unique(
    conn: &rusqlite::Connection,
    mut new: NewAccount,
) -> Result<AccountRow, StoreError> {
    let taken: BTreeSet<String> = accounts::list_active(conn)?
        .into_iter()
        .map(|a| a.label)
        .collect();
    let base = new.label.clone();
    let mut n = 2;
    while taken.contains(&new.label) {
        new.label = format!("{base} {n}");
        n += 1;
    }
    let id = accounts::insert_account(conn, &new)?;
    accounts::load_account(conn, id)?.ok_or_else(|| StoreError::Invalid {
        column: "accounts.id",
        message: format!("account {id} vanished after it was added"),
    })
}

/// The label of the account that follows your `claude` login: "Claude Max" for a `max`
/// subscription, "Claude" when the subscription isn't known.
pub fn claude_account_label(subscription: Option<&str>) -> String {
    let name = login::display_name(Provider::Claude);
    let Some(plan) = subscription.map(str::trim).filter(|p| !p.is_empty()) else {
        return name.to_owned();
    };
    let mut chars = plan.chars();
    let plan: String = chars
        .next()
        .map(|c| c.to_uppercase().chain(chars).collect())
        .unwrap_or_default();
    format!("{name} {plan}")
}

/// The account that follows your `claude` login (a Claude subscription isn't logged into in the
/// app at all: the app detects your `claude` login on each Mac): the active Claude
/// account if there is one (paused or not: a Claude account is paused, never disconnected), else
/// a new one, labelled after the subscription. One store transaction, so two callers never add
/// two. Startup calls it when `claude` is logged in with a subscription; Settings ▸ Connect ▸
/// Claude calls it too.
pub async fn follow_claude_login(
    db: &Db,
    clock: &dyn Clock,
    subscription: Option<String>,
) -> Result<AccountRow, StoreError> {
    let at = clock.now();
    db.call(move |conn| {
        let tx = conn.transaction()?;
        let existing = accounts::list_active(&tx)?
            .into_iter()
            .find(|a| a.provider == Provider::Claude);
        let row = match existing {
            Some(row) => row,
            None => insert_unique(
                &tx,
                NewAccount {
                    provider: Provider::Claude,
                    label: claude_account_label(subscription.as_deref()),
                    kind: AccountKind::Subscription,
                    credential_ref: None,
                    base_url: None,
                    at,
                },
            )?,
        };
        tx.commit()?;
        Ok(row)
    })
    .await
}

fn account_view(a: &AccountRow) -> SettingsAccount {
    let mut v = SettingsAccount::new(a.id, a.label.clone());
    let how = match (a.provider, a.kind) {
        (Provider::Claude, _) => "your claude login".to_owned(),
        (Provider::Scripted, _) => "scripted".to_owned(),
        (_, AccountKind::Subscription) => "signed in here".to_owned(),
        (_, AccountKind::ApiKey) => match &a.base_url {
            Some(url) => format!("API key at {url}"),
            None => "API key".to_owned(),
        },
    };
    let engine = match a.engine {
        EngineKind::Claude => "Claude engine",
        EngineKind::Provider => "provider engine",
        EngineKind::Scripted => "scripted engine",
    };
    v.detail = format!("{how} · {engine}");
    v.status = if a.paused {
        AccountStatus::Paused
    } else {
        AccountStatus::Active
    };
    v.preferred = a.preferred;
    v.usage = a.usage.as_deref().and_then(usage_line);
    v.follows_cli = a.provider == Provider::Claude;
    v.provider = Some(a.provider);
    v.api_key = a.kind == AccountKind::ApiKey;
    v
}

/// "5-hour window 41% · 7-day 35%"; `None` when unknown (never shown as zero).
fn usage_line(json: &str) -> Option<String> {
    let reading: UsageReading = serde_json::from_str(json).ok()?;
    let parts: Vec<String> = reading
        .windows
        .iter()
        .map(|w| {
            let name = match w.name.as_str() {
                "five_hour" => "5-hour window",
                "seven_day" => "7-day",
                "seven_day_opus" => "7-day Opus",
                other => other,
            };
            format!("{name} {:.0}%", w.used_percent)
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Opens an http(s) address in your browser (macOS `open`).
fn open_in_browser(url: &str) {
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        tracing::warn!("refused to open a login address that isn't http(s)");
        return;
    }
    match std::process::Command::new("/usr/bin/open").arg(url).spawn() {
        Ok(mut child) => {
            // Reap it in the background; `open` returns at once.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(e) => tracing::warn!(error = %e, "couldn't open the browser"),
    }
}
