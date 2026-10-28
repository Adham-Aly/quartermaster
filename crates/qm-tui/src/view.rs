//! UI state that drawing reads and input changes. Input code is the only writer; drawing code
//! only reads.

use qm_types::api::AgentRef;
use qm_types::ids::HeldActionId;
use ratatui::layout::Position;
use std::time::Instant;

/// The app's own text selection, in screen cells, from where the drag started to where the
/// pointer is now (either may come first on screen).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) anchor: Position,
    pub(crate) head: Position,
}

/// What a context menu was opened on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuTarget {
    Crew(AgentRef),
    Project(usize),
    /// An Actions inbox row, by index into `Snapshot::actions`.
    Action(usize),
    /// A placed combination on the Settings tier board.
    Model(crate::settings::BoardItem),
    Account(crate::settings::AccountTarget),
    /// The top bar's menu: its entries are `popups::MenuEntry::ALL`.
    Main,
}

impl MenuTarget {
    /// The row (or button) a menu on this target is anchored to, for `Anchor::Row`.
    pub(crate) const fn row(self) -> crate::hit::Hit {
        use crate::hit::Hit;
        use crate::settings::SettingsHit;
        match self {
            Self::Crew(agent) => Hit::Crew(agent),
            Self::Project(i) => Hit::Project(i),
            Self::Action(i) => Hit::ActionRow(i),
            Self::Model(item) => Hit::Settings(SettingsHit::Board(item)),
            Self::Account(account) => Hit::Settings(SettingsHit::Account(account.index)),
            Self::Main => Hit::MenuButton,
        }
    }
}

/// An open menu: what it is on, what it is placed against (never screen cells), its items and
/// the item under the keyboard cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Menu {
    pub(crate) target: MenuTarget,
    pub(crate) anchor: crate::popups::Anchor,
    pub(crate) items: Vec<&'static str>,
    pub(crate) cursor: usize,
}

/// A short notice in the status bar ("Copied 3 lines"), shown until `until`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Flash {
    pub(crate) text: String,
    /// Opaque diagnostic copied from a card line, not app-owned metadata.
    pub(crate) verbatim: bool,
    pub(crate) until: Instant,
}

/// What the Bridge's middle panel shows: the Quartermaster's conversation, or a worker's
/// session in its place, the left and right columns staying as they are.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Middle {
    #[default]
    Quartermaster,
    /// A crewmate's or secondmate's session, opened from the crew or the task pane.
    Session(AgentRef),
}

/// How long a press in the Actions overlay has been waiting on the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PressWait {
    pub(crate) at: Instant,
    /// The app has shown the request being carried out since.
    pub(crate) saw_busy: bool,
}

/// The UI state, held in `App::view`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct View {
    /// The highlighted crew row; its task fills the task pane.
    pub(crate) selected_crew: Option<AgentRef>,
    /// The highlighted project row, by index into `Snapshot::projects`.
    pub(crate) selected_project: Option<usize>,
    /// The project whose details show under its line, by name (Enter or a click toggles it).
    pub(crate) open_project: Option<String>,
    /// The mouse was used last: selected rows are marked with `▸` instead of filled, so a click
    /// leaves no shading behind once the pointer moves on. A key that moves through a list
    /// brings the fills back (like a browser's `:focus-visible`). See `widgets::selection`.
    pub(crate) pointer_picked: bool,
    /// Folded layout only: the task overlay is open.
    pub(crate) task_overlay: bool,
    /// How far the projects, crew and task panes are scrolled down from their tops.
    pub(crate) projects_scroll: usize,
    pub(crate) crew_scroll: usize,
    pub(crate) task_scroll: usize,
    /// The highlighted Actions row. It follows its item across snapshots; never persisted, since
    /// items are momentary.
    pub(crate) selected_action: Option<qm_types::api::ActionKey>,
    pub(crate) actions_scroll: usize,
    /// The Needs you items you have seen (`widgets::beacon`); the beacon is lit while any other
    /// waits. Never persisted: a new window shows what waits as new.
    pub(crate) actions_seen: Vec<qm_types::api::ActionKey>,
    /// The item the Actions overlay shows, while it is open.
    pub(crate) action_open: Option<qm_types::api::ActionKey>,
    /// The overlay's highlighted button: none when it opens, so a second Enter presses nothing.
    pub(crate) action_button: Option<usize>,
    /// You pressed a button on this item: its buttons dim until it goes or the attempt ends
    /// (see `actions::after_press`), and its overlay closes without the "answered elsewhere"
    /// flash.
    pub(crate) action_pressed: Option<qm_types::api::ActionKey>,
    /// When you pressed it, and whether the app has shown it being carried out since.
    pub(crate) action_wait: Option<PressWait>,
    /// How far the overlay's body is scrolled.
    pub(crate) action_scroll: usize,
    /// The Actions item whose Diff button opened the Diff screen: Esc there reopens its overlay.
    pub(crate) diff_from: Option<qm_types::api::ActionKey>,
    /// "Retry with my guidance" picked: the input box collects your words for this held action.
    pub(crate) guidance_for: Option<HeldActionId>,
    /// "Change location" picked on a project proposal: the input box holds the folder for this held
    /// action.
    pub(crate) location_for: Option<HeldActionId>,
    pub(crate) menu: Option<Menu>,
    /// The prompt field's first shown row in view, as the last frame drew it.
    pub(crate) input_scroll: usize,
    /// The sample fixture is showing: core snapshots are ignored.
    pub(crate) sample: bool,
    /// The Settings popup's state.
    pub(crate) settings: crate::settings::SettingsView,
    /// What the middle panel shows. Shared state, not per window, so every window shows the
    /// same session.
    pub(crate) middle: Middle,
    /// The sessions' scroll, expanded lines and drafts.
    pub(crate) session: Option<crate::session::SessionUi>,
    /// The sessions' expanded tool-call runs. Shared state, not per window: keyed by the run,
    /// never by screen position, and kept when you leave a session.
    pub(crate) tool_groups: std::collections::HashSet<crate::session::ToolGroupKey>,
    /// The diff viewer's transient file, focus and scrolling state.
    pub(crate) diff: crate::diff::DiffUi,
    /// The popups open over the screen, top last: shareable state with no screen coordinates.
    pub(crate) popups: Vec<crate::popups::OpenPopup>,
    /// The menu entries showing a badge (a red dot): something there needs you.
    pub(crate) badges: Vec<crate::popups::MenuEntry>,
    /// How far Help is scrolled down from its top.
    pub(crate) help_scroll: usize,
    /// How far the task-kind explanations are scrolled down from their top.
    pub(crate) kind_help_scroll: usize,
    /// The slash-command menu above the Quartermaster's input box.
    pub(crate) slash: crate::input::slash::SlashUi,
}
