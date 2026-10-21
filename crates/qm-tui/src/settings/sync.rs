//! Keeping the settings screens in step with the core.
//!
//! Every change you make is applied to the screens at once and sent to the core. Each snapshot
//! then replaces the screens' data with the core's, keeping only what you are in the middle of
//! (the tab, the selection, an open picker or field, a drag). A change the core hasn't shown yet
//! is applied again on top, until a snapshot shows it or [`GRACE`] passes: the core answers in
//! milliseconds, so a change still missing by then was refused (the core says why in a note) and
//! the screens go back to what the core holds.

use super::model::{Login, Settings};
use super::{FieldFor, SettingsView};
use qm_types::api::{self, Command};
use std::time::{Duration, Instant};

/// How long a change waits for a snapshot to show it before the core's picture wins.
const GRACE: Duration = Duration::from_secs(2);

/// A change sent to the core, not yet seen in a snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pending {
    pub(crate) command: Command,
    pub(crate) sent: Instant,
}

impl Settings {
    /// The core's settings as the screens hold them; a login's countdown starts on the next tick.
    pub(crate) fn from_view(view: &api::SettingsView) -> Self {
        Self {
            accounts: view.accounts.clone(),
            models: view.models.clone(),
            pin: view.pin.clone(),
            quartermaster: view.quartermaster.clone(),
            quartermaster_effort: view.quartermaster_effort,
            quartermaster_now: view.quartermaster_now.clone(),
            providers: view.providers.clone(),
            login: view.login.as_ref().map(Login::from_view),
            notifications: view.notifications,
            unavailable: view.unavailable.clone(),
            tier_efforts: view.tier_efforts.clone(),
            autocompact_window: view.autocompact_window,
        }
    }
}

/// Whether `core` already shows `command`.
fn shown(command: &Command, core: &Settings) -> bool {
    match command {
        // The core's login replaces the screens' placeholder as soon as it exists.
        Command::StartLogin { provider, .. } => {
            core.login.as_ref().is_some_and(|l| l.provider == *provider)
        }
        Command::CancelLogin | Command::FinishLogin => core.login.is_none(),
        _ => {
            let mut after = core.clone();
            after.apply(command);
            after == *core
        }
    }
}

impl SettingsView {
    /// Applies a change at once, queues it for the core and remembers it until the core shows it.
    pub(crate) fn intend(&mut self, command: Command, now: Instant) {
        self.model.apply(&command);
        self.pending.push(Pending {
            command: command.clone(),
            sent: now,
        });
        self.outbox.push(command);
    }

    /// Takes the core's settings, keeping what you are in the middle of.
    pub(crate) fn refresh(&mut self, core: &api::SettingsView, now: Instant) {
        let mut fresh = Settings::from_view(core);
        // The same login step keeps counting down from when it first arrived.
        if let (Some(new), Some(old)) = (&mut fresh.login, &self.model.login)
            && new.provider == old.provider
            && new.step == old.step
        {
            new.received = old.received;
        }
        self.pending.retain(|p| {
            now.saturating_duration_since(p.sent) < GRACE && !shown(&p.command, &fresh)
        });
        for p in &self.pending {
            fresh.apply(&p.command);
        }
        self.model = fresh;
        self.loaded = true;
        self.clamp();
    }

    /// Drops selections and gestures that point past what is there now.
    fn clamp(&mut self) {
        self.routing_row = self.routing_row.min(super::ROUTING_ROWS - 1);
        let accounts = self.model.accounts.len();
        let models = self.model.models.len();
        let providers = self.model.providers.len();
        // `len()` is the "+ Connect an account" row, and the API-key row.
        if self.account.is_some_and(|i| i > accounts) {
            self.account = Some(accounts);
        }
        if self.provider.is_some_and(|i| i > providers) {
            self.provider = Some(providers);
        }
        if self.board.is_some_and(|b| b.model() >= models) {
            self.board = None;
        }
        if self.drag.is_some_and(|d| d.item.model() >= models) {
            self.drag = None;
        }
        let field_gone = matches!(
            self.field.as_ref().map(|f| f.purpose),
            Some(FieldFor::Label(i) | FieldFor::ReconnectKey(i)) if i >= accounts
        );
        if field_gone {
            self.field = None;
        }
    }
}
