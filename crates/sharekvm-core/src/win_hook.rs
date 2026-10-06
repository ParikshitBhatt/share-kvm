//! Windows input hook (low-level mouse and keyboard hooks), used instead of
//! `rdev::grab` because rdev hides Windows' "injected" flag. That flag is how
//! a computer being driven by ShareKVM tells its own mouse and keyboard
//! (take control back) from the input ShareKVM replays (ignore).
//!
//! Event decoding follows rdev 0.5.3's Windows backend.

use crate::hook::Hooked;
use crate::win_keys::key_from_code;
use rdev::{Button, EventType};
use std::cell::RefCell;
use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, SetWindowsHookExW, HC_ACTION, KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSG,
    MSLLHOOKSTRUCT, WHEEL_DELTA, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

type Callback = Box<dyn FnMut(Hooked) -> bool>;

thread_local! {
    // Low-level hooks are called on the thread that installed them.
    static CALLBACK: RefCell<Option<Callback>> = RefCell::new(None);
}

/// Runs the callback; true = let the event through.
fn dispatch(h: Hooked) -> bool {
    CALLBACK.with(|c| match c.try_borrow_mut() {
        Ok(mut cb) => cb.as_mut().map_or(true, |f| f(h)),
        Err(_) => true, // re-entered while busy: never block input
    })
}

fn high_word(v: u32) -> i16 {
    (v >> 16) as u16 as i16
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(lparam as *const MSLLHOOKSTRUCT);
        let ev = match wparam as u32 {
            WM_MOUSEMOVE => Some(EventType::MouseMove { x: info.pt.x as f64, y: info.pt.y as f64 }),
            WM_LBUTTONDOWN => Some(EventType::ButtonPress(Button::Left)),
            WM_LBUTTONUP => Some(EventType::ButtonRelease(Button::Left)),
            WM_RBUTTONDOWN => Some(EventType::ButtonPress(Button::Right)),
            WM_RBUTTONUP => Some(EventType::ButtonRelease(Button::Right)),
            WM_MBUTTONDOWN => Some(EventType::ButtonPress(Button::Middle)),
            WM_MBUTTONUP => Some(EventType::ButtonRelease(Button::Middle)),
            WM_XBUTTONDOWN => Some(EventType::ButtonPress(Button::Unknown(high_word(info.mouseData) as u8))),
            WM_XBUTTONUP => Some(EventType::ButtonRelease(Button::Unknown(high_word(info.mouseData) as u8))),
            WM_MOUSEWHEEL => Some(EventType::Wheel {
                delta_x: 0,
                delta_y: (high_word(info.mouseData) / WHEEL_DELTA as i16) as i64,
            }),
            WM_MOUSEHWHEEL => Some(EventType::Wheel {
                delta_x: (high_word(info.mouseData) / WHEEL_DELTA as i16) as i64,
                delta_y: 0,
            }),
            _ => None,
        };
        if let Some(ev) = ev {
            if !dispatch(Hooked { ev, delta: None, injected: info.flags & LLMHF_INJECTED != 0 }) {
                return 1; // swallowed
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let info = &*(lparam as *const KBDLLHOOKSTRUCT);
        let key = key_from_code(info.vkCode);
        let ev = match wparam as u32 {
            WM_KEYDOWN | WM_SYSKEYDOWN => Some(EventType::KeyPress(key)),
            WM_KEYUP | WM_SYSKEYUP => Some(EventType::KeyRelease(key)),
            _ => None,
        };
        if let Some(ev) = ev {
            if !dispatch(Hooked { ev, delta: None, injected: info.flags & LLKHF_INJECTED != 0 }) {
                return 1;
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

/// Installs both hooks on this thread and pumps its messages forever.
pub fn run(callback: impl FnMut(Hooked) -> bool + 'static) -> Result<(), String> {
    CALLBACK.with(|c| *c.borrow_mut() = Some(Box::new(callback)));
    unsafe {
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), std::ptr::null_mut(), 0);
        let keyboard = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), std::ptr::null_mut(), 0);
        if mouse.is_null() || keyboard.is_null() {
            return Err("could not install the input hook".into());
        }
        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {}
    }
    Ok(())
}
