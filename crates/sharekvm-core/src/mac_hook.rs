//! macOS input hook, used instead of `rdev::grab` on macOS.
//!
//! rdev reports mouse movement only as an absolute position, and only for
//! plain moves. That breaks remote control on a Mac: the hidden cursor keeps
//! drifting (and re-centring it lags behind), so position-minus-centre gives
//! movement that is too fast and points the wrong way; and moves with a
//! button held (drags) were never captured. This hook reads each event's own
//! movement deltas, sees drags, freezes the cursor while the other computer is
//! active (as Barrier/Synergy do), and re-enables itself if macOS disables it.

use crate::mac_keys::key_from_code;
use core_foundation::base::TCFType;
use core_foundation::runloop::{kCFRunLoopCommonModes, CFRunLoop};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventTap, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventType, EventField,
};
use core_graphics::geometry::CGPoint;
use rdev::{Button, EventType, Key};
use std::cell::RefCell;
use std::os::raw::c_void;
use std::sync::atomic::{AtomicPtr, Ordering};

/// What the hook reports to the server.
#[derive(Debug, Clone, Copy)]
pub enum Hooked {
    /// Pointer moved: position after the move, and the move itself (points).
    Move { x: f64, y: f64, dx: f64, dy: f64 },
    Input(EventType),
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn CGEventTapEnable(tap: *mut c_void, enable: bool);
    fn CGAssociateMouseAndMouseCursorPosition(connected: u32) -> i32;
}

static TAP: AtomicPtr<c_void> = AtomicPtr::new(std::ptr::null_mut());

/// Freeze (true) or release (false) the on-screen cursor. While frozen, mouse
/// events still arrive with their deltas; the cursor just doesn't move.
pub fn freeze_cursor(frozen: bool) {
    unsafe {
        CGAssociateMouseAndMouseCursorPosition(if frozen { 0 } else { 1 });
    }
}

/// Moves the cursor without generating an event.
pub fn warp(x: f64, y: f64) {
    let _ = CGDisplay::warp_mouse_cursor_position(CGPoint::new(x, y));
    // A warp briefly suppresses local mouse input unless re-associated.
    freeze_cursor(false);
}

// Device-specific modifier bits (NX_DEVICE*KEYMASK), so left and right keys are told apart.
fn modifier_mask(code: u16) -> Option<u64> {
    Some(match code {
        56 => 0x0000_0002, // left shift
        60 => 0x0000_0004, // right shift
        59 => 0x0000_0001, // left control
        62 => 0x0000_2000, // right control
        58 => 0x0000_0020, // left option
        61 => 0x0000_0040, // right option
        55 => 0x0000_0008, // left command
        54 => 0x0000_0010, // right command
        63 => 0x0080_0000, // fn
        _ => return None,
    })
}

fn convert(etype: CGEventType, ev: &CGEvent) -> Vec<Hooked> {
    let key = || key_from_code(ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16);
    let input = |e| vec![Hooked::Input(e)];
    match etype {
        CGEventType::MouseMoved
        | CGEventType::LeftMouseDragged
        | CGEventType::RightMouseDragged
        | CGEventType::OtherMouseDragged => {
            let p = ev.location();
            vec![Hooked::Move {
                x: p.x,
                y: p.y,
                dx: ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_X),
                dy: ev.get_double_value_field(EventField::MOUSE_EVENT_DELTA_Y),
            }]
        }
        CGEventType::LeftMouseDown => input(EventType::ButtonPress(Button::Left)),
        CGEventType::LeftMouseUp => input(EventType::ButtonRelease(Button::Left)),
        CGEventType::RightMouseDown => input(EventType::ButtonPress(Button::Right)),
        CGEventType::RightMouseUp => input(EventType::ButtonRelease(Button::Right)),
        CGEventType::OtherMouseDown => input(EventType::ButtonPress(Button::Middle)),
        CGEventType::OtherMouseUp => input(EventType::ButtonRelease(Button::Middle)),
        CGEventType::KeyDown => input(EventType::KeyPress(key())),
        CGEventType::KeyUp => input(EventType::KeyRelease(key())),
        CGEventType::FlagsChanged => {
            let code = ev.get_integer_value_field(EventField::KEYBOARD_EVENT_KEYCODE) as u16;
            let flags = ev.get_flags().bits();
            match (key_from_code(code), modifier_mask(code)) {
                // Caps Lock reports each toggle once: send a full key press.
                (Key::CapsLock, _) => vec![
                    Hooked::Input(EventType::KeyPress(Key::CapsLock)),
                    Hooked::Input(EventType::KeyRelease(Key::CapsLock)),
                ],
                (k, Some(mask)) if flags & mask != 0 => input(EventType::KeyPress(k)),
                (k, Some(_)) => input(EventType::KeyRelease(k)),
                _ => vec![],
            }
        }
        CGEventType::ScrollWheel => input(EventType::Wheel {
            delta_x: ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_2),
            delta_y: ev.get_integer_value_field(EventField::SCROLL_WHEEL_EVENT_POINT_DELTA_AXIS_1),
        }),
        _ => vec![],
    }
}

/// Installs the hook on the current (main) thread and runs its loop forever.
/// `callback` returns true to let an event through to this Mac, false to swallow it.
pub fn run(callback: impl FnMut(Hooked) -> bool + 'static) -> Result<(), String> {
    let callback = RefCell::new(callback);
    let tap = CGEventTap::new(
        CGEventTapLocation::HID,
        CGEventTapPlacement::HeadInsertEventTap,
        CGEventTapOptions::Default,
        vec![
            CGEventType::MouseMoved,
            CGEventType::LeftMouseDragged,
            CGEventType::RightMouseDragged,
            CGEventType::OtherMouseDragged,
            CGEventType::LeftMouseDown,
            CGEventType::LeftMouseUp,
            CGEventType::RightMouseDown,
            CGEventType::RightMouseUp,
            CGEventType::OtherMouseDown,
            CGEventType::OtherMouseUp,
            CGEventType::KeyDown,
            CGEventType::KeyUp,
            CGEventType::FlagsChanged,
            CGEventType::ScrollWheel,
        ],
        |_proxy, etype, ev| {
            if matches!(etype, CGEventType::TapDisabledByTimeout | CGEventType::TapDisabledByUserInput) {
                // macOS turns slow taps off; turn ours straight back on.
                let tap = TAP.load(Ordering::SeqCst);
                if !tap.is_null() {
                    unsafe { CGEventTapEnable(tap, true) };
                }
                return None;
            }
            let mut pass = true;
            for h in convert(etype, ev) {
                pass &= (callback.borrow_mut())(h);
            }
            if !pass {
                ev.set_type(CGEventType::Null); // swallowed: never reaches this Mac
            }
            None
        },
    )
    .map_err(|_| "could not create the input hook".to_string())?;

    TAP.store(tap.mach_port.as_concrete_TypeRef() as *mut c_void, Ordering::SeqCst);
    let source = tap.mach_port.create_runloop_source(0).map_err(|_| "could not attach the input hook".to_string())?;
    unsafe {
        CFRunLoop::get_current().add_source(&source, kCFRunLoopCommonModes);
    }
    tap.enable();
    CFRunLoop::run_current();
    Ok(())
}
