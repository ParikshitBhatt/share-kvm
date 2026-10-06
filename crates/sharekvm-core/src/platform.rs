//! OS-specific bits: DPI setup, screen size, cursor warping and input injection.

use rdev::{Button, EventType, Key};
use std::path::PathBuf;

/// Call once at startup, before reading screen sizes or installing hooks.
pub fn init() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        // Use physical pixels so hook coordinates, screen size and SendInput all agree.
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

/// Size of the main display in cursor coordinates (pixels on Windows, points on macOS).
pub fn screen_size() -> (f64, f64) {
    match rdev::display_size() {
        Ok((w, h)) => (w as f64, h as f64),
        Err(e) => {
            log::warn!("could not read display size ({e:?}), assuming 1920x1080");
            (1920.0, 1080.0)
        }
    }
}

/// Every active monitor's bounds in global cursor coordinates, plus the primary's index.
pub fn monitors() -> Option<(Vec<crate::screens::Rect>, usize)> {
    #[cfg(target_os = "macos")]
    {
        use core_graphics::display::CGDisplay;
        let ids = CGDisplay::active_displays().ok()?;
        let main = CGDisplay::main().id;
        let rects = ids
            .iter()
            .map(|&id| {
                let b = CGDisplay::new(id).bounds();
                crate::screens::Rect::new(b.origin.x, b.origin.y, b.size.width, b.size.height)
            })
            .collect::<Vec<_>>();
        let primary = ids.iter().position(|&id| id == main).unwrap_or(0);
        return (!rects.is_empty()).then_some((rects, primary));
    }
    #[cfg(windows)]
    {
        return win::monitors();
    }
    #[allow(unreachable_code)]
    None
}

/// Moves the local cursor without any button semantics (global coordinates).
pub fn warp(x: f64, y: f64) {
    #[cfg(windows)]
    {
        // rdev's absolute moves only span the primary monitor; this spans them all.
        win::set_cursor(x, y);
    }
    #[cfg(not(windows))]
    if let Err(e) = rdev::simulate(&EventType::MouseMove { x, y }) {
        log::debug!("warp failed: {e:?}");
    }
}

/// Changes whenever a new drag starts (macOS drag pasteboard). 0 elsewhere.
pub fn drag_change_count() -> i64 {
    #[cfg(target_os = "macos")]
    return drag::change_count();
    #[cfg(not(target_os = "macos"))]
    0
}

/// Files in the current/last drag. Only macOS exposes this to other apps.
pub fn dragged_files() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    return drag::files();
    #[cfg(not(target_os = "macos"))]
    Vec::new()
}

/// The events that end a local drag without dropping anything: Escape cancels
/// the drag, then the button comes up harmlessly.
pub const CANCEL_DRAG: [EventType; 3] =
    [EventType::KeyPress(Key::Escape), EventType::KeyRelease(Key::Escape), EventType::ButtonRelease(Button::Left)];

pub fn cancel_local_drag() {
    for ev in CANCEL_DRAG {
        let _ = rdev::simulate(&ev);
    }
}

/// Replays remote input on this machine and remembers what is held down,
/// so everything can be released when control leaves.
#[derive(Default)]
pub struct Injector {
    keys_down: Vec<Key>,
    buttons_down: Vec<Button>,
    x: f64,
    y: f64,
    /// Swap Ctrl and Meta (Cmd/Win), for Mac <-> Windows muscle memory.
    pub swap_ctrl_meta: bool,
    /// Drag pasteboard count when the left button went down, to spot a drag starting.
    drag_count_at_press: i64,
    #[cfg(target_os = "macos")]
    clicks: mac::ClickTracker,
}

impl Injector {
    pub fn new(swap_ctrl_meta: bool) -> Self {
        Self { swap_ctrl_meta, ..Default::default() }
    }

    pub fn move_to(&mut self, x: f64, y: f64) {
        self.x = x;
        self.y = y;
        #[cfg(target_os = "macos")]
        mac::move_to(x, y, self.buttons_down.last().copied());
        #[cfg(windows)]
        win::send_move(x, y);
        #[cfg(not(any(target_os = "macos", windows)))]
        warp(x, y);
    }

    pub fn input(&mut self, ev: EventType) {
        let ev = match ev {
            EventType::KeyPress(k) => {
                let k = self.map_key(k);
                if !self.keys_down.contains(&k) {
                    self.keys_down.push(k);
                }
                EventType::KeyPress(k)
            }
            EventType::KeyRelease(k) => {
                let k = self.map_key(k);
                self.keys_down.retain(|d| *d != k);
                EventType::KeyRelease(k)
            }
            EventType::ButtonPress(b) => {
                if !self.buttons_down.contains(&b) {
                    self.buttons_down.push(b);
                }
                if b == Button::Left {
                    self.drag_count_at_press = drag_change_count();
                }
                #[cfg(target_os = "macos")]
                return self.clicks.button(b, true, self.x, self.y);
                #[cfg(not(target_os = "macos"))]
                ev
            }
            EventType::ButtonRelease(b) => {
                self.buttons_down.retain(|d| *d != b);
                #[cfg(target_os = "macos")]
                return self.clicks.button(b, false, self.x, self.y);
                #[cfg(not(target_os = "macos"))]
                ev
            }
            EventType::MouseMove { x, y } => return self.move_to(x, y),
            other => other,
        };
        if let Err(e) = rdev::simulate(&ev) {
            log::debug!("inject {ev:?} failed: {e:?}");
        }
    }

