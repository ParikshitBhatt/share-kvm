//! Turns desktop key events into things Android can do without root:
//! typing text into the focused field, moving focus (TV-remote style),
//! and global actions (Back, Home).
//!
//! Characters follow the US layout, matching the physical key positions the
//! desktop reports.

use sharekvm_core::protocol::{EventType, Key};

#[derive(Debug, PartialEq, Clone)]
pub enum Action {
    /// Type this text into the focused field.
    Text(String),
    /// A navigation/editing key: "up", "down", "left", "right", "enter",
    /// "back", "home", "backspace", "delete", "tab", "page_up", "page_down".
    Key(&'static str),
    /// Ctrl/Cmd+V.
    Paste,
}

#[derive(Default)]
pub struct Keys {
    shift: bool,
    caps: bool,
    ctrl: bool,
    alt: bool,
    meta: bool,
    /// Meta (Cmd/Win) went down and nothing else was pressed since: a tap means Home.
    meta_alone: bool,
}

impl Keys {
    pub fn handle(&mut self, ev: EventType) -> Option<Action> {
        match ev {
            EventType::KeyPress(k) => self.press(k),
            EventType::KeyRelease(k) => self.release(k),
            _ => None,
        }
    }

    /// Forget held modifiers (control moved back to the computer).
    pub fn reset(&mut self) {
        let caps = self.caps;
        *self = Keys { caps, ..Default::default() };
    }

    fn press(&mut self, k: Key) -> Option<Action> {
        match k {
            Key::ShiftLeft | Key::ShiftRight => self.shift = true,
            Key::ControlLeft | Key::ControlRight => self.ctrl = true,
            Key::Alt | Key::AltGr => self.alt = true,
            Key::MetaLeft | Key::MetaRight => {
                self.meta = true;
                self.meta_alone = true;
            }
            Key::CapsLock => self.caps = !self.caps,
            _ => {
                self.meta_alone = false;
                return self.key(k);
            }
        }
        None
    }

    fn release(&mut self, k: Key) -> Option<Action> {
        match k {
            Key::ShiftLeft | Key::ShiftRight => self.shift = false,
            Key::ControlLeft | Key::ControlRight => self.ctrl = false,
            Key::Alt | Key::AltGr => self.alt = false,
            Key::MetaLeft | Key::MetaRight => {
                self.meta = false;
                if std::mem::take(&mut self.meta_alone) {
                    return Some(Action::Key("home"));
                }
            }
            _ => {}
        }
        None
    }

