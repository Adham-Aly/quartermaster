//! The settings view model and the commands the settings screens emit.
//!
//! The pieces are `qm_types::api`'s settings contract: accounts, models and their placements on the
//! tier board, the pin, the Quartermaster's model and the subscriptions you can connect.
//! [`Settings`] holds them as the screens use them, plus a login in progress with the moment its
//! countdown started ([`Login`] wraps [`api::LoginView`]). Every change you make on the screens
//! becomes an `api::Command`; the screens apply it to this model at once (so the board answers your
//! drag straight away) and queue it for the core.

use qm_types::api::{self, Command};
use qm_types::config::{AutocompactWindow, TierEfforts};
use qm_types::domain::{Effort, Provider as ProviderId, Tier};
use qm_types::ids::{AccountId, ModelId};
use std::time::{Duration, Instant};

pub(crate) use qm_types::api::{
    AccountStatus, KindTierRow, LoginMethod, LoginProvider as Provider, LoginStatus, LoginStep,
    ModelPlacement as Placement, PinSetting as Pin, QuartermasterChoice as QuartermasterModel,
    QuartermasterNow, SettingsAccount as Account, SettingsModel as Model,
};

/// The single label used for placed entries and catalogue combinations.
pub(crate) fn combo_label(model: &Model, effort: Option<Effort>) -> String {
    let level = effort.map_or_else(
        || {
            if model.supported.is_empty() {
                "no effort"
            } else {
                "default"
            }
            .to_owned()
        },
        |e| effort_label(e).to_lowercase(),
    );
    format!("{} ({level})", model.name)
}

/// A login in progress or just finished: the core's [`api::LoginView`], with its countdown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Login {
    pub(crate) provider: ProviderId,
    pub(crate) name: String,
    /// Who it waits for ("GitHub").
    pub(crate) waiting_for: String,
    pub(crate) step: LoginStep,
    pub(crate) status: LoginStatus,
    /// How long the code or link was good for when it arrived, and when that was (`None` in
    /// snapshots: the countdown then shows the full time).
    pub(crate) expires_in: Option<Duration>,
    pub(crate) received: Option<Instant>,
    /// The account this login reconnects, when you started it with Reconnect.
    pub(crate) account: Option<AccountId>,
}

impl Login {
    /// The core's login, not yet counted down.
    pub(crate) fn from_view(view: &api::LoginView) -> Self {
        Self {
            provider: view.provider,
            name: view.name.clone(),
            waiting_for: view.waiting_for.clone(),
            step: view.step.clone(),
            status: view.status.clone(),
            expires_in: view.expires_in.map(Duration::from_secs),
            received: None,
            account: view.account,
        }
    }
}

/// Everything the settings screens show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Settings {
    pub(crate) accounts: Vec<Account>,
    pub(crate) models: Vec<Model>,
    pub(crate) pin: Pin,
    pub(crate) quartermaster: QuartermasterModel,
    /// Your Quartermaster effort override; `None` uses Auto's entry or a named model's tier default.
    pub(crate) quartermaster_effort: Option<Effort>,
    /// What the Quartermaster runs on now: a model and its effort.
    pub(crate) quartermaster_now: Option<QuartermasterNow>,
    pub(crate) providers: Vec<Provider>,
    pub(crate) login: Option<Login>,
    /// Each tier's default effort as routing uses it (`config.toml`'s `[tiers]`).
    pub(crate) tier_efforts: TierEfforts,
    /// The autocompact window every agent compacts at (Routing).
    pub(crate) autocompact_window: AutocompactWindow,
    /// The Task kinds rows (Routing), in `KindRow::ALL` order.
    pub(crate) kind_tiers: Vec<KindTierRow>,
    /// Why the core's settings part didn't start this run, shown in place of the tabs that need it.
    pub(crate) unavailable: Option<String>,
}

/// The API-key providers offered by "Add key".
pub(crate) const KEY_PROVIDERS: [&str; 4] = ["Anthropic", "Gemini", "OpenAI", "custom"];

/// The provider behind one of [`KEY_PROVIDERS`].
pub(crate) fn key_provider(label: &str) -> ProviderId {
    match label {
        "Anthropic" => ProviderId::Anthropic,
        "Gemini" => ProviderId::Gemini,
        "OpenAI" => ProviderId::OpenAi,
        _ => ProviderId::Custom,
    }
}

/// The tiers top to bottom, as the board shows them.
pub(crate) const BOARD: [Tier; 4] = [Tier::Premium, Tier::High, Tier::Medium, Tier::Low];

/// "Extra high".
pub(crate) const fn effort_label(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "Low",
        Effort::Medium => "Medium",
        Effort::High => "High",
        Effort::ExtraHigh => "Extra high",
        Effort::Max => "Max",
    }
}

/// "High".
pub(crate) const fn tier_label(tier: Tier) -> &'static str {
    match tier {
        Tier::Premium => "Premium",
        Tier::High => "High",
        Tier::Medium => "Medium",
        Tier::Low => "Low",
    }
}

/// "HIGH", the board's heading.
pub(crate) const fn tier_heading(tier: Tier) -> &'static str {
    match tier {
        Tier::Premium => "PREMIUM",
        Tier::High => "HIGH",
        Tier::Medium => "MEDIUM",
        Tier::Low => "LOW",
    }
}

