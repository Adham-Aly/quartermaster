//! Hit testing: what a click at a cell would act on.
//!
//! Each frame records the clickable regions it drew, in drawing order; a later region wins over
//! an earlier one it overlaps, so an overlay drawn last captures the pointer. The same map drives
//! hover highlighting, so whatever lights up under the pointer is exactly what a click would do.
//!
//! Drawing code records regions here; input code reads them.

use qm_types::api::AgentRef;
use ratatui::layout::{Position, Rect};
use serde::{Deserialize, Serialize};

/// A pane of the Bridge that can take focus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Pane {
    Projects,
    Crew,
    Conversation,
    Input,
    Task,
    /// A worker's session box, in the middle panel in place of the conversation (its input is
    /// `Input`).
    Session,
    /// The Settings popup's tab: it scrolls.
    Settings,
    /// The Help popup: it scrolls.
    Help,
    /// The task-kind explanations popup: it scrolls independently of Help.
    TaskKinds,
    /// The Actions inbox, below the crew.
    Actions,
    /// The Actions overlay's body: it scrolls.
    ActionOverlay,
}

/// A draggable border between two Bridge columns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Divider {
    /// Between the left column (projects and crew) and the conversation.
    Left,
    /// Between the conversation and the task pane.
    Right,
}

/// A status-bar item; a click jumps to what it counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusItem {
    Approvals,
    Decisions,
    Running,
}

/// A button in the task pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TaskButton {
    OpenSession,
    OpenReport,
    Stop,
}

/// Something a click acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Hit {
    /// The top bar's menu button.
    MenuButton,
    /// The update notice beside the menu button: opens the Update popup.
    UpdateNotice,
    /// The Update popup's button.
    UpdateButton,
    /// A popup window, its close button, or outside it.
    Popup(crate::widgets::popup::PopupHit),
    /// A pane: a click focuses it.
    Pane(Pane),
    /// A border: drag it to resize the columns beside it.
    Divider(Divider),
    /// The quit dialog's "Quit" button.
    ConfirmQuit,
    /// The quit dialog's "Cancel" button.
    CancelQuit,
    /// Inside a dialog or menu but on no button: swallows the click.
    Dialog,
    /// A project row, by index into `Snapshot::projects`.
    Project(usize),
    /// A crew row.
    Crew(AgentRef),
    /// Row `index` of the Actions inbox, by index into `Snapshot::actions`.
    ActionRow(usize),
    /// Button `index` of the Actions overlay: the card's buttons, then "Go to" when it has one.
    ActionButton(usize),
    /// A task-pane button.
    TaskButton(TaskButton),
    /// A status-bar item.
    Status(StatusItem),
    /// The folded task overlay's close mark.
    CloseOverlay,
    /// Item `index` of the open context menu.
    MenuItem(usize),
    /// Row `index` of the slash-command menu.
    SlashItem(usize),
    /// Something in the Settings popup.
    Settings(crate::settings::SettingsHit),
    /// Something on the setup check screen.
    Setup(crate::setup::SetupHit),
    /// Something in a worker's session.
    Session(crate::session::SessionHit),
    /// Something on the Diff screen.
    Diff(crate::diff::DiffHit),
}

/// The clickable regions of one frame.
#[derive(Debug, Default)]
pub(crate) struct Hits {
    regions: Vec<(Rect, Hit)>,
}

impl Hits {
    /// Records a region; later regions win.
    pub(crate) fn add(&mut self, area: Rect, hit: Hit) {
        if area.width > 0 && area.height > 0 {
            self.regions.push((area, hit));
        }
    }

    /// What a click at `(column, row)` acts on.
    pub(crate) fn at(&self, column: u16, row: u16) -> Option<Hit> {
        let position = Position::new(column, row);
        self.regions
            .iter()
            .rev()
            .find(|(area, _)| area.contains(position))
            .map(|&(_, hit)| hit)
    }

    /// Where `hit` was drawn, if it was (the last region recorded for it).
    pub(crate) fn rect_of(&self, hit: Hit) -> Option<Rect> {
        self.regions
            .iter()
            .rev()
            .find(|(_, h)| *h == hit)
            .map(|&(area, _)| area)
    }
}

/// One drawn row of selectable text, for the app's own selection and copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextRow {
    /// The screen row and the first column the text starts at.
    pub(crate) at: Position,
    /// The row's text exactly as drawn; columns are measured with `unicode-width`, so a wide
    /// glyph covers two cells.
    pub(crate) text: String,
    /// This row continues the previous one after a soft wrap: copy joins them with a space
    /// instead of a newline.
    pub(crate) continues: bool,
}

/// What one frame drew that input handling needs afterwards.
#[derive(Debug, Default)]
pub(crate) struct FrameMap {
    /// The clickable regions.
    pub(crate) hits: Hits,
    /// Whether the task pane was folded away (the terminal is narrower than about 120 columns).
    pub(crate) folded: bool,
    /// How many lines the conversation can scroll up from the bottom.
    pub(crate) conversation_scroll_max: usize,
    /// The panes the wheel scrolls, by area; later entries win.
    pub(crate) scroll_areas: Vec<(Rect, Pane)>,
    /// How far each scrollable pane other than the conversation can scroll down from its top.
    pub(crate) scroll_max: Vec<(Pane, usize)>,
    /// Selectable text, in screen order (top to bottom, left to right).
    pub(crate) text: Vec<TextRow>,
    /// The Settings tab's selected row, as (rows down from the tab's top, rows the tab shows):
    /// what the keys keep in view.
    pub(crate) settings_selected: Option<(usize, usize)>,
    /// Where a worker's session drew its scrolling rows, when one showed.
    pub(crate) session_rows: Option<Rect>,
    /// The prompt field, when one that takes text was drawn.
    pub(crate) field: Option<crate::input::field::Drawn>,
}

impl FrameMap {
    /// The pane the wheel scrolls at `(column, row)`.
    pub(crate) fn scroll_pane_at(&self, column: u16, row: u16) -> Option<Pane> {
        let position = Position::new(column, row);
        self.scroll_areas
            .iter()
            .rev()
            .find(|(area, _)| area.contains(position))
            .map(|&(_, pane)| pane)
    }

    /// How far `pane` can scroll from its top (0 when it fits or wasn't drawn).
    pub(crate) fn scroll_max_of(&self, pane: Pane) -> usize {
        self.scroll_max
            .iter()
            .find(|(p, _)| *p == pane)
            .map_or(0, |&(_, max)| max)
    }
}
