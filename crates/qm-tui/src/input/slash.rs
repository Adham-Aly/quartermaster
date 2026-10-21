//! The Quartermaster's slash commands: the registry, the command menu's state, and its keys.
//!
//! Typing `/` at the start of your message to the Quartermaster opens the menu above the input
//! box. It lists [`COMMANDS`] with their descriptions and narrows as you type the name; `↑`/`↓`
//! (or the pointer) move the highlight, `Tab` completes the highlighted name, `⏎` (or a click)
//! runs it, and `Esc` closes the menu until you change what you typed. Only the Quartermaster's
//! chat on the Bridge has slash commands: a worker's session and the card modes (guidance, a
//! project's location) never show the menu.
//!
//! Adding a command is one entry here, a [`SlashCommand`] variant in `qm_types::api`, and the
//! core's handling of it (`qm-core` `turns/slash.rs`).

use crate::app::{App, Quit};
use crate::hit::Pane;
use crate::router::Screen;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use qm_types::api::{Command, SlashCommand};

/// One slash command, as the menu lists it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlashEntry {
    /// The name after the `/`.
    pub(crate) name: &'static str,
    /// What it does, in a few words.
    pub(crate) description: &'static str,
    pub(crate) command: SlashCommand,
}

/// Every slash command, in the menu's order.
pub(crate) const COMMANDS: &[SlashEntry] = &[
    SlashEntry {
        name: "clear",
        description: "Clear the Quartermaster's context and start a new session",
        command: SlashCommand::Clear,
    },
    SlashEntry {
        name: "compact",
        description: "Compact the Quartermaster's context to free up room",
        command: SlashCommand::Compact,
    },
];

/// The commands for `typed` (what follows the `/`): names that start with it first, then names
/// that contain it, each in registry order. Case doesn't matter.
pub(crate) fn matching(typed: &str) -> Vec<&'static SlashEntry> {
    let typed = typed.to_lowercase();
    let starts = COMMANDS.iter().filter(|c| c.name.starts_with(&typed));
    let contains = COMMANDS
        .iter()
        .filter(|c| !c.name.starts_with(&typed) && c.name.contains(&typed));
    starts.chain(contains).collect()
}

/// The command `input` names exactly (`/clear`, surrounding blanks ignored).
pub(crate) fn exact(input: &str) -> Option<&'static SlashEntry> {
    let name = input.trim().strip_prefix('/')?;
    COMMANDS.iter().find(|c| c.name.eq_ignore_ascii_case(name))
}

/// The command name being typed: what follows a leading `/`, while it is one word.
fn typed_name(input: &str) -> Option<&str> {
    let name = input.strip_prefix('/')?;
    (!name.chars().any(char::is_whitespace)).then_some(name)
}

/// The menu's state, in `View::slash`. Both fields hold the name they were set for, so typing
/// something else resets them without any bookkeeping.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SlashUi {
    /// The highlighted row, and the name it was moved for.
    pub(crate) cursor: Option<(String, usize)>,
    /// `Esc` closed the menu for this name.
    pub(crate) dismissed: Option<String>,
}

/// The open menu, as drawing and keys see it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SlashMenu {
    pub(crate) entries: Vec<&'static SlashEntry>,
    pub(crate) cursor: usize,
}

impl App {
    /// The command menu, when it shows: on the Bridge, with your message to the Quartermaster
    /// focused and starting with `/` and a part of a command's name.
    pub(crate) fn slash_menu(&self) -> Option<SlashMenu> {
        if self.screen != Screen::Bridge
            || self.shown_session().is_some()
            || self.focus != Pane::Input
            || self.view.guidance_for.is_some()
            || self.view.location_for.is_some()
            || self.view.menu.is_some()
            || self.popup().is_some()
            || matches!(self.term.quit, Quit::Confirm { .. })
        {
            return None;
        }
        let typed = typed_name(&self.draft.text)?;
        if self.view.slash.dismissed.as_deref() == Some(typed) {
            return None;
        }
        let entries = matching(typed);
        if entries.is_empty() {
            return None;
        }
        let cursor = match &self.view.slash.cursor {
            Some((name, at)) if name == typed => (*at).min(entries.len() - 1),
            _ => 0,
        };
        Some(SlashMenu { entries, cursor })
    }