    /// Files being dragged here with the left button, if a drag is in progress.
    pub fn dragged_files(&self) -> Vec<PathBuf> {
        if self.buttons_down.contains(&Button::Left) && drag_change_count() != self.drag_count_at_press {
            dragged_files()
        } else {
            Vec::new()
        }
    }

    /// Release every key and button we pressed. Prevents stuck modifiers.
    /// A held left button may be mid-drag: Escape first so nothing gets dropped.
    pub fn release_all(&mut self) {
        if self.buttons_down.contains(&Button::Left) {
            let _ = rdev::simulate(&EventType::KeyPress(Key::Escape));
            let _ = rdev::simulate(&EventType::KeyRelease(Key::Escape));
        }
        for k in std::mem::take(&mut self.keys_down) {
            let _ = rdev::simulate(&EventType::KeyRelease(k));
        }
        for b in std::mem::take(&mut self.buttons_down) {
            #[cfg(target_os = "macos")]
            self.clicks.button(b, false, self.x, self.y);
            #[cfg(not(target_os = "macos"))]
            let _ = rdev::simulate(&EventType::ButtonRelease(b));
        }
    }

    fn map_key(&self, k: Key) -> Key {
        if !self.swap_ctrl_meta {
            return k;
        }
        match k {
            Key::ControlLeft => Key::MetaLeft,
            Key::ControlRight => Key::MetaRight,
            Key::MetaLeft => Key::ControlLeft,
            Key::MetaRight => Key::ControlRight,
            other => other,
        }
    }
}

/// Windows: monitor enumeration and cursor movement over the whole virtual desktop.
#[cfg(windows)]
mod win {
    use crate::screens::Rect;
    use std::mem::size_of;
    use windows_sys::Win32::Foundation::{BOOL, LPARAM, RECT};
    use windows_sys::Win32::Graphics::Gdi::{EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO};
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SetCursorPos, MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    unsafe extern "system" fn collect(mon: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let out = &mut *(data as *mut Vec<(Rect, bool)>);
        let mut info: MONITORINFO = std::mem::zeroed();
        info.cbSize = size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(mon, &mut info) != 0 {
            let r = info.rcMonitor;
            let rect = Rect::new(r.left as f64, r.top as f64, (r.right - r.left) as f64, (r.bottom - r.top) as f64);
            out.push((rect, info.dwFlags & MONITORINFOF_PRIMARY != 0));
        }
        1
    }

    pub fn monitors() -> Option<(Vec<Rect>, usize)> {
        let mut found: Vec<(Rect, bool)> = Vec::new();
        unsafe {
            EnumDisplayMonitors(std::ptr::null_mut(), std::ptr::null(), Some(collect), &mut found as *mut _ as LPARAM);
        }
        let primary = found.iter().position(|(_, p)| *p).unwrap_or(0);
        let rects: Vec<Rect> = found.into_iter().map(|(r, _)| r).collect();
        (!rects.is_empty()).then_some((rects, primary))
    }

    /// Exact placement; used to park the server's cursor.
    pub fn set_cursor(x: f64, y: f64) {
        unsafe {
            SetCursorPos(x.round() as i32, y.round() as i32);
        }
    }

