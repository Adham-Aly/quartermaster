//! Settings, shown in a popup, and the connect-an-account flow.
//!
//! Tabs: Accounts, Model levels (the tier board), Routing (the pin and the Quartermaster's
//! model), Projects and System, plus the connect-an-account flow opened from Accounts. Every
//! control is clickable and has a key. The screens draw from [`model::Settings`], built from
//! `qm_types::api`'s settings contract, and turn what you do into `api::Command`s.
//!
//! - `draw` (here) lays out the sub-tabs and the tab inside the Settings popup (`popups`); the
//!   tab scrolls when it's taller than the popup; `paint` is the small painter every tab draws through; the hints are the status
//!   bar's (`status_hints`).
//! - `accounts`, `levels`, `routing`, `other` and `connect` draw one tab each.
//! - `overlay` draws the pickers (effort, model, move-to, find) and the one-line text fields.
//! - `input` holds the keys, clicks, drags and context-menu choices.

mod accounts;
mod connect;
mod input;
mod levels;
pub(crate) mod model;
mod other;
mod overlay;
mod paint;
mod routing;
mod sync;

use crate::app::App;
use crate::hit::{FrameMap, Pane};
use crate::text::cells;
use crate::theme;
use model::Settings;
use paint::Painter;
use qm_types::api::Command;
use qm_types::domain::Effort;
use qm_types::domain::Tier;
use qm_types::ids::ModelId;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use serde::{Deserialize, Serialize};
use std::time::Instant;

pub(crate) use input::menu_items;

/// How many rows the Routing tab has: `SettingsView::routing_row` is below it.
pub(crate) const ROUTING_ROWS: usize = 3;

/// A Settings tab.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Tab {
    #[default]
    Accounts,
    ModelLevels,
    Routing,
    Projects,
    System,
}

impl Tab {
    pub(crate) const ALL: [Self; 5] = [
        Self::Accounts,
        Self::ModelLevels,
        Self::Routing,
        Self::Projects,
        Self::System,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Accounts => "Accounts",
            Self::ModelLevels => "Model levels",
            Self::Routing => "Routing",
            Self::Projects => "Projects",
            Self::System => "System",
        }
    }
}

/// One item on the tier board: a tier entry (a model at an effort in a tier; `None` is the tier
/// default), or a model in NOT IN USE. The model is an index into `Settings::models`. One model
/// may have several entries in a tier, one per effort.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BoardItem {
    Placed {
        model: usize,
        tier: Tier,
        effort: Option<Effort>,
    },
    Unused {
        model: usize,
    },
}

impl BoardItem {
    pub(crate) const fn model(self) -> usize {
        match self {
            Self::Placed { model, .. } | Self::Unused { model } => model,
        }
    }

    pub(crate) const fn tier(self) -> Option<Tier> {
        match self {
            Self::Placed { tier, .. } => Some(tier),
            Self::Unused { .. } => None,
        }
    }

    /// A tier entry's effort (`None` is the tier default, and every NOT IN USE item).
    pub(crate) const fn effort(self) -> Option<Effort> {
        match self {
            Self::Placed { effort, .. } => effort,
            Self::Unused { .. } => None,
        }
    }
}

/// A drop zone on the board: a tier, or NOT IN USE.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Zone {
    Tier(Tier),
    NotInUse,
}

/// Something in the Settings popup a click acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SettingsHit {
    Tab(Tab),
    /// An account row, and its buttons.
    Account(usize),
    AccountLabel(usize),
    AccountPause(usize),
    AccountDisconnect(usize),
    /// "Reconnect", on an account whose login stopped working.
    AccountReconnect(usize),
    /// "+ Connect an account".
    Connect,
    /// A board item, its effort dropdown, a drop zone, and "Find another model…".
    Board(BoardItem),
    Effort(BoardItem),
    Zone(Zone),
    Find,
    /// Routing: the pin's switch, its model and fallback, the Quartermaster's model and effort,
    /// and the autocompact window's two arrows (the one at an end is drawn dim and not a target).
    PinSwitch,
    PinModel,
    PinFallback,
    QuartermasterModel,
    QuartermasterEffort,
    AutocompactDown,
    AutocompactUp,
    /// System: the notifications switch and the setup check's "Open".
    NotificationsSwitch,
    SetupCheck,
    /// Connect: a provider row and its button, "Add key", and the login's buttons.
    Provider(usize),
    ProviderButton(usize),
    AddKey,
    OpenBrowser,
    CopyCode,
    CancelLogin,
    RetryLogin,
    ReviewTiers,
    DoneLogin,
    /// An overlay: a picker row, and the overlay's box (swallows the click).
    PickerItem(usize),
    FieldSave,
    FieldCancel,
    Overlay,
    /// Under an open picker or field, everywhere else: a click there closes it.
    Backdrop,
}

