//! Two-way control, run identically at both ends of a connection.
//!
//! Each computer both captures its own input (the hook) and replays the
//! other's (the injector). At any moment one of three things is true here:
//!
//! - `Local`:   this computer's mouse and keyboard work on this computer.
//! - `Driving`: they've crossed the shared edge and are driving the other one.
//! - `Driven`:  the other computer's mouse and keyboard are driving this one.
//!
//! Messages (both directions):
//! - `Enter{pos}`   the sender starts driving the receiver.
//! - `MouseDelta`, `Input`  while driving.
//! - `Leave{pos}`   the driven side's cursor went back over the edge.
//! - `Release`      stop: whoever receives it returns to `Local`. Sent when the
//!                  driven computer's own mouse/keyboard is touched (it takes
//!                  over), or by the Ctrl+Alt+Esc hotkey on the driving side.
//!
//! Which computer listens and which connects (server/client) only matters for
//! pairing; control works the same in both directions.

use crate::files;
use crate::hook::Hooked;
use crate::link::Link;
use crate::platform::{self, Injector};
use crate::protocol::{Edge, Msg};
use crate::screens::{Layout, Step};
use crate::status::{emit, Status};
use rdev::{Button, EventType, Key};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock, RwLockReadGuard};
use std::thread;
use std::time::Duration;

const LAYOUT_REFRESH: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Local,
    Driving,
    Driven,
}

struct State {
    mode: Mode,
    /// The current connection: (id, link).
    link: Option<(u64, Link)>,
    /// Edge of this desktop that leads to the other computer.
    edge: Edge,

    // --- driving the other computer
    /// Windows: our warp to the parking spot is on its way through the hook.
    warp_pending: bool,
    /// Where the cursor crossed, to put it back if control is released.
    enter_pos: f64,
    ctrl: bool,
    alt: bool,
    /// Keys/buttons pressed locally before crossing; their releases stay local.
    local_keys: Vec<Key>,
    local_buttons: Vec<Button>,
    left_press_count: i64,
    crossed_with_left: bool,
    /// Files dragged across the edge from here, sent on release over there.
    carrying: Vec<PathBuf>,
    /// The other computer is dragging files toward us; our next release drops them.
    drag_in_pending: bool,
    /// Synthetic events we injected that must reach this computer.
    passthrough: Vec<EventType>,

    // --- being driven
    /// Virtual cursor position while driven.
    x: f64,
    y: f64,
    /// Files dragged off this screen toward the driver, sent once dropped there.
    dragging_out: Vec<PathBuf>,
}

pub struct Peer {
    st: Mutex<State>,
    /// Replays the other computer's input; separate lock so injecting never blocks the hook.
    inj: Mutex<Injector>,
    layout: RwLock<Layout>,
    /// Breaks ties if both computers start driving at the same instant.
    is_server: bool,
}

fn describe(l: &Layout) -> String {
    let b = l.bbox();
    match l.monitors.len() {
        1 => format!("1 monitor, {}x{}", b.w, b.h),
        n => format!("{n} monitors, desktop {}x{} at ({}, {})", b.w, b.h, b.x, b.y),
    }
}

impl Peer {
    pub fn new(edge: Edge, is_server: bool, swap_ctrl_meta: bool) -> Arc<Self> {
        let layout = Layout::detect();
        log::info!("{}", describe(&layout));
        let (x, y) = layout.home();
        let peer = Arc::new(Peer {
            st: Mutex::new(State {
                mode: Mode::Local,
                link: None,
                edge,
                warp_pending: false,
                enter_pos: 0.5,
                ctrl: false,
                alt: false,
                local_keys: Vec::new(),
                local_buttons: Vec::new(),
                left_press_count: 0,
                crossed_with_left: false,
                carrying: Vec::new(),
                drag_in_pending: false,
                passthrough: Vec::new(),
                x,
                y,
                dragging_out: Vec::new(),
            }),
            inj: Mutex::new(Injector::new(swap_ctrl_meta)),
            layout: RwLock::new(layout),
            is_server,
        });
        // Plugging a monitor in or out is picked up within a few seconds.
        let p = peer.clone();
        thread::Builder::new()
            .name("screens".into())
            .spawn(move || loop {
                thread::sleep(LAYOUT_REFRESH);
                let fresh = Layout::detect();
                if *p.layout() != fresh {
                    log::info!("monitors changed: {}", describe(&fresh));
                    *p.layout.write().unwrap() = fresh;
                }
            })
            .expect("spawn screens thread");
        peer
    }

