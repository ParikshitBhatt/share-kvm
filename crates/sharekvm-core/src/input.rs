//! Keyboard/mouse event types as they travel on the wire.
//!
//! These are copied verbatim from rdev 0.5.3 (same variants, same order), so
//! they serialize byte-for-byte like `rdev::EventType`. Desktop builds use
//! rdev's own types; builds without rdev (Android) use these. A test in
//! `protocol` checks the two encodings stay identical.
//!
//! Do not reorder or rename variants: bincode encodes enums by index.

#![allow(missing_docs)]

use serde::{Deserialize, Serialize};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Key {
    /// Alt key on Linux and Windows (option key on macOS)
    Alt,
    AltGr,
    Backspace,
    CapsLock,
    ControlLeft,
    ControlRight,
    Delete,
    DownArrow,
    End,
    Escape,
    F1,
    F10,
    F11,
    F12,
    F2,
    F3,
    F4,
    F5,
    F6,
    F7,
    F8,
    F9,
    Home,
    LeftArrow,
    /// also known as "windows", "super", and "command"
    MetaLeft,
    /// also known as "windows", "super", and "command"
    MetaRight,
    PageDown,
    PageUp,
    Return,
    RightArrow,
    ShiftLeft,
    ShiftRight,
    Space,
    Tab,
    UpArrow,
    PrintScreen,
    ScrollLock,
    Pause,
    NumLock,
    BackQuote,
    Num1,
    Num2,
    Num3,
    Num4,
    Num5,
    Num6,
    Num7,
    Num8,
    Num9,
    Num0,
    Minus,
    Equal,
    KeyQ,
    KeyW,
    KeyE,
    KeyR,
    KeyT,
    KeyY,
    KeyU,
    KeyI,
    KeyO,
    KeyP,
    LeftBracket,
    RightBracket,
    KeyA,
    KeyS,
    KeyD,
    KeyF,
    KeyG,
    KeyH,
    KeyJ,
    KeyK,
    KeyL,
    SemiColon,
    Quote,
    BackSlash,
    IntlBackslash,
    KeyZ,
    KeyX,
    KeyC,
    KeyV,
    KeyB,
    KeyN,
    KeyM,
    Comma,
    Dot,
    Slash,
    Insert,
    KpReturn,
    KpMinus,
    KpPlus,
    KpMultiply,
    KpDivide,
    Kp0,
    Kp1,
    Kp2,
    Kp3,
    Kp4,
    Kp5,
    Kp6,
    Kp7,
    Kp8,
    Kp9,
    KpDelete,
    Function,
    Unknown(u32),
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Button {
    Left,
    Right,
    Middle,
    Unknown(u8),
}

#[derive(Debug, Copy, Clone, PartialEq, Serialize, Deserialize)]
pub enum EventType {
    /// The keys correspond to a standard qwerty layout, they don't correspond
    /// To the actual letter a user would use, that requires some layout logic to be added.
    KeyPress(Key),
    KeyRelease(Key),
    /// Mouse Button
    ButtonPress(Button),
    ButtonRelease(Button),
    /// Values in pixels. `EventType::MouseMove{x: 0, y: 0}` corresponds to the
    /// top left corner, with x increasing downward and y increasing rightward
    MouseMove {
        x: f64,
        y: f64,
    },
    /// `delta_y` represents vertical scroll and `delta_x` represents horizontal scroll.
    /// Positive values correspond to scrolling up or right and negative values
    /// correspond to scrolling down or left
    Wheel {
        delta_x: i64,
        delta_y: i64,
    },
}