/// Where an empty tier's work goes instead, using routing's shared order. Premium requests
/// fall back to ordinary tiers, but ordinary requests never fall back to Premium.
pub(crate) fn fallback_tier(settings: &Settings, tier: Tier) -> Option<Tier> {
    tier.fallback_order()
        .iter()
        .copied()
        .skip(1)
        .find(|&t| settings.models.iter().any(|m| m.in_tier(t)))
}

impl Settings {
    /// What a Task kinds command changes, shown before the core answers.
    fn apply_kind_tier(&mut self, command: &Command) {
        for r in &mut self.kind_tiers {
            match command {
                Command::SetKindTier { row, tier } if r.row == *row => {
                    r.tier = tier.unwrap_or(r.default);
                }
                Command::ResetKindTiers => r.tier = r.default,
                _ => {}
            }
        }
    }

    /// The model with `id`.
    pub(crate) fn model(&self, id: &ModelId) -> Option<&Model> {
        self.models.iter().find(|m| &m.id == id)
    }

    /// "Opus 5.5", or the id when the model is unknown.
    pub(crate) fn model_name<'a>(&'a self, id: &'a ModelId) -> &'a str {
        self.model(id).map_or(id.as_str(), |m| m.name.as_str())
    }

    /// The models in at least one tier, board order.
    pub(crate) fn in_use(&self) -> Vec<&Model> {
        self.models.iter().filter(|m| !m.unused()).collect()
    }

    /// Applies `command` to this picture, as the core will once it accepts it. Commands that
    /// aren't settings changes leave it alone.
    pub(crate) fn apply(&mut self, command: &Command) {
        match command {
            Command::StartLogin { provider, account } => {
                let Some(p) = self.providers.iter().find(|p| &p.id == provider) else {
                    return;
                };
                let mut login =
                    Login::from_view(&api::LoginView::new(p.id, p.name.clone(), p.name.clone()));
                login.account = *account;
                self.login = Some(login);
            }
            Command::CancelLogin | Command::FinishLogin => self.login = None,
            Command::SetAccountLabel { account, label } => {
                if let Some(a) = self.account_mut(*account) {
                    a.label.clone_from(label);
                }
            }
            Command::PauseAccount { account } => {
                if let Some(a) = self.account_mut(*account) {
                    a.status = AccountStatus::Paused;
                }
            }
            Command::ResumeAccount { account } => {
                if let Some(a) = self.account_mut(*account) {
                    a.status = AccountStatus::Active;
                }
            }
            Command::DisconnectAccount { account } => {
                self.accounts.retain(|a| a.id != *account || a.follows_cli);
            }
            Command::SetPreferredAccount { account } => {
                for a in &mut self.accounts {
                    a.preferred = *account == Some(a.id);
                }
            }
            Command::PlaceModel {
                model,
                from,
                effort,
                to,
            } => self.place(model, *from, *effort, *to),
            Command::RemovePlacement {
                model,
                tier,
                effort,
            } => {
                if let Some(m) = self.model_mut(model) {
                    m.placements
                        .retain(|p| !(p.tier == *tier && p.effort == *effort));
                    m.listed |= m.placements.is_empty();
                }
            }
            Command::TakeOutOfUse { model } => {
                if let Some(m) = self.model_mut(model) {
                    m.placements.clear();
                    m.listed = true;
                }
            }
            Command::SetPlacementEffort {
                model,
                tier,
                current,
                effort,
            } => {
                // The core refuses an effort the tier already holds the model at.
                if let Some(m) = self.model_mut(model)
                    && m.placement(*tier, *effort).is_none()
                    && let Some(p) = m
                        .placements
                        .iter_mut()
                        .find(|p| p.tier == *tier && p.effort == *current)
                {
                    p.effort = *effort;
                }
            }
            Command::ListModel { model } => {
                if let Some(m) = self.model_mut(model) {
                    m.listed = true;
                }
            }
            Command::SetPin {
                on,
                model,
                fallback,
            } => {
                self.pin = Pin::new(*on, model.clone(), fallback.clone());
            }
            Command::SetQuartermasterModel { choice } => {
                self.quartermaster = choice.clone();
            }
            Command::SetQuartermasterEffort { effort } => self.quartermaster_effort = *effort,
            Command::SetAutocompactWindow { tokens } => {
                self.autocompact_window = AutocompactWindow::new(*tokens);
            }
            Command::SetKindTier { .. } | Command::ResetKindTiers => self.apply_kind_tier(command),
            // Only the core can act on the rest (logins it runs, keys it checks).
            _ => {}
        }
    }

    fn account_mut(&mut self, id: AccountId) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }

    fn model_mut(&mut self, id: &ModelId) -> Option<&mut Model> {
        self.models.iter_mut().find(|m| &m.id == id)
    }

    /// Adds `model`'s entry at `effort` to `to`, out of `from` when it moves (keeping its
    /// effort). A duplicate destination is refused without removing the source.
    fn place(&mut self, model: &ModelId, from: Option<Tier>, effort: Option<Effort>, to: Tier) {
        let Some(m) = self.model_mut(model) else {
            return;
        };
        if from == Some(to) || m.placement(to, effort).is_some() {
            return;
        }
        if let Some(from) = from {
            m.placements
                .retain(|p| !(p.tier == from && p.effort == effort));
        }
        if m.placement(to, effort).is_none() {
            m.placements.push(Placement::new(to, effort));
        }
        m.new = false;
    }
}