/// An account a context menu was opened on, with what its items depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AccountTarget {
    pub(crate) index: usize,
    pub(crate) paused: bool,
    pub(crate) preferred: bool,
    pub(crate) follows_cli: bool,
    pub(crate) login: AccountLogin,
}

/// Whether an account's login works; a broken one's menu offers Reconnect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AccountLogin {
    Working,
    Broken,
}

/// What a picker chooses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PickFor {
    Effort(BoardItem),
    /// The effort a new entry for this model goes into the tier at ("Add to tier ▸ Low ▸ High").
    AddAt(BoardItem, Tier),
    MoveTo(BoardItem),
    PinModel,
    PinFallback,
    QuartermasterModel,
    QuartermasterEffort,
    Find,
    KeyProvider,
}

/// One choice in a picker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PickValue {
    Effort(Option<Effort>),
    Zone(Zone),
    Model(ModelId),
    /// No model ("None" for the fallback).
    NoModel,
    Auto,
    KeyProvider(&'static str),
}

/// An open picker: a small list under the control that opened it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Picker {
    pub(crate) purpose: PickFor,
    pub(crate) options: Vec<(String, PickValue)>,
    pub(crate) cursor: usize,
    /// The control it opened from (its top-left corner sits just under it).
    pub(crate) under: Rect,
    /// The find query, for the find picker.
    pub(crate) query: Option<String>,
}

impl Picker {
    /// The options the query lets through, with their indices into `options`.
    pub(crate) fn visible(&self) -> Vec<usize> {
        let query = self.query.as_deref().unwrap_or("").to_lowercase();
        self.options
            .iter()
            .enumerate()
            .filter(|(_, (label, _))| query.is_empty() || label.to_lowercase().contains(&query))
            .map(|(i, _)| i)
            .collect()
    }
}

/// What a one-line text field is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FieldFor {
    /// A new label for account `usize`.
    Label(usize),
    /// An API key for a provider.
    ApiKey(&'static str),
    /// A new API key for account `usize`, whose key stopped working.
    ReconnectKey(usize),
    /// A custom endpoint's base URL, asked before its key.
    BaseUrl,
}

/// An open text field.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Field {
    pub(crate) purpose: FieldFor,
    pub(crate) text: String,
    /// The custom endpoint's base URL, carried from its field into the key's.
    pub(crate) base_url: Option<String>,
}

/// Never prints what you typed into a key field.
impl std::fmt::Debug for Field {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self.purpose {
            FieldFor::ApiKey(_) | FieldFor::ReconnectKey(_) => "<redacted>",
            FieldFor::Label(_) | FieldFor::BaseUrl => self.text.as_str(),
        };
        f.debug_struct("Field")
            .field("purpose", &self.purpose)
            .field("text", &text)
            .field("base_url", &self.base_url)
            .finish()
    }
}

/// A model being dragged on the board, and the zone under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Drag {
    pub(crate) item: BoardItem,
    pub(crate) over: Option<Zone>,
}

/// The Settings popup's state, held in `View::settings`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SettingsView {
    /// What the screens show: the core's settings, or the developer sample.
    pub(crate) model: Settings,
    /// `model` holds real data: the core's first snapshot (or the sample) has arrived.
    pub(crate) loaded: bool,
    /// Changes sent to the core that no snapshot has shown yet.
    pub(crate) pending: Vec<sync::Pending>,
    pub(crate) tab: Tab,
    /// The connect-an-account flow is showing (over Accounts).
    pub(crate) connecting: bool,
    pub(crate) account: Option<usize>,
    pub(crate) board: Option<BoardItem>,
    /// Routing: 0 the pin, 1 the Quartermaster, 2 the autocompact window (`ROUTING_ROWS` in all).
    pub(crate) routing_row: usize,
    /// Connect: a provider row, or `providers.len()` for the API-key row.
    pub(crate) provider: Option<usize>,
    pub(crate) drag: Option<Drag>,
    pub(crate) picker: Option<Picker>,
    pub(crate) field: Option<Field>,
    /// The clock as of the last tick, for the login's spinner and countdown (`None` in
    /// snapshots: the first frame and the full time).
    pub(crate) now: Option<Instant>,
    /// Changes waiting to be sent to the core (the app drains them each loop).
    pub(crate) outbox: Vec<Command>,
    /// How many rows the tab is scrolled down.
    pub(crate) scroll: usize,
    /// The keys moved the selection: scroll it into view after the next frame.
    pub(crate) reveal: bool,
}

