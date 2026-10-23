//! What keys, clicks, drags and context-menu choices do in the Settings popup. A click and its key
//! always run the same action here.

use super::model::{
    AccountStatus, BOARD, KEY_PROVIDERS, LoginMethod, LoginStatus, LoginStep, QuartermasterModel,
    effort_label, key_provider, model_tier_default, tier_label,
};
use super::{
    AccountLogin, AccountTarget, BoardItem, Drag, Field, FieldFor, PickFor, PickValue, Picker,
    SettingsHit, Tab, Zone, connect, levels,
};
use crate::app::App;
use crate::hit::Hit;
use crate::input::keys::list_move;
use crate::popups::{Anchor, Popup};
use crate::router::Screen;
use crate::view::MenuTarget;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use qm_types::api::{Command, SecretText};
use qm_types::domain::{Autonomy, Effort, Provider as ProviderId, Tier};
use qm_types::ids::{AccountId, ModelId};
use ratatui::layout::{Position, Rect};
use std::time::Instant;

/// One picker row: its label and what choosing it means.
type PickOption = (String, PickValue);

/// Where a menu opened from the keyboard sits: just under its row's label.
const BELOW: Anchor = Anchor::Row { dx: 4, dy: 1 };

const ADD_TO_TIER: &str = "Add to tier ▸";
const REMOVE_FROM_PREMIUM: &str = "Remove from Premium";
const REMOVE_FROM_HIGH: &str = "Remove from High";
const REMOVE_FROM_MEDIUM: &str = "Remove from Medium";
const REMOVE_FROM_LOW: &str = "Remove from Low";
const REMOVE_FROM_ALL: &str = "Remove from all tiers";
const ALWAYS_USE: &str = "Always use this model";
const LABEL: &str = "Label…";
const PAUSE: &str = "Pause";
const RESUME: &str = "Resume";
const PREFER: &str = "Prefer for crew work";
const UNPREFER: &str = "Stop preferring";
const DISCONNECT: &str = "Disconnect";
const RECONNECT: &str = "Reconnect";

/// Shown when you try to disconnect the account that follows your `claude` login.
const FOLLOWS_CLI: &str = "This account follows your claude login on this Mac; pause it instead.";
/// Shown for Connect on Claude before `claude` is logged in.
const LOG_IN_CLAUDE: &str = "Log in with claude in a terminal first; Quartermaster finds it.";

const fn remove_from(tier: Tier) -> &'static str {
    match tier {
        Tier::Premium => REMOVE_FROM_PREMIUM,
        Tier::High => REMOVE_FROM_HIGH,
        Tier::Medium => REMOVE_FROM_MEDIUM,
        Tier::Low => REMOVE_FROM_LOW,
    }
}

/// The context-menu items for a Settings target (the board's right-click menu, its "Add to tier"
/// submenu, and an account's menu).
pub(crate) fn menu_items(target: MenuTarget) -> Vec<&'static str> {
    match target {
        MenuTarget::Model(BoardItem::Placed { tier, .. }) => {
            vec![ADD_TO_TIER, remove_from(tier), REMOVE_FROM_ALL, ALWAYS_USE]
        }
        MenuTarget::Model(BoardItem::Unused { .. }) => vec![ADD_TO_TIER, ALWAYS_USE],
        // Every tier, the entry's own too: the same model can sit there again at another effort,
        // which the next step asks for.
        MenuTarget::AddToTier(_) => BOARD.into_iter().map(tier_label).collect(),
        MenuTarget::Account(account) => {
            let mut items = Vec::new();
            if account.login == AccountLogin::Broken {
                items.push(RECONNECT);
            }
            items.push(LABEL);
            items.push(if account.paused { RESUME } else { PAUSE });
            items.push(if account.preferred { UNPREFER } else { PREFER });
            if !account.follows_cli {
                items.push(DISCONNECT);
            }
            items
        }
        MenuTarget::Crew(_) | MenuTarget::Project(_) | MenuTarget::Action(_) | MenuTarget::Main => {
            Vec::new()
        }
    }
}

impl App {
    /// Applies a settings change at once and queues it for the core.
    fn intend(&mut self, command: Command) {
        self.view.settings.intend(command, Instant::now());
        self.redraw();
    }

    fn settings_set<T: PartialEq>(
        &mut self,
        slot: impl FnOnce(&mut super::SettingsView) -> &mut T,
        value: T,
    ) {
        self.change(|app| slot(&mut app.view.settings), value);
    }

    fn show_tab(&mut self, tab: Tab) {
        if self.view.settings.tab != tab {
            self.settings_set(|s| &mut s.scroll, 0);
        }
        self.settings_set(|s| &mut s.connecting, false);
        self.settings_set(|s| &mut s.tab, tab);
        self.settings_set(|s| &mut s.picker, None);
    }

    /// Where `hit` was drawn in the last frame.
    fn settings_rect(&self, hit: SettingsHit) -> Option<Rect> {
        self.term.drawn.hits.rect_of(Hit::Settings(hit))
    }

    // ---------------------------------------------------------------- keys

