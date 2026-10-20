//! The settings view model and the commands the settings screens emit.
//!
//! The pieces are `qm_types::api`'s settings contract: accounts, models and their placements on the
//! tier board, the pin, the Quartermaster's model and the subscriptions you can connect.
//! [`Settings`] holds them as the screens use them, plus a login in progress with the moment its
//! countdown started ([`Login`] wraps [`api::LoginView`]). Every change you make on the screens
//! becomes an `api::Command`; the screens apply it to this model at once (so the board answers your
//! drag straight away) and queue it for the core.

use qm_types::api::{self, Command};
use qm_types::config::TierEfforts;
use qm_types::domain::{Effort, Provider as ProviderId, Tier};
use qm_types::ids::{AccountId, ModelId};
use std::time::{Duration, Instant};

pub(crate) use qm_types::api::{
    AccountStatus, LoginMethod, LoginProvider as Provider, LoginStatus, LoginStep,
    ModelPlacement as Placement, PinSetting as Pin, QuartermasterChoice as QuartermasterModel,
    QuartermasterNow, SettingsAccount as Account, SettingsModel as Model,
};

/// The effort the tier default comes to on `model`: the tier's level, or the nearest level the
/// model supports below it (else its lowest).
pub(crate) fn model_tier_default(model: &Model, tier: Tier, tiers: &TierEfforts) -> Effort {
    let wanted = tier_default(tiers, tier);
    model
        .supported
        .iter()
        .copied()
        .filter(|&e| e <= wanted)
        .max()
        .or_else(|| model.supported.iter().copied().min())
        .unwrap_or(wanted)
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
    /// What the Quartermaster runs on now: a model and its effort.
    pub(crate) quartermaster_now: Option<QuartermasterNow>,
    pub(crate) providers: Vec<Provider>,
    pub(crate) login: Option<Login>,
    /// The bell and desktop notifications are on (System).
    pub(crate) notifications: bool,
    /// Each tier's default effort as routing uses it (`config.toml`'s `[tiers]`).
    pub(crate) tier_efforts: TierEfforts,
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

/// A tier's default effort before clamping to a model, from `tiers`: the efforts routing uses,
/// as `config.toml` sets them (`SettingsView.tier_efforts`; routing's own defaults when the file
/// says nothing, where Low runs on Medium).
pub(crate) const fn tier_default(tiers: &TierEfforts, tier: Tier) -> Effort {
    match tier {
        Tier::Premium => tiers.premium,
        Tier::High => tiers.high,
        Tier::Medium => tiers.medium,
        Tier::Low => tiers.low,
    }
}

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

/// Where an empty tier's work goes instead: the nearest tier with a model, looking up first
/// (low → medium → high → premium), then down.
pub(crate) fn fallback_tier(settings: &Settings, tier: Tier) -> Option<Tier> {
    let filled = |t: Tier| settings.models.iter().any(|m| m.placement(t).is_some());
    let order: [Tier; 3] = match tier {
        Tier::Low => [Tier::Medium, Tier::High, Tier::Premium],
        Tier::Medium => [Tier::High, Tier::Premium, Tier::Low],
        Tier::High => [Tier::Premium, Tier::Medium, Tier::Low],
        Tier::Premium => [Tier::High, Tier::Medium, Tier::Low],
    };
    order.into_iter().find(|&t| filled(t))
}

impl Settings {
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

    /// How many models only find reaches.
    pub(crate) fn unlisted(&self) -> usize {
        self.models
            .iter()
            .filter(|m| m.unused() && !m.listed)
            .count()
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
            Command::PlaceModel { model, from, to } => self.place(model, *from, *to),
            Command::RemovePlacement { model, tier } => {
                if let Some(m) = self.model_mut(model) {
                    m.placements.retain(|p| p.tier != *tier);
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
                effort,
            } => {
                if let Some(p) = self
                    .model_mut(model)
                    .and_then(|m| m.placements.iter_mut().find(|p| p.tier == *tier))
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
            Command::SetNotifications { enabled } => self.notifications = *enabled,
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

    /// Adds `model` to `to`, out of `from` when it moves; a placement keeps its effort.
    fn place(&mut self, model: &ModelId, from: Option<Tier>, to: Tier) {
        let Some(m) = self.model_mut(model) else {
            return;
        };
        let effort = from.and_then(|t| m.placement(t)).and_then(|p| p.effort);
        if let Some(from) = from {
            m.placements.retain(|p| p.tier != from);
        }
        if m.placement(to).is_none() {
            m.placements.push(Placement::new(to, effort));
        }
        m.new = false;
    }
}