/// Draws Settings into `body`, the inside of its popup: the sub-tabs on the first row (not while
/// connecting), then the tab. The tab's hints are the status bar's (`status_hints`). A tab
/// taller than the body scrolls.
pub(crate) fn draw(frame: &mut Frame<'_>, body: Rect, app: &App, map: &mut FrameMap) {
    let view = &app.view.settings;
    let mut p = Painter::new(
        frame,
        &mut map.hits,
        app.term.hover,
        // A model held in a drag stays filled until it drops.
        !app.view.pointer_picked || app.view.settings.drag.is_some(),
        body,
    );
    let content = if view.connecting {
        body
    } else {
        // System holds the setup check's findings: its tab carries their dot.
        let badged = (!app.snapshot.setup_issues.is_empty()).then_some(Tab::System);
        draw_tabs(&mut p, body.x, body.y, view.tab, badged);
        // A rule under the tabs sets them apart from the tab's content.
        let rule = "─".repeat(usize::from(body.width));
        p.text(body.x, body.y + 1, &rule, theme::dim());
        Rect::new(
            body.x,
            body.y + 2,
            body.width,
            body.height.saturating_sub(2),
        )
    };
    // The page runs on below the screen; the painter scrolls and clips it.
    let page = Rect::new(
        content.x,
        content.y,
        content.width,
        u16::MAX / 2 - content.y,
    );
    p.view(content, 0, true);
    draw_page(&mut p, page, app);
    let max = p.bottom.saturating_sub(content.bottom());
    let scroll = u16::try_from(view.scroll).unwrap_or(u16::MAX).min(max);
    p.view(content, scroll, false);
    draw_page(&mut p, page, app);
    let selected = p
        .selected
        .map(|y| (usize::from(y - content.y), usize::from(content.height)));
    p.view(body, 0, false);
    overlay::draw(&mut p, body, app);
    map.scroll_areas.push((content, Pane::Settings));
    map.scroll_max.push((Pane::Settings, usize::from(max)));
    map.settings_selected = selected;
}

/// The current tab, or the connect flow, in page coordinates.
fn draw_page(p: &mut Painter<'_, '_>, page: Rect, app: &App) {
    let view = &app.view.settings;
    if let Some(reason) = view
        .model
        .unavailable
        .as_deref()
        .filter(|_| needs_core(view))
    {
        draw_unavailable(p, page, view, reason);
    } else if view.connecting {
        connect::draw(p, page, app);
    } else if !view.loaded && matches!(view.tab, Tab::Accounts | Tab::ModelLevels | Tab::Routing) {
        draw_loading(p, page);
    } else {
        match view.tab {
            Tab::Accounts => accounts::draw(p, page, app),
            Tab::ModelLevels => levels::draw(p, page, app),
            Tab::Routing => routing::draw(p, page, app),
            Tab::Projects => other::draw_projects(p, page, app),
            Tab::System => other::draw_system(p, page, app),
        }
    }
}

/// The status bar's two sides on Settings: what the mouse does, and the keys.
pub(crate) fn status_hints(app: &App) -> (&'static str, &'static str) {
    let view = &app.view.settings;
    if needs_core(view) && view.model.unavailable.is_some() {
        ("", "tab next · esc close")
    } else if view.connecting {
        connect::hints(view)
    } else if !view.loaded && matches!(view.tab, Tab::Accounts | Tab::ModelLevels | Tab::Routing) {
        ("", "tab next · esc close")
    } else {
        hints(view.tab)
    }
}

/// Whether what's showing comes from the core's settings part: the connect flow, Accounts, Model
/// levels and Routing (Projects and System have their own sources).
fn needs_core(view: &SettingsView) -> bool {
    view.connecting || matches!(view.tab, Tab::Accounts | Tab::ModelLevels | Tab::Routing)
}

