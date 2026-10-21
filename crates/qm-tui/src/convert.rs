//! Turning crossterm's events into [`InputEvent`]s for the wire, and back.
//!
//! The wire has its own input type (`qm_types::wire`), so a crossterm upgrade never changes it.
//! Only the events the UI reads are carried; anything else (media keys, a bare modifier press) is
//! dropped on the way out.

use crossterm::event::{
    Event, KeyCode as CKey, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
    MouseButton as CBtn, MouseEvent, MouseEventKind,
};
use qm_types::wire::{InputEvent, KeyCode, KeyKind, Modifiers, MouseButton, MouseKind};

/// `event` for the wire, or `None` for an event the UI never reads. A resize is not an input
/// event; it has its own frame.
pub(crate) fn to_wire(event: &Event) -> Option<InputEvent> {
    Some(match event {
        Event::Key(key) => InputEvent::Key {
            code: key_to_wire(key.code)?,
            modifiers: mods_to_wire(key.modifiers),
            kind: match key.kind {
                KeyEventKind::Press => KeyKind::Press,
                KeyEventKind::Repeat => KeyKind::Repeat,
                KeyEventKind::Release => KeyKind::Release,
            },
        },
        Event::Mouse(mouse) => InputEvent::Mouse {
            kind: match mouse.kind {
                MouseEventKind::Down(b) => MouseKind::Down(button_to_wire(b)),
                MouseEventKind::Up(b) => MouseKind::Up(button_to_wire(b)),
                MouseEventKind::Drag(b) => MouseKind::Drag(button_to_wire(b)),
                MouseEventKind::Moved => MouseKind::Moved,
                MouseEventKind::ScrollDown => MouseKind::ScrollDown,
                MouseEventKind::ScrollUp => MouseKind::ScrollUp,
                MouseEventKind::ScrollLeft => MouseKind::ScrollLeft,
                MouseEventKind::ScrollRight => MouseKind::ScrollRight,
            },
            column: mouse.column,
            row: mouse.row,
            modifiers: mods_to_wire(mouse.modifiers),
        },
        Event::Paste(text) => InputEvent::Paste(text.clone()),
        Event::FocusGained => InputEvent::FocusGained,
        Event::FocusLost => InputEvent::FocusLost,
        Event::Resize(..) => return None,
    })
}

/// The crossterm event the UI's input code reads for `event`.
pub(crate) fn from_wire(event: InputEvent) -> Event {
    match event {
        InputEvent::Key {
            code,
            modifiers,
            kind,
        } => Event::Key(KeyEvent {
            code: key_from_wire(code),
            modifiers: mods_from_wire(modifiers),
            kind: match kind {
                KeyKind::Press => KeyEventKind::Press,
                KeyKind::Repeat => KeyEventKind::Repeat,
                KeyKind::Release => KeyEventKind::Release,
            },
            state: KeyEventState::NONE,
        }),
        InputEvent::Mouse {
            kind,
            column,
            row,
            modifiers,
        } => Event::Mouse(MouseEvent {
            kind: match kind {
                MouseKind::Down(b) => MouseEventKind::Down(button_from_wire(b)),
                MouseKind::Up(b) => MouseEventKind::Up(button_from_wire(b)),
                MouseKind::Drag(b) => MouseEventKind::Drag(button_from_wire(b)),
                MouseKind::Moved => MouseEventKind::Moved,
                MouseKind::ScrollDown => MouseEventKind::ScrollDown,
                MouseKind::ScrollUp => MouseEventKind::ScrollUp,
                MouseKind::ScrollLeft => MouseEventKind::ScrollLeft,
                MouseKind::ScrollRight => MouseEventKind::ScrollRight,
            },
            column,
            row,
            modifiers: mods_from_wire(modifiers),
        }),
        InputEvent::Paste(text) => Event::Paste(text),
        InputEvent::FocusGained => Event::FocusGained,
        InputEvent::FocusLost => Event::FocusLost,
    }
}