    /// A key while the menu shows; `false` leaves it to the input box.
    pub(crate) fn on_slash_key(&mut self, key: KeyEvent) -> bool {
        let Some(menu) = self.slash_menu() else {
            return false;
        };
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT | KeyModifiers::CONTROL);
        let last = menu.entries.len() - 1;
        match key.code {
            KeyCode::Up => {
                let to = if menu.cursor == 0 {
                    last
                } else {
                    menu.cursor - 1
                };
                self.set_slash_cursor(to);
            }
            KeyCode::Down => {
                let to = if menu.cursor == last {
                    0
                } else {
                    menu.cursor + 1
                };
                self.set_slash_cursor(to);
            }
            KeyCode::Tab => {
                let name = menu.entries[menu.cursor].name;
                self.draft = crate::input::draft::Draft::new(format!("/{name}"));
                self.view.slash.cursor = None;
                self.redraw();
            }
            KeyCode::Enter if plain => self.run_slash(menu.entries[menu.cursor]),
            KeyCode::Esc => {
                let typed = typed_name(&self.draft.text).map(str::to_owned);
                self.change(|app| &mut app.view.slash.dismissed, typed);
            }
            _ => return false,
        }
        true
    }

    /// Highlights row `at` of the open menu (keys, and the pointer over a row).
    pub(crate) fn set_slash_cursor(&mut self, at: usize) {
        let Some(typed) = typed_name(&self.draft.text).map(str::to_owned) else {
            return;
        };
        self.change(|app| &mut app.view.slash.cursor, Some((typed, at)));
    }

    /// Runs row `at` of the open menu (a click).
    pub(crate) fn run_slash_row(&mut self, at: usize) {
        if let Some(entry) = self.slash_menu().and_then(|m| m.entries.get(at).copied()) {
            self.run_slash(entry);
        }
    }

    /// Runs `entry`: your message box empties and the core runs the command.
    pub(crate) fn run_slash(&mut self, entry: &SlashEntry) {
        self.draft = crate::input::draft::Draft::default();
        self.view.slash = SlashUi::default();
        self.scroll_from_bottom = 0;
        self.redraw();
        self.command(Command::RunSlashCommand {
            slash: entry.command,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_is_exactly_clear_and_compact() {
        let names: Vec<&str> = COMMANDS.iter().map(|c| c.name).collect();
        assert_eq!(names, ["clear", "compact"]);
    }

    #[test]
    fn typing_narrows_the_list() {
        let names = |typed| -> Vec<&str> { matching(typed).iter().map(|c| c.name).collect() };
        assert_eq!(names(""), ["clear", "compact"]);
        assert_eq!(names("c"), ["clear", "compact"]);
        assert_eq!(names("cl"), ["clear"]);
        assert_eq!(names("COMP"), ["compact"]);
        // A name that only contains what you typed comes after the ones that start with it.
        assert_eq!(names("act"), ["compact"]);
        assert_eq!(names("users"), [] as [&str; 0]);
    }

    #[test]
    fn only_a_leading_slash_and_one_word_is_a_command_name() {
        assert_eq!(typed_name("/"), Some(""));
        assert_eq!(typed_name("/cle"), Some("cle"));
        assert_eq!(typed_name("/clear now"), None);
        assert_eq!(typed_name("/clear\n"), None);
        assert_eq!(typed_name("say /clear"), None);
        assert_eq!(typed_name(""), None);
    }

    #[test]
    fn an_exact_name_is_recognised() {
        assert_eq!(
            exact("/clear").map(|c| c.command),
            Some(SlashCommand::Clear)
        );
        assert_eq!(
            exact(" /Compact \n").map(|c| c.command),
            Some(SlashCommand::Compact)
        );
        assert_eq!(exact("/cl"), None);
        assert_eq!(exact("/Users/me/project"), None);
        assert_eq!(exact("clear"), None);
    }
}