/// The tab (or the connect flow's box) when the core's settings part didn't start: the reason, wrapped, in place of the
/// tab.
fn draw_unavailable(p: &mut Painter<'_, '_>, area: Rect, view: &SettingsView, reason: &str) {
    let title = if view.connecting {
        "Connect an account"
    } else {
        view.tab.label()
    };
    let room = area.width.saturating_sub(4).max(1);
    let runs = [(reason.to_owned(), theme::warn())];
    let rows = cells(crate::text::wrap_styled(&runs, usize::from(room)).len());
    let inner = if view.connecting {
        boxed(p, area, title, rows, theme::dim(), theme::dim())
    } else {
        section(area, rows)
    };
    let text = Rect::new(
        inner.x + 1,
        inner.y,
        inner.width.saturating_sub(2),
        inner.height,
    );
    paragraph(p, text, &runs);
}

/// The tab before the core has sent its settings: says so, rather than look empty.
fn draw_loading(p: &mut Painter<'_, '_>, area: Rect) {
    let inner = section(area, 1);
    p.text(inner.x + 1, inner.y, "Loading settings…", theme::dim());
}

/// The sub-tabs on row `y`, from column `left`; `badged` shows a red dot after its label.
fn draw_tabs(p: &mut Painter<'_, '_>, left: u16, y: u16, current: Tab, badged: Option<Tab>) {
    let mut x = left;
    for tab in Tab::ALL {
        let label = format!(" {} ", tab.label());
        let hit = SettingsHit::Tab(tab);
        let style = if tab == current {
            theme::filled().add_modifier(Modifier::BOLD)
        } else if p.hovered(hit) {
            theme::dim().add_modifier(Modifier::UNDERLINED)
        } else {
            theme::dim()
        };
        let mut end = p.text(x, y, &label, style);
        if badged == Some(tab) {
            end = p.text(
                end,
                y,
                crate::widgets::dropdown::BADGE,
                style.patch(theme::badge()),
            );
            end = p.text(end, y, " ", style);
        }
        p.hit(Rect::new(x, y, end - x, 1), hit);
        x = end;
    }
}

/// The hint row's text per tab.
const fn hints(tab: Tab) -> (&'static str, &'static str) {
    match tab {
        Tab::Accounts => (
            "right-click an account for more",
            "⏎ open · p pause · * preferred · esc close",
        ),
        Tab::ModelLevels => (
            "drag to move · right-click for options",
            "m move · x remove · e effort · / find",
        ),
        Tab::Routing => (
            "click to change",
            "space pin · c model · f fallback · q quartermaster · e effort · −/+ window · esc",
        ),
        Tab::System => (
            "click to change",
            "n notifications · s setup check · esc close",
        ),
        Tab::Projects => ("click a tab", "tab next · esc close"),
    }
}

/// A tab's content area: `rows` rows at the top of `area`, unframed. The popup's border and the
/// tab bar already say where you are, so a tab draws flat, its text one cell in, in line with
/// the tab labels.
const fn section(area: Rect, rows: u16) -> Rect {
    let height = if rows < area.height {
        rows
    } else {
        area.height
    };
    Rect::new(area.x, area.y, area.width, height)
}

/// A rounded box titled `title`, `rows` rows of content tall (clamped to `area`); returns the
/// inner area.
fn boxed(
    p: &mut Painter<'_, '_>,
    area: Rect,
    title: &str,
    rows: u16,
    border: Style,
    title_style: Style,
) -> Rect {
    let height = (rows + 2).min(area.height);
    let rect = Rect::new(area.x, area.y, area.width, height);
    p.frame_box(rect, title, border, title_style);
    Rect::new(
        rect.x + 1,
        rect.y + 1,
        rect.width.saturating_sub(2),
        rect.height.saturating_sub(2),
    )
}

/// A wrapped paragraph of styled runs inside `area`, from its top; returns the rows used.
fn paragraph(p: &mut Painter<'_, '_>, area: Rect, runs: &[(String, Style)]) -> u16 {
    let rows = crate::text::wrap_styled(runs, usize::from(area.width.max(1)));
    let n = cells(rows.len()).min(area.height);
    for (y, row) in (area.y..area.y + n).zip(rows) {
        let mut x = area.x;
        for (text, style) in row {
            x = p.text(x, y, &text, style);
        }
    }
    n
}