fn key_to_wire(code: CKey) -> Option<KeyCode> {
    Some(match code {
        CKey::Char(c) => KeyCode::Char(c),
        CKey::F(n) => KeyCode::F(n),
        CKey::Enter => KeyCode::Enter,
        CKey::Backspace => KeyCode::Backspace,
        CKey::Tab => KeyCode::Tab,
        CKey::BackTab => KeyCode::BackTab,
        CKey::Esc => KeyCode::Esc,
        CKey::Left => KeyCode::Left,
        CKey::Right => KeyCode::Right,
        CKey::Up => KeyCode::Up,
        CKey::Down => KeyCode::Down,
        CKey::Home => KeyCode::Home,
        CKey::End => KeyCode::End,
        CKey::PageUp => KeyCode::PageUp,
        CKey::PageDown => KeyCode::PageDown,
        CKey::Delete => KeyCode::Delete,
        CKey::Insert => KeyCode::Insert,
        _ => return None,
    })
}

fn key_from_wire(code: KeyCode) -> CKey {
    match code {
        KeyCode::Char(c) => CKey::Char(c),
        KeyCode::F(n) => CKey::F(n),
        KeyCode::Enter => CKey::Enter,
        KeyCode::Backspace => CKey::Backspace,
        KeyCode::Tab => CKey::Tab,
        KeyCode::BackTab => CKey::BackTab,
        KeyCode::Esc => CKey::Esc,
        KeyCode::Left => CKey::Left,
        KeyCode::Right => CKey::Right,
        KeyCode::Up => CKey::Up,
        KeyCode::Down => CKey::Down,
        KeyCode::Home => CKey::Home,
        KeyCode::End => CKey::End,
        KeyCode::PageUp => CKey::PageUp,
        KeyCode::PageDown => CKey::PageDown,
        KeyCode::Delete => CKey::Delete,
        KeyCode::Insert => CKey::Insert,
    }
}

fn button_to_wire(button: CBtn) -> MouseButton {
    match button {
        CBtn::Left => MouseButton::Left,
        CBtn::Right => MouseButton::Right,
        CBtn::Middle => MouseButton::Middle,
    }
}

fn button_from_wire(button: MouseButton) -> CBtn {
    match button {
        MouseButton::Left => CBtn::Left,
        MouseButton::Right => CBtn::Right,
        MouseButton::Middle => CBtn::Middle,
    }
}

fn mods_to_wire(mods: KeyModifiers) -> Modifiers {
    let mut bits = 0;
    for (flag, bit) in [
        (KeyModifiers::SHIFT, Modifiers::SHIFT),
        (KeyModifiers::CONTROL, Modifiers::CONTROL),
        (KeyModifiers::ALT, Modifiers::ALT),
        (KeyModifiers::SUPER, Modifiers::SUPER),
    ] {
        if mods.contains(flag) {
            bits |= bit;
        }
    }
    Modifiers(bits)
}

fn mods_from_wire(mods: Modifiers) -> KeyModifiers {
    let mut out = KeyModifiers::NONE;
    for (flag, bit) in [
        (KeyModifiers::SHIFT, Modifiers::SHIFT),
        (KeyModifiers::CONTROL, Modifiers::CONTROL),
        (KeyModifiers::ALT, Modifiers::ALT),
        (KeyModifiers::SUPER, Modifiers::SUPER),
    ] {
        if mods.contains(bit) {
            out |= flag;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_survive_the_wire() {
        let events = [
            Event::Key(KeyEvent::new(
                CKey::Char('é'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )),
            Event::Key(KeyEvent::new(CKey::BackTab, KeyModifiers::NONE)),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Drag(CBtn::Left),
                column: 9,
                row: 4,
                modifiers: KeyModifiers::ALT,
            }),
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 1,
                row: 2,
                modifiers: KeyModifiers::NONE,
            }),
            Event::Paste(String::from("a\nb")),
            Event::FocusGained,
            Event::FocusLost,
        ];
        for event in events {
            let wire = to_wire(&event).expect("carried");
            assert_eq!(from_wire(wire), event);
        }
    }

    #[test]
    fn unread_events_are_dropped() {
        assert!(to_wire(&Event::Resize(1, 2)).is_none());
        assert!(
            to_wire(&Event::Key(KeyEvent::new(
                CKey::CapsLock,
                KeyModifiers::NONE
            )))
            .is_none()
        );
    }
}