    fn layout(&self) -> RwLockReadGuard<'_, Layout> {
        self.layout.read().unwrap()
    }

    pub fn bbox_size(&self) -> (f64, f64) {
        let b = self.layout().bbox();
        (b.w, b.h)
    }

    pub fn send(&self, msg: Msg) {
        if let Some((_, l)) = &self.st.lock().unwrap().link {
            l.send(msg);
        }
    }

    /// A connection is up. `edge`: the side of this desktop the other computer is on.
    pub fn connected(&self, id: u64, link: Link, edge: Edge) {
        self.reset();
        let mut st = self.st.lock().unwrap();
        st.link = Some((id, link));
        st.edge = edge;
        log::info!("the other computer is beyond my {edge:?} edge; either side can take control");
    }

    /// The connection `id` ended: give everything back to this computer.
    pub fn disconnected(&self, id: u64) {
        let mine = self.st.lock().unwrap().link.as_ref().map(|(i, _)| *i) == Some(id);
        if mine {
            self.reset();
            self.st.lock().unwrap().link = None;
        }
    }

    pub fn is_current(&self, id: u64) -> bool {
        self.st.lock().unwrap().link.as_ref().map(|(i, _)| *i) == Some(id)
    }

    /// Back to `Local` from whatever we were doing.
    fn reset(&self) {
        let mut st = self.st.lock().unwrap();
        let was = st.mode;
        st.mode = Mode::Local;
        st.drag_in_pending = false;
        st.carrying.clear();
        st.crossed_with_left = false;
        st.dragging_out.clear();
        drop(st);
        match was {
            Mode::Driving => {
                let (cx, cy) = self.layout().home();
                platform::leave_remote(cx, cy);
                emit(Status::Focus { remote: false, controlling: true });
            }
            Mode::Driven => {
                self.inj.lock().unwrap().release_all();
                emit(Status::Focus { remote: false, controlling: false });
            }
            Mode::Local => {}
        }
    }

    // ------------------------------------------------------------ this computer's input

    /// Handles one event from this computer's hook. Returns true to let it through.
    pub fn on_local(self: &Arc<Self>, h: Hooked) -> bool {
        let mut st = self.st.lock().unwrap();
        let ev = h.ev;

        if let Some(i) = st.passthrough.iter().position(|e| *e == ev) {
            st.passthrough.remove(i);
            return true;
        }
        // Our own replayed input (or other software's): never a reason to act.
        if h.injected {
            return true;
        }

        match st.mode {
            Mode::Driven => {
                // Someone is using this computer's own mouse or keyboard: they take over.
                let moved = match (ev, h.delta) {
                    (EventType::MouseMove { .. }, Some((dx, dy))) => dx != 0.0 || dy != 0.0,
                    _ => true,
                };
                if moved {
                    log::info!("local input while being driven: taking control back");
                    st.mode = Mode::Local;
                    if let Some((_, l)) = &st.link {
                        l.send(Msg::Release);
                    }
                    drop(st);
                    emit(Status::Focus { remote: false, controlling: false });
                    // Release held keys off the hook thread (injecting from inside a hook is unwise).
                    let me = self.clone();
                    thread::spawn(move || me.inj.lock().unwrap().release_all());
                }
                true
            }
            Mode::Local => self.local(st, ev, h.delta),
            Mode::Driving => self.driving(st, ev, h.delta),
        }
    }

    fn local(self: &Arc<Self>, mut st: std::sync::MutexGuard<'_, State>, ev: EventType, delta: Option<(f64, f64)>) -> bool {
        let layout = self.layout();
        match ev {
            EventType::MouseMove { x, y } if st.link.is_some() && layout.at_outer_edge(st.edge, x, y) => {
                if st.local_buttons.contains(&Button::Left) {
                    st.crossed_with_left = true;
                    if platform::drag_change_count() != st.left_press_count {
                        st.carrying = platform::dragged_files();
                        if !st.carrying.is_empty() {
                            log::info!("carrying {} dragged item(s) across the edge", st.carrying.len());
                        }
                    }
                }
                let pos = layout.edge_pos(st.edge, x, y);
                st.enter_pos = pos;
                if let Some((_, l)) = &st.link {
                    l.send(Msg::Enter { pos });
                }
                st.mode = Mode::Driving;
                st.warp_pending = delta.is_none();
                let (cx, cy) = layout.home();
                drop(st);
                emit(Status::Focus { remote: true, controlling: true });
                if delta.is_some() {
                    platform::freeze_cursor(true); // raw deltas: just hold the cursor still
                } else {
                    platform::warp(cx, cy); // measure moves from the parking spot
                }
                return false;
            }
            EventType::KeyPress(k) if !st.local_keys.contains(&k) => st.local_keys.push(k),
            EventType::KeyRelease(k) => st.local_keys.retain(|d| *d != k),
            EventType::ButtonPress(b) if !st.local_buttons.contains(&b) => {
                st.local_buttons.push(b);
                if b == Button::Left {
                    st.left_press_count = platform::drag_change_count();
                }
            }
            EventType::ButtonRelease(b) => {
                st.local_buttons.retain(|d| *d != b);
                if b == Button::Left && std::mem::take(&mut st.drag_in_pending) {
                    if let Some((_, l)) = &st.link {
                        l.send(Msg::DropHere); // files dragged off the other screen were dropped here
                    }
                }
            }
            _ => {}
        }
        true
    }

    fn driving(&self, mut st: std::sync::MutexGuard<'_, State>, ev: EventType, delta: Option<(f64, f64)>) -> bool {
        let send = |st: &State, m: Msg| {
            if let Some((_, l)) = &st.link {
                l.send(m);
            }
        };
        let layout = self.layout();
        let (cx, cy) = layout.home();
        match ev {
            EventType::MouseMove { .. } if delta.is_some() => {
                let (dx, dy) = delta.unwrap();
                if dx != 0.0 || dy != 0.0 {
                    send(&st, Msg::MouseDelta { dx, dy });
                }
                false
            }
            EventType::MouseMove { x, y } => {
                let (dx, dy) = (x - cx, y - cy);
                // Let our own warp to the parking spot through.
                if (st.warp_pending && dx.abs() <= 1.0 && dy.abs() <= 1.0) || (dx == 0.0 && dy == 0.0) {
                    st.warp_pending = false;
                    return true;
                }
                // Logical pixels, so the move is the same size on any display scale.
                let s = layout.scale_at(cx, cy);
                send(&st, Msg::MouseDelta { dx: dx / s, dy: dy / s });
                st.warp_pending = true;
                drop(st);
                platform::warp(cx, cy);
                false
            }
            // Releases for things pressed before crossing belong to this computer.
            EventType::KeyRelease(k) if st.local_keys.contains(&k) => {
                st.local_keys.retain(|d| *d != k);
                update_modifiers(&mut st, k, false);
                true
            }
            // Left button held since before crossing, released over there: a drop there.
            EventType::ButtonRelease(Button::Left) if st.crossed_with_left => {
                st.local_buttons.retain(|d| *d != Button::Left);
                st.crossed_with_left = false;
                let carried = std::mem::take(&mut st.carrying);
                st.passthrough.extend(platform::CANCEL_DRAG);
                drop(st);
                platform::cancel_local_drag();
                if !carried.is_empty() {
                    log::info!("dropped {} item(s) on the other computer; sending", carried.len());
                    files::send_paths(carried);
                }
                false
            }
            EventType::ButtonRelease(b) if st.local_buttons.contains(&b) => {
                st.local_buttons.retain(|d| *d != b);
                true
            }
            EventType::KeyPress(Key::Escape) if st.ctrl && st.alt => {
                log::info!("hotkey: returning control to this computer");
                send(&st, Msg::Release);
                st.mode = Mode::Local;
                st.carrying.clear();
                st.crossed_with_left = false;
                st.ctrl = false;
                st.alt = false;
                let (x, y) = layout.entry_point(st.edge, st.enter_pos, 2.0);
                drop(st);
                emit(Status::Focus { remote: false, controlling: true });
                platform::leave_remote(x, y);
                false
            }
            other => {
                match other {
                    EventType::KeyPress(k) => update_modifiers(&mut st, k, true),
                    EventType::KeyRelease(k) => update_modifiers(&mut st, k, false),
                    _ => {}
                }
                send(&st, Msg::Input(other));
                false
            }
        }
    }

    // ------------------------------------------------------------ the other computer's messages

    /// Handles a control message from connection `id`. Returns false if it isn't one.
    pub fn on_remote(&self, id: u64, msg: &Msg) -> bool {
        if !matches!(
            msg,
            Msg::Enter { .. } | Msg::MouseDelta { .. } | Msg::Input(_) | Msg::Leave { .. } | Msg::Release | Msg::DragOut | Msg::DropHere
        ) {
            return false;
        }
        if !self.is_current(id) {
            return true;
        }
        match *msg {
            Msg::Enter { pos } => self.enter(pos),
            Msg::MouseDelta { dx, dy } => self.driven_move(dx, dy),
            Msg::Input(ev) => {
                if self.st.lock().unwrap().mode == Mode::Driven {
                    self.inj.lock().unwrap().input(ev);
                }
            }
            Msg::Leave { pos } => self.came_back(Some(pos)),
            Msg::Release => {
                let mode = self.st.lock().unwrap().mode;
                match mode {
                    Mode::Driving => self.came_back(None), // the other side took over
                    Mode::Driven => self.reset(),          // driver pressed the hotkey
                    Mode::Local => {}
                }
            }
            Msg::DragOut => self.st.lock().unwrap().drag_in_pending = true,
            Msg::DropHere => {
                let out = std::mem::take(&mut self.st.lock().unwrap().dragging_out);
                if !out.is_empty() {
                    files::send_paths(out);
                }
            }
            _ => {}
        }
        true
    }

    /// The other computer starts driving this one.
    fn enter(&self, pos: f64) {
        let mut st = self.st.lock().unwrap();
        if st.mode == Mode::Driving {
            if self.is_server {
                // Both started at once: the computer that hosts the connection keeps control.
                if let Some((_, l)) = &st.link {
                    l.send(Msg::Release);
                }
                return;
            }
            drop(st);
            self.came_back(None);
            st = self.st.lock().unwrap();
        }
        let fresh = Layout::detect();
        if *self.layout() != fresh {
            log::info!("monitors changed: {}", describe(&fresh));
            *self.layout.write().unwrap() = fresh;
        }
        let (x, y) = self.layout().entry_point(st.edge, pos, 1.0);
        st.mode = Mode::Driven;
        st.x = x;
        st.y = y;
        st.dragging_out.clear();
        drop(st);
        self.inj.lock().unwrap().move_to(x, y);
        emit(Status::Focus { remote: true, controlling: false });
    }

    /// Movement from the driving computer, in logical pixels.
    fn driven_move(&self, dx: f64, dy: f64) {
        let mut st = self.st.lock().unwrap();
        if st.mode != Mode::Driven {
            return;
        }
        let layout = self.layout();
        let s = layout.scale_at(st.x, st.y);
        match layout.step(st.edge, (st.x, st.y), (st.x + dx * s, st.y + dy * s)) {
            Step::Move(x, y) => {
                st.x = x;
                st.y = y;
                drop(st);
                self.inj.lock().unwrap().move_to(x, y);
            }
            Step::Leave(pos) => {
                // Back over the edge: hand control back to the driver.
                st.mode = Mode::Local;
                let mut inj = self.inj.lock().unwrap();
                st.dragging_out = inj.dragged_files();
                if let Some((_, l)) = &st.link {
                    l.send(Msg::Leave { pos });
                    if !st.dragging_out.is_empty() {
                        log::info!("carrying {} dragged item(s) to the other computer", st.dragging_out.len());
                        l.send(Msg::DragOut);
                    }
                }
                drop(st);
                inj.release_all(); // cancels a local drag (Escape) before releasing
                drop(inj);
                emit(Status::Focus { remote: false, controlling: false });
            }
        }
    }

    /// We were driving; the cursor is back here (`pos` along our edge) or the other side took over.
    fn came_back(&self, pos: Option<f64>) {
        let mut st = self.st.lock().unwrap();
        if st.mode != Mode::Driving {
            return;
        }
        st.mode = Mode::Local;
        st.warp_pending = false;
        // Still holding the button: the local drag simply carries on here.
        st.carrying.clear();
        st.crossed_with_left = false;
        let (x, y) = self.layout().entry_point(st.edge, pos.unwrap_or(st.enter_pos), 2.0);
        drop(st);
        emit(Status::Focus { remote: false, controlling: true });
        platform::leave_remote(x, y);
    }
}

fn update_modifiers(st: &mut State, k: Key, down: bool) {
    match k {
        Key::ControlLeft | Key::ControlRight => st.ctrl = down,
        Key::Alt | Key::AltGr => st.alt = down,
        _ => {}
    }
}
