//! Input: what keys and the mouse do.
//!
//! Input reads geometry only from the last frame's [`FrameMap`](crate::hit::FrameMap), and is the
//! only writer of [`View`](crate::view::View). The actions here are shared by the keys, the mouse
//! and the context menu, so a click and its keyboard twin always do the same thing.

pub(crate) mod clipboard;
pub(crate) mod draft;
pub(crate) mod editor;
pub(crate) mod field;
pub(crate) mod focus;
pub(crate) mod keys;
pub(crate) mod menu;
pub(crate) mod mouse;
pub(crate) mod prefs;
pub(crate) mod select;

use crate::app::App;
use crate::hit::Pane;
use qm_types::api::{
    AgentRef, Card, CardAction, CardKind, Command, HeldAnswer, HeldChoice, NoticeAnswer,
};
use qm_types::ids::MessageId;
use std::time::Instant;

/// Shown for anything whose screen or command this build doesn't have.
pub(crate) const NOT_BUILT: &str = "Not built yet.";

impl App {
    /// The messages whose card still has buttons to press, oldest first.
    pub(crate) fn live_cards(&self) -> Vec<MessageId> {
        self.snapshot
            .conversation
            .iter()
            .filter(|line| {
                line.card
                    .as_ref()
                    .is_some_and(|card| card.answered.is_none() && !card.buttons.is_empty())
            })
            .map(|line| line.id)
            .collect()
    }

    /// The card the card keys act on: the one you made current while it still has buttons,
    /// otherwise the newest that has.
    pub(crate) fn current_card(&self) -> Option<MessageId> {
        let live = self.live_cards();
        self.view
            .card
            .filter(|id| live.contains(id))
            .or_else(|| live.last().copied())
    }

    fn card_of(&self, message: MessageId) -> Option<&Card> {
        self.snapshot
            .conversation
            .iter()
            .find(|line| line.id == message)
            .and_then(|line| line.card.as_ref())
    }

    /// Makes the previous (`forward == false`) or next live card current.
    pub(crate) fn step_card(&mut self, forward: bool) {
        let live = self.live_cards();
        let Some(current) = self.current_card() else {
            return;
        };
        let at = live.iter().position(|&id| id == current).unwrap_or(0);
        let next = if forward {
            (at + 1).min(live.len() - 1)
        } else {
            at.saturating_sub(1)
        };
        self.change(|app| &mut app.view.card, Some(live[next]));
        self.reveal = Some(live[next]);
    }

    /// Presses the current card's button whose key is `key`; returns whether one matched.
    pub(crate) fn press_card_key(&mut self, key: char, now: Instant) -> bool {
        let Some(message) = self.current_card() else {
            return false;
        };
        let index = self.card_of(message).and_then(|card| {
            card.buttons
                .iter()
                .position(|button| button.key.eq_ignore_ascii_case(&key))
        });
        match index {
            Some(index) => {
                self.press_card_button(message, index, now);
                true
            }
            None => false,
        }
    }

    /// Presses button `index` of the card under `message`, and makes that card current.
    pub(crate) fn press_card_button(&mut self, message: MessageId, index: usize, now: Instant) {
        let Some(card) = self.card_of(message) else {
            return;
        };
        if card.answered.is_some() {
            return;
        }
        let held = card.held;
        let Some(action) = card.buttons.get(index).map(|button| button.action.clone()) else {
            return;
        };
        self.change(|app| &mut app.view.card, Some(message));
        match action {
            CardAction::Answer {
                answer:
                    HeldAnswer::Choose {
                        choice: HeldChoice::RetryWithGuidance,
                        ..
                    },
            } => {
                if let Some(held) = held {
                    self.change(|app| &mut app.view.guidance_for, Some(held));
                    self.focus_pane(Pane::Input);
                }
            }
            CardAction::Answer { answer } => {
                if let Some(held) = held {
                    self.command(Command::AnswerHeld { held, answer });
                }
            }
            CardAction::AskAgain => self.command(Command::AnswerNotice {
                message,
                answer: NoticeAnswer::AskAgain,
            }),
            CardAction::LeaveIt => self.command(Command::AnswerNotice {
                message,
                answer: NoticeAnswer::LeaveIt,
            }),
            CardAction::Reply { text } => self.command(Command::Send {
                to: AgentRef::Quartermaster,
                text,
            }),
            CardAction::ChangeLocation { location } => {
                if let Some(held) = held {
                    self.change(|app| &mut app.view.guidance_for, None);
                    self.change(|app| &mut app.view.location_for, Some(held));
                    self.set_input(location);
                    self.focus_pane(Pane::Input);
                }
            }
            CardAction::ShowDiff => self.show_diff(held, now),
            // Any action this build doesn't know.
            _ => self.flash(NOT_BUILT, now),
        }
    }