    /// A real (injected) mouse move, so apps and drags see normal input.
    pub fn send_move(x: f64, y: f64) {
        unsafe {
            let vx = GetSystemMetrics(SM_XVIRTUALSCREEN) as f64;
            let vy = GetSystemMetrics(SM_YVIRTUALSCREEN) as f64;
            let vw = (GetSystemMetrics(SM_CXVIRTUALSCREEN) as f64 - 1.0).max(1.0);
            let vh = (GetSystemMetrics(SM_CYVIRTUALSCREEN) as f64 - 1.0).max(1.0);
            let input = INPUT {
                r#type: INPUT_MOUSE,
                Anonymous: INPUT_0 {
                    mi: MOUSEINPUT {
                        dx: ((x - vx) * 65535.0 / vw).round() as i32,
                        dy: ((y - vy) * 65535.0 / vh).round() as i32,
                        mouseData: 0,
                        dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            };
            SendInput(1, &input, size_of::<INPUT>() as i32);
        }
    }
}

/// Reads the system drag pasteboard, which macOS shares with every app.
#[cfg(target_os = "macos")]
#[allow(unexpected_cfgs)] // from the objc 0.2 macros
mod drag {
    use objc::runtime::{Class, Object, YES};
    use objc::{class, msg_send, sel, sel_impl};
    use std::ffi::{CStr, CString};
    use std::os::raw::c_char;
    use std::path::PathBuf;

    #[link(name = "AppKit", kind = "framework")]
    extern "C" {}

    type Id = *mut Object;

    unsafe fn nsstring(s: &str) -> Id {
        let c = CString::new(s).unwrap();
        msg_send![class!(NSString), stringWithUTF8String: c.as_ptr()]
    }

    /// NSPasteboardNameDrag
    unsafe fn pasteboard() -> Id {
        msg_send![class!(NSPasteboard), pasteboardWithName: nsstring("Apple CFPasteboard drag")]
    }

    fn with_pool<T>(f: impl FnOnce() -> T) -> T {
        unsafe {
            let pool: Id = msg_send![class!(NSAutoreleasePool), new];
            let out = f();
            let _: () = msg_send![pool, drain];
            out
        }
    }

    pub fn change_count() -> i64 {
        with_pool(|| unsafe {
            let pb = pasteboard();
            if pb.is_null() {
                return 0;
            }
            let n: isize = msg_send![pb, changeCount];
            n as i64
        })
    }

    pub fn files() -> Vec<PathBuf> {
        with_pool(|| unsafe {
            let mut out = Vec::new();
            let pb = pasteboard();
            if pb.is_null() {
                return out;
            }
            let url_class = class!(NSURL) as *const Class as Id;
            let classes: Id = msg_send![class!(NSArray), arrayWithObject: url_class];
            let yes: Id = msg_send![class!(NSNumber), numberWithBool: YES];
            let opts: Id = msg_send![class!(NSDictionary), dictionaryWithObject: yes forKey: nsstring("NSPasteboardURLReadingFileURLsOnlyKey")];
            let urls: Id = msg_send![pb, readObjectsForClasses: classes options: opts];
            if urls.is_null() {
                return out;
            }
            let n: usize = msg_send![urls, count];
            for i in 0..n {
                let url: Id = msg_send![urls, objectAtIndex: i];
                let path: Id = msg_send![url, path];
                if path.is_null() {
                    continue;
                }
                let c: *const c_char = msg_send![path, UTF8String];
                if !c.is_null() {
                    out.push(PathBuf::from(CStr::from_ptr(c).to_string_lossy().into_owned()));
                }
            }
            out
        })
    }
}

/// macOS needs real "dragged" events for drag-select and an explicit click count
/// for double-clicks; rdev's generic simulate does neither.
#[cfg(target_os = "macos")]
mod mac {
    use core_graphics::event::{CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, EventField};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use core_graphics::geometry::CGPoint;
    use rdev::Button;
    use std::time::{Duration, Instant};

    const DOUBLE_CLICK: Duration = Duration::from_millis(500);

    fn post(ty: CGEventType, x: f64, y: f64, button: CGMouseButton, clicks: i64) {
        let Ok(src) = CGEventSource::new(CGEventSourceStateID::HIDSystemState) else { return };
        if let Ok(ev) = CGEvent::new_mouse_event(src, ty, CGPoint::new(x, y), button) {
            if clicks > 0 {
                ev.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, clicks);
            }
            ev.post(CGEventTapLocation::HID);
        }
    }

    pub fn move_to(x: f64, y: f64, held: Option<Button>) {
        match held {
            Some(Button::Left) => post(CGEventType::LeftMouseDragged, x, y, CGMouseButton::Left, 0),
            Some(Button::Right) => post(CGEventType::RightMouseDragged, x, y, CGMouseButton::Right, 0),
            Some(_) => post(CGEventType::OtherMouseDragged, x, y, CGMouseButton::Center, 0),
            None => post(CGEventType::MouseMoved, x, y, CGMouseButton::Left, 0),
        }
    }

    #[derive(Default)]
    pub struct ClickTracker {
        last: Option<(Button, Instant, f64, f64)>,
        count: i64,
    }

    impl ClickTracker {
        pub fn button(&mut self, b: Button, down: bool, x: f64, y: f64) {
            let (down_ty, up_ty, cg) = match b {
                Button::Left => (CGEventType::LeftMouseDown, CGEventType::LeftMouseUp, CGMouseButton::Left),
                Button::Right => (CGEventType::RightMouseDown, CGEventType::RightMouseUp, CGMouseButton::Right),
                _ => (CGEventType::OtherMouseDown, CGEventType::OtherMouseUp, CGMouseButton::Center),
            };
            if down {
                let now = Instant::now();
                self.count = match self.last {
                    Some((lb, t, lx, ly))
                        if lb == b
                            && now.duration_since(t) < DOUBLE_CLICK
                            && (lx - x).abs() < 5.0
                            && (ly - y).abs() < 5.0 =>
                    {
                        self.count + 1
                    }
                    _ => 1,
                };
                self.last = Some((b, now, x, y));
            }
            post(if down { down_ty } else { up_ty }, x, y, cg, self.count.max(1));
        }
    }
}