    /// A key in the Settings popup; returns whether it was used (otherwise the popup's keys
    /// apply: `esc` closes it, `?` help). `PgUp` and `PgDn` scroll a tab taller than
    /// the screen, and a selection the keys move is kept in view.
    pub(crate) fn on_settings_key(&mut self, key: KeyEvent, now: Instant) -> bool {
        self.show_selection();
        let overlay = self.view.settings.field.is_some() || self.view.settings.picker.is_some();
        if !overlay && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) {
            let max = self.term.drawn.scroll_max_of(crate::hit::Pane::Settings);
            let page = self
                .term
                .drawn
                .scroll_areas
                .iter()
                .find(|(_, p)| *p == crate::hit::Pane::Settings)
                .map_or(1, |(area, _)| {
                    usize::from(area.height.saturating_sub(2)).max(1)
                });
            let current = self.view.settings.scroll;
            let to = if key.code == KeyCode::PageUp {
                current.saturating_sub(page)
            } else {
                (current + page).min(max)
            };
            self.settings_set(|s| &mut s.scroll, to);
            return true;
        }
        let selection = |s: &super::SettingsView| {
            (
                s.tab,
                s.account,
                s.board,
                s.routing_row,
                s.project_row,
                s.provider,
            )
        };
        let before = selection(&self.view.settings);
        let used = self.on_settings_key_here(key, now);
        let after = selection(&self.view.settings);
        // `show_tab` starts another tab at its top; a moved selection is kept in view.
        if before.0 == after.0 && before != after {
            self.view.settings.reveal = true;
        }
        used
    }

    fn on_settings_key_here(&mut self, key: KeyEvent, now: Instant) -> bool {
        if self.view.settings.field.is_some() {
            self.on_field_key(key);
            return true;
        }
        if self.view.settings.picker.is_some() {
            self.on_picker_key(key.code);
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        if self.view.settings.connecting {
            return self.on_connect_key(key.code, now);
        }
        let tab = self.view.settings.tab;
        let at = Tab::ALL.iter().position(|&t| t == tab).unwrap_or(0);
        let last = Tab::ALL.len() - 1;
        match key.code {
            KeyCode::Tab | KeyCode::Right => {
                self.show_tab(Tab::ALL[if at == last { 0 } else { at + 1 }]);
                return true;
            }
            KeyCode::BackTab | KeyCode::Left => {
                self.show_tab(Tab::ALL[if at == 0 { last } else { at - 1 }]);
                return true;
            }
            KeyCode::Char(c @ '1'..='5') => {
                let i = usize::from(u8::try_from(c).unwrap_or(b'1') - b'1');
                self.show_tab(Tab::ALL[i]);
                return true;
            }
            _ => {}
        }
        match tab {
            Tab::Accounts => self.on_accounts_key(key.code, now),
            Tab::ModelLevels => self.on_levels_key(key.code),
            Tab::Routing => self.on_routing_key(key.code),
            Tab::System => self.on_system_key(key.code),
            Tab::Projects => self.on_settings_projects_key(key.code),
        }
    }

    fn on_accounts_key(&mut self, code: KeyCode, now: Instant) -> bool {
        let count = self.view.settings.model.accounts.len();
        let current = self.view.settings.account;
        if let Some(at) = list_move(code, current, count + 1) {
            self.settings_set(|s| &mut s.account, Some(at));
            return true;
        }
        let Some(i) = current else {
            if matches!(code, KeyCode::Char('c') | KeyCode::Enter) {
                self.open_connect();
                return true;
            }
            return false;
        };
        match code {
            KeyCode::Char('c') => self.open_connect(),
            KeyCode::Enter | KeyCode::Char(' ') if i == count => self.open_connect(),
            KeyCode::Enter | KeyCode::Char(' ') => self.open_account_menu(i, None),
            KeyCode::Char('p') => self.toggle_pause(i),
            KeyCode::Char('*') => self.toggle_preferred(i),
            KeyCode::Char('l') => self.open_label(i),
            KeyCode::Char('d') => self.disconnect(i, now),
            KeyCode::Char('r') => self.reconnect(i),
            _ => return false,
        }
        true
    }

    fn on_levels_key(&mut self, code: KeyCode) -> bool {
        let items = levels::items(&self.view.settings.model);
        let current = self
            .view
            .settings
            .board
            .and_then(|item| items.iter().position(|&i| i == item));
        if let Some(at) = list_move(code, current, items.len()) {
            self.settings_set(|s| &mut s.board, Some(items[at]));
            return true;
        }
        if code == KeyCode::Char('/') {
            self.open_picker(PickFor::Find, SettingsHit::Find);
            return true;
        }
        let Some(item) = current.map(|at| items[at]) else {
            return false;
        };
        match code {
            KeyCode::Char('m') => self.open_picker(PickFor::MoveTo(item), SettingsHit::Board(item)),
            KeyCode::Char('e') => {
                if item.tier().is_some() {
                    self.open_picker(PickFor::Effort(item), SettingsHit::Effort(item));
                }
            }
            KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace => {
                if item.tier().is_some() {
                    self.move_item(item, Zone::NotInUse);
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => self.open_board_menu(item, None),
            _ => return false,
        }
        true
    }

    fn on_routing_key(&mut self, code: KeyCode) -> bool {
        let row = self.view.settings.routing_row;
        if let Some(at) = list_move(code, Some(row), 3) {
            self.settings_set(|s| &mut s.routing_row, at);
            return true;
        }
        match code {
            KeyCode::Char(' ') => self.toggle_pin(),
            KeyCode::Enter if row == 0 => self.toggle_pin(),
            KeyCode::Enter if row == 2 => {}
            KeyCode::Char('-') => self.step_autocompact(false),
            KeyCode::Char('+' | '=') => self.step_autocompact(true),
            KeyCode::Enter | KeyCode::Char('q') => {
                self.open_picker(PickFor::QuartermasterModel, SettingsHit::QuartermasterModel);
            }
            KeyCode::Char('e') => {
                self.settings_set(|s| &mut s.routing_row, 1);
                self.open_picker(
                    PickFor::QuartermasterEffort,
                    SettingsHit::QuartermasterEffort,
                );
            }
            KeyCode::Char('c') => self.open_picker(PickFor::PinModel, SettingsHit::PinModel),
            KeyCode::Char('f') => self.open_picker(PickFor::PinFallback, SettingsHit::PinFallback),
            _ => return false,
        }
        true
    }

    /// Steps the autocompact window one step up or down and selects its row. At an end it does
    /// nothing.
    fn step_autocompact(&mut self, up: bool) {
        self.settings_set(|s| &mut s.routing_row, 2);
        let window = self.view.settings.model.autocompact_window;
        let next = if up { window.up() } else { window.down() };
        if next != window {
            self.intend(Command::SetAutocompactWindow {
                tokens: u32::from(next),
            });
        }
    }

    fn on_settings_projects_key(&mut self, code: KeyCode) -> bool {
        let count = self.snapshot.projects.len();
        let row = self.view.settings.project_row.min(count.saturating_sub(1));
        if let Some(at) = list_move(code, Some(row), count) {
            self.settings_set(|s| &mut s.project_row, at);
            return true;
        }
        match code {
            KeyCode::Char('a' | ' ') | KeyCode::Enter if count > 0 => {
                self.toggle_project_autonomy(row);
            }
            KeyCode::Char('f') => {
                let field = Field {
                    purpose: FieldFor::WorkspaceFolder,
                    text: self.view.settings.model.workspace_folder.clone(),
                    base_url: None,
                };
                self.settings_set(|s| &mut s.field, Some(field));
            }
            _ => return false,
        }
        true
    }

    /// Flips project row `i`'s autonomy and selects its row.
    fn toggle_project_autonomy(&mut self, i: usize) {
        let Some(current) = self.snapshot.projects.get(i).map(|p| p.autonomy) else {
            return;
        };
        self.settings_set(|s| &mut s.project_row, i);
        let next = match current {
            Autonomy::Auto => Autonomy::Normal,
            Autonomy::Normal => Autonomy::Auto,
        };
        self.set_project_autonomy(i, next);
    }

    fn on_system_key(&mut self, code: KeyCode) -> bool {
        if code != KeyCode::Char('s') {
            return false;
        }
        self.show(Screen::Setup);
        true
    }

    fn on_connect_key(&mut self, code: KeyCode, now: Instant) -> bool {
        let status = self
            .view
            .settings
            .model
            .login
            .as_ref()
            .map(|l| l.status.clone());
        match (code, &status) {
            (KeyCode::Esc, _) => self.settings_set(|s| &mut s.connecting, false),
            (KeyCode::Char('o'), Some(LoginStatus::Waiting)) => self.open_login_page(),
            (KeyCode::Char('y'), Some(LoginStatus::Waiting)) => self.copy_login_code(now),
            (KeyCode::Char('x'), Some(LoginStatus::Waiting | LoginStatus::Failed { .. })) => {
                self.intend(Command::CancelLogin);
            }
            (KeyCode::Char('r'), Some(LoginStatus::Connected { .. })) => self.finish_login(true),
            (KeyCode::Enter, Some(LoginStatus::Connected { .. })) => self.finish_login(false),
            (KeyCode::Enter, Some(LoginStatus::Failed { .. })) => self.retry_login(),
            (KeyCode::Enter | KeyCode::Char(' '), _) => {
                if let Some(i) = self.view.settings.provider {
                    self.press_provider(i, now);
                }
            }
            _ => {
                let rows = self.view.settings.model.providers.len() + 1;
                let Some(at) = list_move(code, self.view.settings.provider, rows) else {
                    return false;
                };
                self.settings_set(|s| &mut s.provider, Some(at));
            }
        }
        true
    }

    fn on_picker_key(&mut self, code: KeyCode) {
        let Some(picker) = &self.view.settings.picker else {
            return;
        };
        let visible = picker.visible();
        let at = visible.iter().position(|&i| i == picker.cursor);
        let finding = picker.query.is_some();
        match code {
            KeyCode::Esc => self.settings_set(|s| &mut s.picker, None),
            KeyCode::Enter => {
                if let Some(at) = at {
                    self.choose_pick(visible[at]);
                }
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End => {
                if let Some(to) = list_move(code, at, visible.len()) {
                    self.set_picker_cursor(visible[to]);
                }
            }
            KeyCode::Char(c @ ('j' | 'k')) if !finding => {
                if let Some(to) = list_move(KeyCode::Char(c), at, visible.len()) {
                    self.set_picker_cursor(visible[to]);
                }
            }
            KeyCode::Backspace if finding => self.edit_query(None),
            KeyCode::Char(c) if finding => self.edit_query(Some(c)),
            _ => {}
        }
    }

    fn set_picker_cursor(&mut self, cursor: usize) {
        if let Some(picker) = &mut self.view.settings.picker
            && picker.cursor != cursor
        {
            picker.cursor = cursor;
            self.redraw();
        }
    }

    /// Types `c` into the find query (`None` deletes a character), keeping the cursor on a match.
    fn edit_query(&mut self, c: Option<char>) {
        let Some(picker) = &mut self.view.settings.picker else {
            return;
        };
        let Some(query) = &mut picker.query else {
            return;
        };
        match c {
            Some(c) => query.push(c),
            None => {
                query.pop();
            }
        }
        let visible = picker.visible();
        if !visible.contains(&picker.cursor)
            && let Some(&first) = visible.first()
        {
            picker.cursor = first;
        }
        self.redraw();
    }

    fn on_field_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.settings_set(|s| &mut s.field, None),
            KeyCode::Enter => self.save_field(),
            KeyCode::Backspace => {
                if let Some(field) = &mut self.view.settings.field {
                    field.text.pop();
                    self.redraw();
                }
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(field) = &mut self.view.settings.field {
                    field.text.clear();
                    self.redraw();
                }
            }
            KeyCode::Char(c) if !ctrl => {
                if let Some(field) = &mut self.view.settings.field {
                    field.text.push(c);
                    self.redraw();
                }
            }
            _ => {}
        }
    }

    /// Pasted text goes into the open field (an API key is usually pasted), on one line.
    pub(crate) fn settings_paste(&mut self, pasted: &str) {
        if let Some(field) = &mut self.view.settings.field {
            field
                .text
                .extend(pasted.chars().filter(|c| !c.is_control()));
            self.redraw();
        } else if let Some(picker) = &mut self.view.settings.picker
            && let Some(query) = &mut picker.query
        {
            query.extend(pasted.chars().filter(|c| !c.is_control()));
            self.redraw();
        }
    }

    // ---------------------------------------------------------------- mouse

    /// A left click on a Settings target.
    pub(crate) fn settings_click(&mut self, hit: SettingsHit, double: bool, now: Instant) {
        let s = &self.view.settings;
        let overlay_open = s.picker.is_some() || s.field.is_some();
        if overlay_open
            && !matches!(
                hit,
                SettingsHit::PickerItem(_)
                    | SettingsHit::FieldSave
                    | SettingsHit::FieldCancel
                    | SettingsHit::Overlay
            )
        {
            // A click outside the open picker or field closes it.
            self.settings_set(|s| &mut s.picker, None);
            self.settings_set(|s| &mut s.field, None);
            return;
        }
        match hit {
            SettingsHit::Tab(tab) => self.show_tab(tab),
            SettingsHit::Account(i) => {
                self.settings_set(|s| &mut s.account, Some(i));
                if double {
                    self.open_label(i);
                }
            }
            SettingsHit::AccountLabel(i) => self.open_label(i),
            SettingsHit::AccountPause(i) => self.toggle_pause(i),
            SettingsHit::AccountDisconnect(i) => self.disconnect(i, now),
            SettingsHit::AccountReconnect(i) => self.reconnect(i),
            SettingsHit::Connect => self.open_connect(),
            SettingsHit::Board(item) => {
                self.settings_set(|s| &mut s.board, Some(item));
                self.settings_set(|s| &mut s.drag, Some(Drag { item, over: None }));
            }
            SettingsHit::Effort(item) => {
                self.settings_set(|s| &mut s.board, Some(item));
                self.open_picker(PickFor::Effort(item), hit);
            }
            SettingsHit::Zone(_) | SettingsHit::Overlay | SettingsHit::Backdrop => {}
            SettingsHit::Find => self.open_picker(PickFor::Find, hit),
            SettingsHit::PinSwitch => {
                self.settings_set(|s| &mut s.routing_row, 0);
                self.toggle_pin();
            }
            SettingsHit::ProjectAutonomy(i, autonomy) => {
                self.settings_set(|s| &mut s.project_row, i);
                if self.snapshot.projects.get(i).map(|p| p.autonomy) != Some(autonomy) {
                    self.set_project_autonomy(i, autonomy);
                }
            }
            SettingsHit::SetupCheck => self.show(Screen::Setup),
            SettingsHit::PinModel => self.open_picker(PickFor::PinModel, hit),
            SettingsHit::PinFallback => self.open_picker(PickFor::PinFallback, hit),
            SettingsHit::QuartermasterModel => {
                self.settings_set(|s| &mut s.routing_row, 1);
                self.open_picker(PickFor::QuartermasterModel, hit);
            }
            SettingsHit::QuartermasterEffort => {
                self.settings_set(|s| &mut s.routing_row, 1);
                self.open_picker(PickFor::QuartermasterEffort, hit);
            }
            SettingsHit::AutocompactDown => self.step_autocompact(false),
            SettingsHit::AutocompactUp => self.step_autocompact(true),
            SettingsHit::Provider(i) => {
                self.settings_set(|s| &mut s.provider, Some(i));
                if double {
                    self.press_provider(i, now);
                }
            }
            SettingsHit::ProviderButton(i) => {
                self.settings_set(|s| &mut s.provider, Some(i));
                self.press_provider(i, now);
            }
            SettingsHit::AddKey => {
                let row = self.view.settings.model.providers.len();
                self.settings_set(|s| &mut s.provider, Some(row));
                self.open_picker(PickFor::KeyProvider, hit);
            }
            SettingsHit::OpenBrowser => self.open_login_page(),
            SettingsHit::CopyCode => self.copy_login_code(now),
            SettingsHit::CancelLogin => self.intend(Command::CancelLogin),
            SettingsHit::RetryLogin => self.retry_login(),
            SettingsHit::ReviewTiers => self.finish_login(true),
            SettingsHit::DoneLogin => self.finish_login(false),
            SettingsHit::PickerItem(i) => self.choose_pick(i),
            SettingsHit::FieldSave => self.save_field(),
            SettingsHit::FieldCancel => self.settings_set(|s| &mut s.field, None),
        }
    }

    /// A right-click on a Settings target: a board item's or an account's menu at the pointer.
    pub(crate) fn settings_right_click(&mut self, hit: SettingsHit, at: Position) {
        if self.view.settings.picker.is_some() || self.view.settings.field.is_some() {
            return;
        }
        match hit {
            SettingsHit::Board(item) | SettingsHit::Effort(item) => {
                self.open_board_menu(item, Some(at));
            }
            SettingsHit::Account(i)
            | SettingsHit::AccountLabel(i)
            | SettingsHit::AccountPause(i)
            | SettingsHit::AccountDisconnect(i)
            | SettingsHit::AccountReconnect(i) => self.open_account_menu(i, Some(at)),
            _ => {}
        }
    }

    /// Whether a model is being dragged on the board.
    pub(crate) fn settings_dragging(&self) -> bool {
        self.popup() == Some(Popup::Settings) && self.view.settings.drag.is_some()
    }

    /// The pointer moved with a model held: light up the zone under it (not the one it left).
    pub(crate) fn settings_drag_to(&mut self, hit: Option<Hit>) {
        let Some(drag) = self.view.settings.drag else {
            return;
        };
        let zone = match hit {
            Some(Hit::Settings(SettingsHit::Zone(zone))) => Some(zone),
            Some(Hit::Settings(SettingsHit::Board(item) | SettingsHit::Effort(item))) => {
                Some(item.tier().map_or(Zone::NotInUse, Zone::Tier))
            }
            _ => None,
        };
        let source = drag.item.tier().map_or(Zone::NotInUse, Zone::Tier);
        let over = zone.filter(|&z| z != source);
        self.settings_set(|s| &mut s.drag, Some(Drag { over, ..drag }));
    }

    /// The button came up: drop the model on the lit zone, if any.
    pub(crate) fn settings_drop(&mut self, hit: Option<Hit>) {
        self.settings_drag_to(hit);
        let Some(drag) = self.view.settings.drag.take() else {
            return;
        };
        self.redraw();
        if let Some(zone) = drag.over {
            self.move_item(drag.item, zone);
        }
        self.change(|app| &mut app.term.hover, hit);
    }

    /// The clock: the login's spinner and countdown move on.
    pub(crate) fn settings_tick(&mut self, now: Instant) {
        let showing = self.popup() == Some(Popup::Settings);
        let s = &mut self.view.settings;
        let Some(login) = &mut s.model.login else {
            return;
        };
        if login.status != LoginStatus::Waiting {
            return;
        }
        if login.received.is_none() {
            login.received = Some(now);
        }
        let login = login.clone();
        let before = (
            connect::spinner_frame(&login, s),
            connect::remaining(&login, s).map(|d| d.as_secs()),
        );
        s.now = Some(now);
        let after = (
            connect::spinner_frame(&login, s),
            connect::remaining(&login, s).map(|d| d.as_secs()),
        );
        if showing && s.connecting && before != after {
            self.redraw();
        }
    }

    // ---------------------------------------------------------------- menus

    fn open_board_menu(&mut self, item: BoardItem, at: Option<Position>) {
        self.settings_set(|s| &mut s.board, Some(item));
        let target = MenuTarget::Model(item);
        let anchor = at.map_or(BELOW, |at| self.anchor_at(target, at));
        self.open_menu(target, anchor);
    }

    fn open_account_menu(&mut self, i: usize, at: Option<Position>) {
        let Some(account) = self.view.settings.model.accounts.get(i) else {
            return;
        };
        let target = AccountTarget {
            index: i,
            paused: account.status == AccountStatus::Paused,
            preferred: account.preferred,
            follows_cli: account.follows_cli,
            login: if account.problem.is_some() {
                AccountLogin::Broken
            } else {
                AccountLogin::Working
            },
        };
        self.settings_set(|s| &mut s.account, Some(i));
        let target = MenuTarget::Account(target);
        let anchor = at.map_or(BELOW, |at| self.anchor_at(target, at));
        self.open_menu(target, anchor);
    }

    /// Selects what a Settings menu acts on.
    pub(crate) fn settings_select_target(&mut self, target: MenuTarget) {
        match target {
            MenuTarget::Model(item) | MenuTarget::AddToTier(item) => {
                self.settings_set(|s| &mut s.board, Some(item));
            }
            MenuTarget::Account(account) => {
                self.settings_set(|s| &mut s.account, Some(account.index));
            }
            MenuTarget::Crew(_)
            | MenuTarget::Project(_)
            | MenuTarget::Action(_)
            | MenuTarget::Main => {}
        }
    }

    /// Runs a Settings menu item.
    pub(crate) fn settings_menu_choice(&mut self, item: &str, target: MenuTarget, now: Instant) {
        match target {
            MenuTarget::Model(board) => {
                let Some(id) = self.model_id(board) else {
                    return;
                };
                match item {
                    ADD_TO_TIER => {
                        self.open_menu(MenuTarget::AddToTier(board), BELOW);
                    }
                    REMOVE_FROM_ALL => self.intend(Command::TakeOutOfUse { model: id }),
                    ALWAYS_USE => {
                        let fallback = self
                            .view
                            .settings
                            .model
                            .pin
                            .fallback
                            .clone()
                            .filter(|f| *f != id);
                        self.intend(Command::SetPin {
                            on: true,
                            model: Some(id),
                            fallback,
                        });
                    }
                    _ => {
                        if board.tier().is_some_and(|t| item == remove_from(t)) {
                            self.move_item(board, Zone::NotInUse);
                        }
                    }
                }
            }
            MenuTarget::AddToTier(board) => {
                if let Some(&to) = BOARD.iter().find(|&&t| tier_label(t) == item) {
                    self.open_picker(PickFor::AddAt(board, to), SettingsHit::Board(board));
                }
            }
            MenuTarget::Account(account) => match item {
                LABEL => self.open_label(account.index),
                PAUSE | RESUME => self.toggle_pause(account.index),
                PREFER | UNPREFER => self.toggle_preferred(account.index),
                DISCONNECT => self.disconnect(account.index, now),
                RECONNECT => self.reconnect(account.index),
                _ => {}
            },
            MenuTarget::Crew(_)
            | MenuTarget::Project(_)
            | MenuTarget::Action(_)
            | MenuTarget::Main => {}
        }
    }

    // ---------------------------------------------------------------- actions

    fn model_id(&self, item: BoardItem) -> Option<ModelId> {
        self.view
            .settings
            .model
            .models
            .get(item.model())
            .map(|m| m.id.clone())
    }

    fn account_id(&self, i: usize) -> Option<AccountId> {
        self.view.settings.model.accounts.get(i).map(|a| a.id)
    }

    /// Moves a board item to `zone`, and keeps it selected where it lands.
    fn move_item(&mut self, item: BoardItem, zone: Zone) {
        let Some(model) = self.model_id(item) else {
            return;
        };
        let (from, effort) = (item.tier(), item.effort());
        let intent = match zone {
            Zone::Tier(to) if from == Some(to) => return,
            Zone::Tier(to) => Command::PlaceModel {
                model,
                from,
                effort,
                to,
            },
            Zone::NotInUse => match from {
                Some(tier) => Command::RemovePlacement {
                    model,
                    tier,
                    effort,
                },
                None => return,
            },
        };
        self.intend(intent);
        let index = item.model();
        let landed = match zone {
            Zone::Tier(tier) => Some(BoardItem::Placed {
                model: index,
                tier,
                effort,
            }),
            Zone::NotInUse => {
                let m = &self.view.settings.model.models[index];
                if m.unused() {
                    Some(BoardItem::Unused { model: index })
                } else {
                    m.placements.first().map(|p| BoardItem::Placed {
                        model: index,
                        tier: p.tier,
                        effort: p.effort,
                    })
                }
            }
        };
        self.settings_set(|s| &mut s.board, landed);
    }

    fn toggle_pause(&mut self, i: usize) {
        let Some(account) = self.view.settings.model.accounts.get(i) else {
            return;
        };
        let id = account.id;
        let intent = match account.status {
            AccountStatus::Paused => Command::ResumeAccount { account: id },
            _ => Command::PauseAccount { account: id },
        };
        self.settings_set(|s| &mut s.account, Some(i));
        self.intend(intent);
    }

    fn toggle_preferred(&mut self, i: usize) {
        let Some(account) = self.view.settings.model.accounts.get(i) else {
            return;
        };
        let account = (!account.preferred).then_some(account.id);
        self.intend(Command::SetPreferredAccount { account });
    }

    fn disconnect(&mut self, i: usize, now: Instant) {
        let Some(account) = self.view.settings.model.accounts.get(i) else {
            return;
        };
        if account.follows_cli {
            self.flash(FOLLOWS_CLI, now);
            return;
        }
        let Some(id) = self.account_id(i) else { return };
        self.intend(Command::DisconnectAccount { account: id });
        let count = self.view.settings.model.accounts.len();
        self.settings_set(|s| &mut s.account, Some(i.min(count)));
    }

    fn open_label(&mut self, i: usize) {
        let Some(account) = self.view.settings.model.accounts.get(i) else {
            return;
        };
        let field = Field {
            purpose: FieldFor::Label(i),
            text: account.label.clone(),
            base_url: None,
        };
        self.settings_set(|s| &mut s.account, Some(i));
        self.settings_set(|s| &mut s.field, Some(field));
    }

    fn save_field(&mut self) {
        let Some(field) = self.view.settings.field.take() else {
            return;
        };
        self.redraw();
        let text = field.text.trim().to_owned();
        if text.is_empty() {
            return;
        }
        match field.purpose {
            FieldFor::Label(i) => {
                if let Some(account) = self.account_id(i) {
                    self.intend(Command::SetAccountLabel {
                        account,
                        label: text,
                    });
                }
            }
            FieldFor::WorkspaceFolder => self.intend(Command::SetWorkspaceFolder { folder: text }),
            FieldFor::BaseUrl => {
                let field = Field {
                    purpose: FieldFor::ApiKey("custom"),
                    text: String::new(),
                    base_url: Some(text),
                };
                self.settings_set(|s| &mut s.field, Some(field));
            }
            FieldFor::ApiKey(provider) => self.intend(Command::AddApiKey {
                provider: key_provider(provider),
                key: SecretText::new(text),
                base_url: field.base_url,
                models: Vec::new(),
                account: None,
            }),
            FieldFor::ReconnectKey(i) => {
                let Some(account) = self.view.settings.model.accounts.get(i) else {
                    return;
                };
                let Some(provider) = account.provider else {
                    return;
                };
                let id = account.id;
                self.open_connect();
                // The core keeps a custom endpoint's address when none is given.
                self.intend(Command::AddApiKey {
                    provider,
                    key: SecretText::new(text),
                    base_url: None,
                    models: Vec::new(),
                    account: Some(id),
                });
            }
        }
    }

    /// Reconnects account `i`, whose login stopped working: a new login (or key, for a key account)
    /// replaces its credential in place, so it keeps its tiers.
    fn reconnect(&mut self, i: usize) {
        let Some(account) = self.view.settings.model.accounts.get(i) else {
            return;
        };
        let (Some(provider), id, api_key) = (account.provider, account.id, account.api_key) else {
            return;
        };
        if account.follows_cli {
            // A Claude account follows your `claude` login; there is nothing to sign in here.
            return;
        }
        self.settings_set(|s| &mut s.account, Some(i));
        if api_key {
            let field = Field {
                purpose: FieldFor::ReconnectKey(i),
                text: String::new(),
                base_url: None,
            };
            self.settings_set(|s| &mut s.field, Some(field));
            return;
        }
        self.open_connect();
        self.start_login(provider, Some(id));
    }

    fn toggle_pin(&mut self) {
        let pin = self.view.settings.model.pin.clone();
        if pin.on {
            self.intend(Command::SetPin {
                on: false,
                model: pin.model,
                fallback: pin.fallback,
            });
        } else if pin.model.is_some() {
            self.intend(Command::SetPin {
                on: true,
                model: pin.model,
                fallback: pin.fallback,
            });
        } else {
            // Nothing to pin yet: choosing a model turns the pin on.
            self.open_picker(PickFor::PinModel, SettingsHit::PinModel);
        }
    }

    // ---------------------------------------------------------------- connecting

    fn open_connect(&mut self) {
        self.settings_set(|s| &mut s.tab, Tab::Accounts);
        self.settings_set(|s| &mut s.connecting, true);
        // A login already running is the row to show; otherwise the first.
        let running = self.view.settings.model.login.as_ref().and_then(|login| {
            let providers = &self.view.settings.model.providers;
            providers.iter().position(|p| p.id == login.provider)
        });
        if running.is_some() || self.view.settings.provider.is_none() {
            self.settings_set(|s| &mut s.provider, Some(running.unwrap_or(0)));
        }
    }

    fn press_provider(&mut self, i: usize, now: Instant) {
        let providers = &self.view.settings.model.providers;
        if i == providers.len() {
            self.open_picker(PickFor::KeyProvider, SettingsHit::AddKey);
            return;
        }
        let Some(provider) = providers.get(i) else {
            return;
        };
        let id = provider.id;
        if provider.method == LoginMethod::Detected {
            if provider.connected.is_some() {
                self.intend(Command::UseDetectedLogin { provider: id });
            } else {
                self.flash(LOG_IN_CLAUDE, now);
            }
            return;
        }
        // Signing in to a connected subscription again reconnects the account it signs in as; the
        // core decides.
        self.start_login(id, None);
    }

    /// Starts a subscription login, reconnecting `account` if set; a login still waiting is
    /// cancelled first.
    fn start_login(&mut self, provider: ProviderId, account: Option<AccountId>) {
        if self
            .view
            .settings
            .model
            .login
            .as_ref()
            .is_some_and(|l| l.status == LoginStatus::Waiting)
        {
            self.intend(Command::CancelLogin);
        }
        self.intend(Command::StartLogin { provider, account });
    }

    fn login_url(&self) -> Option<String> {
        match &self.view.settings.model.login.as_ref()?.step {
            LoginStep::Browser { url } | LoginStep::DeviceCode { url, .. } => Some(url.clone()),
            _ => None,
        }
    }

    fn open_login_page(&mut self) {
        if let Some(url) = self.login_url() {
            self.intend(Command::OpenLoginPage { url });
        }
    }

    fn copy_login_code(&mut self, now: Instant) {
        let code = match self.view.settings.model.login.as_ref().map(|l| &l.step) {
            Some(LoginStep::DeviceCode { code, .. }) => code.clone(),
            _ => return,
        };
        self.copy(&code, now);
    }

    fn retry_login(&mut self) {
        let Some((provider, account)) = self
            .view
            .settings
            .model
            .login
            .as_ref()
            .map(|l| (l.provider, l.account))
        else {
            return;
        };
        self.intend(Command::StartLogin { provider, account });
    }

    /// Dismisses a connected login: to the tier board ("Review tiers") or back to Accounts.
    fn finish_login(&mut self, review: bool) {
        self.intend(Command::FinishLogin);
        self.settings_set(|s| &mut s.connecting, false);
        let tab = if review {
            Tab::ModelLevels
        } else {
            Tab::Accounts
        };
        self.settings_set(|s| &mut s.tab, tab);
    }

    // ---------------------------------------------------------------- pickers

    fn open_picker(&mut self, purpose: PickFor, under: SettingsHit) {
        let Some((options, current)) = self.picker_options(purpose) else {
            return;
        };
        if options.is_empty() && purpose != PickFor::Find {
            return;
        }
        let cursor = current
            .and_then(|c| options.iter().position(|(_, v)| *v == c))
            .unwrap_or(0);
        let under = self
            .settings_rect(under)
            .unwrap_or_else(|| Rect::new(self.term.size.0 / 3, self.term.size.1 / 3, 1, 1));
        let picker = Picker {
            purpose,
            options,
            cursor,
            under,
            query: (purpose == PickFor::Find).then(String::new),
        };
        self.settings_set(|s| &mut s.picker, Some(picker));
    }

    /// What a picker for `purpose` offers, and the option now in effect.
    fn picker_options(&self, purpose: PickFor) -> Option<(Vec<PickOption>, Option<PickValue>)> {
        let settings = &self.view.settings.model;
        let in_use: Vec<(String, PickValue)> = settings
            .in_use()
            .into_iter()
            .map(|m| {
                (
                    format!("{} · {}", m.name, m.account),
                    PickValue::Model(m.id.clone()),
                )
            })
            .collect();
        Some(match purpose {
            PickFor::Effort(_) | PickFor::AddAt(..) | PickFor::QuartermasterEffort => {
                return effort_options(settings, purpose);
            }
            PickFor::MoveTo(item) => {
                let mut options: Vec<(String, PickValue)> = BOARD
                    .into_iter()
                    .filter(|&t| item.tier() != Some(t))
                    .map(|t| {
                        (
                            format!("Move to {}", tier_label(t)),
                            PickValue::Zone(Zone::Tier(t)),
                        )
                    })
                    .collect();
                if item.tier().is_some() {
                    options.push((String::from("Not in use"), PickValue::Zone(Zone::NotInUse)));
                }
                (options, None)
            }
            PickFor::PinModel => {
                let current = settings.pin.model.clone().map(PickValue::Model);
                (in_use, current)
            }
            PickFor::PinFallback => {
                let pinned = settings.pin.model.clone();
                let mut options = vec![(String::from("None"), PickValue::NoModel)];
                options.extend(in_use.into_iter().filter(|(_, v)| {
                    pinned
                        .as_ref()
                        .is_none_or(|p| *v != PickValue::Model(p.clone()))
                }));
                let current = Some(
                    settings
                        .pin
                        .fallback
                        .clone()
                        .map_or(PickValue::NoModel, PickValue::Model),
                );
                (options, current)
            }
            PickFor::QuartermasterModel => {
                let mut options = vec![(String::from("Auto"), PickValue::Auto)];
                options.extend(in_use);
                let current = Some(match &settings.quartermaster {
                    QuartermasterModel::Model(id) => PickValue::Model(id.clone()),
                    _ => PickValue::Auto,
                });
                (options, current)
            }
            PickFor::Find => {
                let options = settings
                    .models
                    .iter()
                    .filter(|m| m.unused() && !m.listed)
                    .map(|m| {
                        (
                            format!("{} · {}", m.name, m.account),
                            PickValue::Model(m.id.clone()),
                        )
                    })
                    .collect();
                (options, None)
            }
            PickFor::KeyProvider => {
                let options = KEY_PROVIDERS
                    .iter()
                    .map(|&p| (p.to_owned(), PickValue::KeyProvider(p)))
                    .collect();
                (options, None)
            }
        })
    }

    /// Runs an effort picker's choice: a tier entry's effort, the effort a new entry is added at,
    /// or the Quartermaster's.
    fn choose_effort(&mut self, purpose: PickFor, effort: Option<Effort>) {
        match purpose {
            PickFor::Effort(item) => {
                let (Some(model), Some(tier)) = (self.model_id(item), item.tier()) else {
                    return;
                };
                let current = item.effort();
                if current == effort {
                    return;
                }
                self.intend(Command::SetPlacementEffort {
                    model,
                    tier,
                    current,
                    effort,
                });
                let landed = BoardItem::Placed {
                    model: item.model(),
                    tier,
                    effort,
                };
                self.settings_set(|s| &mut s.board, Some(landed));
            }
            PickFor::AddAt(item, tier) => {
                let Some(model) = self.model_id(item) else {
                    return;
                };
                self.intend(Command::PlaceModel {
                    model,
                    from: None,
                    effort,
                    to: tier,
                });
                let landed = BoardItem::Placed {
                    model: item.model(),
                    tier,
                    effort,
                };
                self.settings_set(|s| &mut s.board, Some(landed));
            }
            PickFor::QuartermasterEffort => {
                self.intend(Command::SetQuartermasterEffort { effort });
            }
            _ => {}
        }
    }

    fn choose_pick(&mut self, index: usize) {
        let Some(picker) = self.view.settings.picker.take() else {
            return;
        };
        self.redraw();
        let Some((_, value)) = picker.options.get(index).cloned() else {
            return;
        };
        let pin = self.view.settings.model.pin.clone();
        match (picker.purpose, value) {
            (purpose, PickValue::Effort(effort)) => self.choose_effort(purpose, effort),
            (PickFor::MoveTo(item), PickValue::Zone(zone)) => self.move_item(item, zone),
            (PickFor::PinModel, PickValue::Model(id)) => {
                let fallback = pin.fallback.filter(|f| *f != id);
                self.intend(Command::SetPin {
                    on: true,
                    model: Some(id),
                    fallback,
                });
            }
            (PickFor::PinFallback, value) => {
                let fallback = match value {
                    PickValue::Model(id) => Some(id),
                    _ => None,
                };
                self.intend(Command::SetPin {
                    on: pin.on,
                    model: pin.model,
                    fallback,
                });
            }
            (PickFor::QuartermasterModel, value) => {
                let choice = match value {
                    PickValue::Model(id) => QuartermasterModel::Model(id),
                    _ => QuartermasterModel::Auto,
                };
                self.intend(Command::SetQuartermasterModel { choice });
            }
            (PickFor::Find, PickValue::Model(id)) => {
                let index = self
                    .view
                    .settings
                    .model
                    .models
                    .iter()
                    .position(|m| m.id == id);
                self.intend(Command::ListModel { model: id });
                if let Some(model) = index {
                    self.settings_set(|s| &mut s.board, Some(BoardItem::Unused { model }));
                }
            }
            (PickFor::KeyProvider, PickValue::KeyProvider(provider)) => {
                // A custom endpoint needs its address before its key.
                let purpose = if key_provider(provider) == ProviderId::Custom {
                    FieldFor::BaseUrl
                } else {
                    FieldFor::ApiKey(provider)
                };
                let field = Field {
                    purpose,
                    text: String::new(),
                    base_url: None,
                };
                self.settings_set(|s| &mut s.field, Some(field));
            }
            _ => {}
        }
    }
}

/// The efforts a tier entry for `m` in `tier` can have: the tier default (with the level it comes
/// to on this model), then each level the model supports, lowest first.
fn entry_efforts(
    m: &super::model::Model,
    tier: Tier,
    tiers: &qm_types::config::TierEfforts,
) -> Vec<(String, Option<Effort>)> {
    let mut out = vec![(
        format!(
            "Tier default · {}",
            effort_label(model_tier_default(m, tier, tiers))
        ),
        None,
    )];
    out.extend(
        m.supported
            .iter()
            .map(|&e| (effort_label(e).to_owned(), Some(e))),
    );
    out
}

/// The levels the Quartermaster's effort picker offers: those its model supports (the model you
/// chose, else the one it runs on now), or the whole ladder when that isn't known.
fn quartermaster_levels(settings: &super::model::Settings) -> Vec<Effort> {
    let model = match &settings.quartermaster {
        QuartermasterModel::Model(id) => Some(id),
        _ => settings.quartermaster_now.as_ref().map(|now| &now.model),
    };
    let supported = model
        .and_then(|id| settings.model(id))
        .map(|m| m.supported.clone())
        .unwrap_or_default();
    if supported.is_empty() {
        vec![
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::ExtraHigh,
            Effort::Max,
        ]
    } else {
        supported
    }
}

/// What an effort picker offers, and the option now in effect: a tier entry's effort (leaving out
/// the efforts the model's other entries in that tier hold), the effort a new entry is added at
/// (only those the tier doesn't hold yet), or the Quartermaster's.
fn effort_options(
    settings: &super::model::Settings,
    purpose: PickFor,
) -> Option<(Vec<PickOption>, Option<PickValue>)> {
    Some(match purpose {
        PickFor::Effort(item) => {
            let tier = item.tier()?;
            let m = settings.models.get(item.model())?;
            let own = item.effort();
            // The model's other entries in this tier keep their efforts.
            let options = entry_efforts(m, tier, &settings.tier_efforts)
                .into_iter()
                .filter(|(_, e)| *e == own || m.placement(tier, *e).is_none())
                .map(|(label, e)| (label, PickValue::Effort(e)))
                .collect();
            (options, Some(PickValue::Effort(own)))
        }
        PickFor::AddAt(item, tier) => {
            let m = settings.models.get(item.model())?;
            let options: Vec<PickOption> = entry_efforts(m, tier, &settings.tier_efforts)
                .into_iter()
                .filter(|(_, e)| m.placement(tier, *e).is_none())
                .map(|(label, e)| (format!("Add at {label}"), PickValue::Effort(e)))
                .collect();
            (options, Some(PickValue::Effort(None)))
        }
        PickFor::QuartermasterEffort => {
            let levels = quartermaster_levels(settings);
            let mut options = vec![(
                String::from("Auto · its model's tier default"),
                PickValue::Effort(None),
            )];
            options.extend(
                levels
                    .into_iter()
                    .map(|e| (effort_label(e).to_owned(), PickValue::Effort(Some(e)))),
            );
            (
                options,
                Some(PickValue::Effort(settings.quartermaster_effort)),
            )
        }
        _ => return None,
    })
}
