//! The header at the top of every chat, the Quartermaster's and each crewmate's or secondmate's:
//! the logo, then the app and its version, the model the agent runs on, and where it works.
//!
//! ```text
//! ┌────┐  Quartermaster v0.7.1
//! │logo│  Sonnet 5.5 · High effort
//! └────┘  api-server · qm/task-0042 ← main · supervisor: Quartermaster
//! ```
//!
//! The app's name is teal and bold; the version and the lines under it are dim.
//!
//! The logo is an image where the terminal shows images (`logo::Size::Header`, three rows tall,
//! square on the window's cells); elsewhere a teal `◆` leads the first line, as in the top bar.
//! The header is the chat's first rows and scrolls away with it; the image shows only while all
//! three rows are on screen.

use super::conversation::{Row, RowText};
use crate::app::App;
use crate::hit::FrameMap;
use crate::logo;
use crate::settings::model::effort_label;
use crate::text::{truncate, width};
use crate::theme;
use qm_types::api::SessionView;
use qm_types::domain::Effort;
use qm_types::ids::ModelId;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};

/// The rows the header's text takes, and the logo's height.
pub(crate) const ROWS: u16 = 3;
/// The cells between the logo and the text.
const GAP: u16 = 2;
/// The fewest cells left for the text beside the logo; narrower, the `◆` stands in for it.
const MIN_TEXT: u16 = 20;
/// The app's name on the first line.
const NAME: &str = "Quartermaster";

/// What the header says under the app's name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// "Sonnet 5.5 · High effort".
    pub(crate) model: Option<String>,
    /// The folder or branch it works in.
    pub(crate) place: Option<String>,
}

impl Facts {
    /// The Quartermaster's: the model it runs on now, and the data directory it keeps.
    pub(crate) fn quartermaster(app: &App) -> Self {
        let now = app.view.settings.model.quartermaster_now.as_ref();
        Self {
            model: now.map(|now| model_line(app, &now.model, Some(now.effort))),
            place: app.home.as_deref().map(home_relative),
        }
    }

    /// A crewmate's or secondmate's, from its session view.
    pub(crate) fn session(app: &App, view: &SessionView) -> Self {
        Self {
            model: view
                .model
                .as_ref()
                .map(|model| model_line(app, model, view.effort)),
            place: Some(view.place.as_str())
                .filter(|p| !p.is_empty())
                .map(home_relative),
        }
    }
}

/// "Sonnet 5.5 · High effort", the model named as Settings names it.
fn model_line(app: &App, model: &ModelId, effort: Option<Effort>) -> String {
    let name = app.view.settings.model.model_name(model);
    match effort {
        Some(effort) => format!("{name} · {} effort", effort_label(effort)),
        None => name.to_owned(),
    }
}

/// `text` with your home folder written `~` in the paths in it.
fn home_relative(text: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if home.trim_end_matches('/').len() > 1 => {
            text.replace(&format!("{}/", home.trim_end_matches('/')), "~/")
        }
        _ => text.to_owned(),
    }
}

/// The logo's columns in a chat `max` wide, when the terminal shows images and it leaves the
/// text room; `None` means the `◆`.
fn logo_columns(app: &App, max: u16) -> Option<u16> {
    let columns = app.term.logo?.columns(ROWS);
    (columns + GAP + MIN_TEXT <= max).then_some(columns)
}

/// The header's rows in a chat `max` wide, and the blank row under them.
pub(crate) fn rows(app: &App, facts: &Facts, max: u16) -> Vec<Row> {
    let image = logo_columns(app, max);
    let indent = image.map_or(2, |columns| columns + GAP);
    let room = usize::from(max.saturating_sub(indent));
    let pad = Span::raw(" ".repeat(usize::from(indent)));
    let version = format!(" v{}", env!("CARGO_PKG_VERSION"));

    let mut first = vec![if image.is_some() {
        pad.clone()
    } else {
        Span::styled("◆ ", theme::accent())
    }];
    let name = truncate(NAME, room);
    let mut text = name.clone();
    first.push(Span::styled(
        name,
        theme::accent().add_modifier(Modifier::BOLD),
    ));
    if width(&text) + width(&version) <= room {
        text.push_str(&version);
        first.push(Span::styled(version, theme::dim()));
    }
    let mut out = vec![row(Line::from(first), indent, text)];
    for line in [&facts.model, &facts.place] {
        let shown = line
            .as_deref()
            .map(|l| truncate(l, room))
            .unwrap_or_default();
        out.push(row(
            Line::from(vec![pad.clone(), Span::styled(shown.clone(), theme::dim())]),
            indent,
            shown,
        ));
    }
    out.push(Row::plain(Line::default()));
    out
}

fn row(line: Line<'static>, offset: u16, text: String) -> Row {
    Row {
        line,
        text: Some(RowText {
            offset,
            text,
            continues: false,
        }),
        hits: Vec::new(),
    }
}

/// Reserves the logo's cells where the header's rows were drawn into `area` from row `start`:
/// only while all of its rows show. Call after the rows are drawn.
pub(crate) fn place_logo(
    frame: &mut Frame<'_>,
    area: Rect,
    app: &App,
    start: usize,
    map: &mut FrameMap,
) {
    let Some(columns) = logo_columns(app, area.width) else {
        return;
    };
    if start > 0 || area.height < ROWS {
        return;
    }
    let spot = Rect::new(area.x, area.y, columns, ROWS);
    map.logos
        .push(logo::reserve(frame.buffer_mut(), logo::Size::Header, spot));
}
