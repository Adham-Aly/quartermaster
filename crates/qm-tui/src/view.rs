//! UI state that drawing reads and input changes. Input code is the only writer; drawing code
//! only reads.

use qm_types::api::AgentRef;
use qm_types::ids::{HeldActionId, MessageId};
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
    /// A model on the Settings tier board, its "Add to tier" submenu, and an account.
    Model(crate::settings::BoardItem),
    AddToTier(crate::settings::BoardItem),
    Account(crate::settings::AccountTarget),
    /// The top bar's menu: its entries are `popups::MenuEntry::ALL`.
    Main,
}

/// An open context menu: its items, top-left corner and the item under the keyboard cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Menu {
    pub(crate) target: MenuTarget,
    pub(crate) at: Position,
    pub(crate) items: Vec<&'static str>,
    pub(crate) cursor: usize,
}

/// A short notice in the status bar ("Copied 3 lines"), shown until `until`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Flash {
    pub(crate) text: String,
    pub(crate) until: Instant,
}

/// The UI state, held in `App::view`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct View {
    /// The highlighted crew row; its task fills the task pane.
    pub(crate) selected_crew: Option<AgentRef>,
    /// The highlighted project row, by index into `Snapshot::projects`.
    pub(crate) selected_project: Option<usize>,
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
    /// The card the keys `a`/`d`/`r` and `1`/`2`/`3` act on: the newest unanswered card unless
    /// you moved to another.
    pub(crate) card: Option<MessageId>,
    /// "Retry with my guidance" picked: the input box collects your words for this held action.
    pub(crate) guidance_for: Option<HeldActionId>,
    /// "Change location" picked on a project proposal: the input box holds the folder for this held
    /// action.
    pub(crate) location_for: Option<HeldActionId>,
    /// The app's own text selection, while dragging and until the next click.
    pub(crate) selection: Option<Selection>,
    pub(crate) menu: Option<Menu>,
    pub(crate) flash: Option<Flash>,
    /// The prompt field's first shown row in view, as the last frame drew it.
    pub(crate) input_scroll: usize,
    /// The sample fixture is showing: core snapshots are ignored.
    pub(crate) sample: bool,
    /// The Settings popup's state.
    pub(crate) settings: crate::settings::SettingsView,
    /// The worker-session screen: its scroll, expanded lines and drafts.
    pub(crate) session: Option<crate::session::SessionUi>,
    /// The sessions' expanded tool-call runs. Shared state, not per window: keyed by the run,
    /// never by screen position, and kept when you leave a session.
    pub(crate) tool_groups: std::collections::HashSet<crate::session::ToolGroupKey>,
    /// How far the Diff screen is scrolled down from its first line.
    pub(crate) diff_scroll: usize,
    /// The popup open over the screen, if any.
    pub(crate) popup: Option<crate::popups::Popup>,
    /// The menu entries showing a badge (a red dot): something there needs you.
    pub(crate) badges: Vec<crate::popups::MenuEntry>,
    /// How far Help is scrolled down from its top.
    pub(crate) help_scroll: usize,
}