    fn key(&self, k: Key) -> Option<Action> {
        if self.ctrl || self.meta {
            return (k == Key::KeyV).then_some(Action::Paste);
        }
        let named = match k {
            Key::UpArrow => "up",
            Key::DownArrow => "down",
            Key::LeftArrow => "left",
            Key::RightArrow => "right",
            Key::Return | Key::KpReturn => "enter",
            Key::Escape => "back",
            Key::Backspace => "backspace",
            Key::Delete | Key::KpDelete => "delete",
            Key::Tab => "tab",
            Key::PageUp => "page_up",
            Key::PageDown => "page_down",
            _ => {
                if self.alt {
                    return None; // Alt+letter shortcuts mean nothing here
                }
                return char_for(k, self.shift, self.caps).map(|c| Action::Text(c.to_string()));
            }
        };
        Some(Action::Key(named))
    }
}

/// US-layout character for a key, or None for keys that don't type.
fn char_for(k: Key, shift: bool, caps: bool) -> Option<char> {
    let letter = |c: char| Some(if shift ^ caps { c.to_ascii_uppercase() } else { c });
    let pair = |plain: char, shifted: char| Some(if shift { shifted } else { plain });
    match k {
        Key::KeyA => letter('a'),
        Key::KeyB => letter('b'),
        Key::KeyC => letter('c'),
        Key::KeyD => letter('d'),
        Key::KeyE => letter('e'),
        Key::KeyF => letter('f'),
        Key::KeyG => letter('g'),
        Key::KeyH => letter('h'),
        Key::KeyI => letter('i'),
        Key::KeyJ => letter('j'),
        Key::KeyK => letter('k'),
        Key::KeyL => letter('l'),
        Key::KeyM => letter('m'),
        Key::KeyN => letter('n'),
        Key::KeyO => letter('o'),
        Key::KeyP => letter('p'),
        Key::KeyQ => letter('q'),
        Key::KeyR => letter('r'),
        Key::KeyS => letter('s'),
        Key::KeyT => letter('t'),
        Key::KeyU => letter('u'),
        Key::KeyV => letter('v'),
        Key::KeyW => letter('w'),
        Key::KeyX => letter('x'),
        Key::KeyY => letter('y'),
        Key::KeyZ => letter('z'),
        Key::Num1 => pair('1', '!'),
        Key::Num2 => pair('2', '@'),
        Key::Num3 => pair('3', '#'),
        Key::Num4 => pair('4', '$'),
        Key::Num5 => pair('5', '%'),
        Key::Num6 => pair('6', '^'),
        Key::Num7 => pair('7', '&'),
        Key::Num8 => pair('8', '*'),
        Key::Num9 => pair('9', '('),
        Key::Num0 => pair('0', ')'),
        Key::Minus => pair('-', '_'),
        Key::Equal => pair('=', '+'),
        Key::LeftBracket => pair('[', '{'),
        Key::RightBracket => pair(']', '}'),
        Key::SemiColon => pair(';', ':'),
        Key::Quote => pair('\'', '"'),
        Key::BackQuote => pair('`', '~'),
        Key::BackSlash | Key::IntlBackslash => pair('\\', '|'),
        Key::Comma => pair(',', '<'),
        Key::Dot => pair('.', '>'),
        Key::Slash => pair('/', '?'),
        Key::Space => Some(' '),
        Key::Kp0 => Some('0'),
        Key::Kp1 => Some('1'),
        Key::Kp2 => Some('2'),
        Key::Kp3 => Some('3'),
        Key::Kp4 => Some('4'),
        Key::Kp5 => Some('5'),
        Key::Kp6 => Some('6'),
        Key::Kp7 => Some('7'),
        Key::Kp8 => Some('8'),
        Key::Kp9 => Some('9'),
        Key::KpMinus => Some('-'),
        Key::KpPlus => Some('+'),
        Key::KpMultiply => Some('*'),
        Key::KpDivide => Some('/'),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use EventType::{KeyPress as P, KeyRelease as R};

    fn typed(keys: &mut Keys, events: &[EventType]) -> Vec<Action> {
        events.iter().filter_map(|e| keys.handle(*e)).collect()
    }

    #[test]
    fn letters_shift_and_caps() {
        let mut k = Keys::default();
        let out = typed(&mut k, &[P(Key::KeyH), R(Key::KeyH), P(Key::ShiftLeft), P(Key::KeyI), P(Key::Num1), R(Key::ShiftLeft)]);
        assert_eq!(out, vec![Action::Text("h".into()), Action::Text("I".into()), Action::Text("!".into())]);
        let out = typed(&mut k, &[P(Key::CapsLock), P(Key::KeyA), P(Key::ShiftLeft), P(Key::KeyA), P(Key::Num2)]);
        assert_eq!(out, vec![Action::Text("A".into()), Action::Text("a".into()), Action::Text("@".into())]);
    }

    #[test]
    fn navigation_and_editing() {
        let mut k = Keys::default();
        let out = typed(&mut k, &[P(Key::DownArrow), P(Key::Return), P(Key::Escape), P(Key::Backspace)]);
        assert_eq!(out, vec![Action::Key("down"), Action::Key("enter"), Action::Key("back"), Action::Key("backspace")]);
    }

    #[test]
    fn shortcuts() {
        let mut k = Keys::default();
        // Cmd+V / Ctrl+V paste; other shortcuts do nothing.
        assert_eq!(typed(&mut k, &[P(Key::MetaLeft), P(Key::KeyV), R(Key::KeyV), R(Key::MetaLeft)]), vec![Action::Paste]);
        assert_eq!(typed(&mut k, &[P(Key::ControlLeft), P(Key::KeyC), R(Key::ControlLeft)]), vec![]);
        // Tapping Cmd/Win alone goes Home.
        assert_eq!(typed(&mut k, &[P(Key::MetaLeft), R(Key::MetaLeft)]), vec![Action::Key("home")]);
    }

    #[test]
    fn reset_drops_held_modifiers_but_keeps_caps_lock() {
        let mut k = Keys::default();
        typed(&mut k, &[P(Key::CapsLock), P(Key::ShiftLeft)]);
        k.reset();
        assert_eq!(typed(&mut k, &[P(Key::KeyQ)]), vec![Action::Text("Q".into())]);
    }
}