    /// The status bar's approvals or decisions count was clicked: focus the conversation and make
    /// the newest card of that kind current, scrolled into view.
    pub(crate) fn jump_to_card(&mut self, kind: CardKind) {
        // From a session or the Diff screen, the card is on the Bridge.
        self.show(crate::router::Screen::Bridge);
        self.focus_pane(Pane::Conversation);
        let newest = self
            .snapshot
            .conversation
            .iter()
            .rev()
            .find(|line| {
                line.card.as_ref().is_some_and(|card| {
                    card.kind == kind && card.answered.is_none() && !card.buttons.is_empty()
                })
            })
            .map(|line| line.id);
        if let Some(message) = newest {
            self.change(|app| &mut app.view.card, Some(message));
            self.change(|app| &mut app.scroll_from_bottom, 0);
            self.reveal = Some(message);
        }
    }

    /// Opens a crew row (`⏎`, `o`, a double-click, "Show task"): selects it; with `session` it
    /// opens the worker's session (the Quartermaster's row focuses the Bridge input, its
    /// conversation being the Bridge), otherwise it opens the task overlay when folded and the
    /// row has a task.
    pub(crate) fn open_crew(&mut self, agent: AgentRef, session: bool, now: Instant) {
        let _ = now;
        self.select_crew(agent);
        if session {
            self.open_worker_session(agent);
            return;
        }
        if self.has_task(agent) && self.drawn.folded {
            self.change(|app| &mut app.view.task_overlay, true);
        }
    }

    /// Opens `agent`'s session screen: a crewmate or a secondmate; the Quartermaster's row focuses
    /// the Bridge input.
    pub(crate) fn open_worker_session(&mut self, agent: AgentRef) {
        match agent {
            AgentRef::Crewmate { .. } | AgentRef::Secondmate { .. } => self.enter_session(agent),
            AgentRef::Quartermaster => {
                self.show(crate::router::Screen::Bridge);
                self.focus_pane(Pane::Input);
            }
        }
    }

    /// Selects a crew row; the task pane follows it.
    pub(crate) fn select_crew(&mut self, agent: AgentRef) {
        if self.view.selected_crew != Some(agent) {
            self.view.task_scroll = 0;
        }
        self.change(|app| &mut app.view.selected_crew, Some(agent));
    }

    /// Whether `agent` is a crewmate whose task the task pane can show.
    pub(crate) fn has_task(&self, agent: AgentRef) -> bool {
        match agent {
            AgentRef::Crewmate { task } => self.snapshot.tasks.iter().any(|t| t.task == task),
            AgentRef::Quartermaster | AgentRef::Secondmate { .. } => false,
        }
    }

    /// The task pane's Stop (button or `s`): interrupts the selected crewmate while its task
    /// runs.
    pub(crate) fn stop_selected(&mut self) {
        let Some(agent @ AgentRef::Crewmate { task }) = self.view.selected_crew else {
            return;
        };
        if self
            .snapshot
            .tasks
            .iter()
            .any(|detail| detail.task == task && detail.running)
        {
            self.command(Command::Interrupt { agent });
        }
    }
}
