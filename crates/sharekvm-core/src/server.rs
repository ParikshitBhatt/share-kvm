//! Server: the machine whose physical mouse and keyboard are shared.
//!
//! A global hook watches the cursor. When it hits the configured edge, local
//! input is swallowed and forwarded to the client. While remote, the local
//! cursor is pinned to the screen centre and every move is reported as a delta
//! from the centre, which works whether or not the OS already moved the cursor.

use crate::clipboard;
use crate::discovery;
use crate::files;
use crate::link::{self, Link};
use crate::platform;
use crate::protocol::{describe_disconnect, Edge, Msg};
use crate::screens::Layout;
use crate::secure::{self, AuthError};
use crate::status::{emit, Status};
use crate::trust::{self, DeviceId, Role, TrustStore};
use anyhow::{anyhow, Context, Result};
use rdev::{Button, Event, EventType, Key};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};

const PING_EVERY: Duration = Duration::from_secs(2);
const PEER_TIMEOUT: Duration = Duration::from_secs(7);
/// Wrong pairing codes allowed before the server stops accepting attempts.
const MAX_FAILURES: u32 = 5;
const LOCKOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub struct ServerConfig {
    pub port: u16,
    /// Pairing code a new client must present. Remembered clients don't need it.
    pub code: String,
    /// Which edge of this screen leads to the client.
    pub edge: Edge,
    /// This computer's name, shown on clients.
    pub name: String,
    /// Where the device identity and remembered pairings live.
    pub data_dir: PathBuf,
}

#[derive(Default)]
struct State {
    remote: bool,
    warp_pending: bool,
    ctrl: bool,
    alt: bool,
    /// Keys/buttons pressed locally before switching; their releases stay local.
    local_keys: Vec<Key>,
    local_buttons: Vec<Button>,
    /// Drag pasteboard count at the last local left press (to spot a drag starting).
    left_press_count: i64,
    /// The left button was held while crossing: its release must not drop anything here.
    crossed_with_left: bool,
    /// Files dragged across the edge from this machine, sent on release.
    carrying: Vec<PathBuf>,
    /// The client is dragging files toward us; the next local release is the drop.
    drag_in_pending: bool,
    /// Synthetic events we injected ourselves that must reach this machine.
    passthrough: Vec<EventType>,
    conn: Option<Conn>,
    next_id: u64,
}

struct Conn {
    id: u64,
    link: Link,
    stream: TcpStream,
    name: String,
}

impl State {
    fn send(&self, msg: Msg) {
        if let Some(c) = &self.conn {
            c.link.send(msg);
        }
    }
}

/// Limits online guessing of the pairing code.
#[derive(Default)]
struct AuthGuard {
    failures: u32,
    locked_until: Option<Instant>,
}

impl AuthGuard {
    fn locked_for(&mut self) -> Option<Duration> {
        let until = self.locked_until?;
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            self.locked_until = None;
            None
        } else {
            Some(left)
        }
    }

    /// Returns the lockout length if this failure triggered one.
    fn fail(&mut self) -> Option<Duration> {
        self.failures += 1;
        if self.failures >= MAX_FAILURES {
            self.failures = 0;
            self.locked_until = Some(Instant::now() + LOCKOUT);
            Some(LOCKOUT)
        } else {
            None
        }
    }
}

/// Which edge leads to the client, and this computer's monitors (refreshed
/// in the background so plugging a monitor in or out just works).
struct Geometry {
    edge: Edge,
    layout: RwLock<Layout>,
}

impl Geometry {
    fn layout(&self) -> std::sync::RwLockReadGuard<'_, Layout> {
        self.layout.read().unwrap()
    }
}

const LAYOUT_REFRESH: Duration = Duration::from_secs(3);

fn describe(l: &Layout) -> String {
    let b = l.bbox();
    match l.monitors.len() {
        1 => format!("1 monitor, {}x{}", b.w, b.h),
        n => format!("{n} monitors, desktop {}x{} at ({}, {})", b.w, b.h, b.x, b.y),
    }
}

pub fn run_server(cfg: ServerConfig) -> Result<()> {
    platform::init();
    let layout = Layout::detect();
    log::info!("{}; client is beyond the {:?} edge", describe(&layout), cfg.edge);
    let bbox = layout.bbox();
    let g = Arc::new(Geometry { edge: cfg.edge, layout: RwLock::new(layout) });
    {
        let g = g.clone();
        thread::Builder::new().name("screens".into()).spawn(move || loop {
            thread::sleep(LAYOUT_REFRESH);
            let fresh = Layout::detect();
            if *g.layout() != fresh {
                log::info!("monitors changed: {}", describe(&fresh));
                *g.layout.write().unwrap() = fresh;
            }
        })?;
    }

    let state = Arc::new(Mutex::new(State::default()));
    let guard = Arc::new(Mutex::new(AuthGuard::default()));
    let trust = Arc::new(TrustStore::open(&cfg.data_dir).with_context(|| format!("open {}", cfg.data_dir.display()))?);
    let clip = clipboard::new_last();

    {
        let state = state.clone();
        clipboard::spawn_watcher(clip.clone(), move |text| state.lock().unwrap().send(Msg::Clipboard(text)));
    }

    let listener = TcpListener::bind(("0.0.0.0", cfg.port)).with_context(|| format!("bind port {}", cfg.port))?;
    log::info!("listening on port {}", cfg.port);
    emit(Status::Listening { port: cfg.port, width: bbox.w, height: bbox.h });
    // Discovery is a convenience: if mDNS is blocked, connecting by address still works.
    if let Err(e) = discovery::advertise(&cfg.name, &trust.device_id(), cfg.port) {
        log::warn!("not discoverable on the network: {e:#}");
    }
    {
        let (state, g, clip) = (state.clone(), g.clone(), clip.clone());
        thread::Builder::new().name("accept".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let (state, g, clip, cfg, guard, trust) =
                    (state.clone(), g.clone(), clip.clone(), cfg.clone(), guard.clone(), trust.clone());
                thread::spawn(move || {
                    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                    if let Err(e) = handle_client(stream, &cfg, &state, &guard, &trust, &g, &clip) {
                        log::warn!("client {peer}: {e:#}");
                    }
                });
            }
        })?;
    }

    // The hook must run on the main thread (macOS keyboard layout APIs require it).
    rdev::grab(move |ev| on_event(&state, &g, ev)).map_err(|e| {
        anyhow!("could not install input hook: {e:?}. On macOS, grant Accessibility and Input Monitoring permission to this app/terminal.")
    })
}

fn on_event(state: &Mutex<State>, g: &Geometry, ev: Event) -> Option<Event> {
    let mut st = state.lock().unwrap();
    let layout = g.layout();
    let (cx, cy) = layout.home();

    if let Some(i) = st.passthrough.iter().position(|e| *e == ev.event_type) {
        st.passthrough.remove(i);
        return Some(ev);
    }

    if !st.remote {
        match ev.event_type {
            EventType::MouseMove { x, y } if st.conn.is_some() && layout.at_outer_edge(g.edge, x, y) => {
                if st.local_buttons.contains(&Button::Left) {
                    st.crossed_with_left = true;
                    if platform::drag_change_count() != st.left_press_count {
                        st.carrying = platform::dragged_files();
                        if !st.carrying.is_empty() {
                            log::info!("carrying {} dragged item(s) across the edge", st.carrying.len());
                        }
                    }
                }
                st.send(Msg::Enter { pos: layout.edge_pos(g.edge, x, y) });
                st.remote = true;
                st.warp_pending = true;
                drop(st);
                emit(Status::Focus { remote: true });
                platform::warp(cx, cy);
                return None;
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
                    // Files dragged off the client were dropped here.
                    st.send(Msg::DropHere);
                }
            }
            _ => {}
        }
        return Some(ev);
    }

    match ev.event_type {
        EventType::MouseMove { x, y } => {
            let (dx, dy) = (x - cx, y - cy);
            // Let our own warp-to-centre through, or the cursor would never get there.
            if (st.warp_pending && dx.abs() <= 1.0 && dy.abs() <= 1.0) || (dx == 0.0 && dy == 0.0) {
                st.warp_pending = false;
                return Some(ev);
            }
            st.send(Msg::MouseDelta { dx, dy });
            st.warp_pending = true;
            drop(st);
            platform::warp(cx, cy);
            None
        }
        // Releases for things pressed before crossing belong to this machine.
        EventType::KeyRelease(k) if st.local_keys.contains(&k) => {
            st.local_keys.retain(|d| *d != k);
            update_modifiers(&mut st, k, false);
            Some(ev)
        }
        // Left button held since before crossing, released on the client: a drop there.
        // Cancel the local drag (Escape, then a harmless release) and send what was dragged.
        EventType::ButtonRelease(Button::Left) if st.crossed_with_left => {
            st.local_buttons.retain(|d| *d != Button::Left);
            st.crossed_with_left = false;
            let files = std::mem::take(&mut st.carrying);
            st.passthrough.extend(platform::CANCEL_DRAG);
            drop(st);
            platform::cancel_local_drag();
            if !files.is_empty() {
                log::info!("dropped {} item(s) on the client; sending", files.len());
                files::send_paths(files);
            }
            None
        }
        EventType::ButtonRelease(b) if st.local_buttons.contains(&b) => {
            st.local_buttons.retain(|d| *d != b);
            Some(ev)
        }
        EventType::KeyPress(Key::Escape) if st.ctrl && st.alt => {
            // Emergency hotkey: Ctrl+Alt+Esc takes control back.
            log::info!("hotkey: returning control to this machine");
            st.send(Msg::Release);
            st.remote = false;
            st.carrying.clear();
            st.crossed_with_left = false;
            st.ctrl = false;
            st.alt = false;
            drop(st);
            emit(Status::Focus { remote: false });
            platform::warp(cx, cy);
            None
        }
        other => {
            match other {
                EventType::KeyPress(k) => update_modifiers(&mut st, k, true),
                EventType::KeyRelease(k) => update_modifiers(&mut st, k, false),
                _ => {}
            }
            st.send(Msg::Input(other));
            None
        }
    }
}

fn update_modifiers(st: &mut State, k: Key, down: bool) {
    match k {
        Key::ControlLeft | Key::ControlRight => st.ctrl = down,
        Key::Alt | Key::AltGr => st.alt = down,
        _ => {}
    }
}

fn handle_client(
    stream: TcpStream,
    cfg: &ServerConfig,
    state: &Arc<Mutex<State>>,
    guard: &Mutex<AuthGuard>,
    trust: &TrustStore,
    g: &Geometry,
    clip: &clipboard::Last,
) -> Result<()> {
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(PEER_TIMEOUT))?;
    let addr = stream.peer_addr()?.to_string();

    if let Some(left) = guard.lock().unwrap().locked_for() {
        let mut w = stream.try_clone()?;
        let _ = secure::send_reject(&mut w, &format!("too many wrong pairing codes; try again in {}s", left.as_secs() + 1));
        return Err(anyhow!("refused {addr}: pairing locked for {}s", left.as_secs() + 1));
    }

    let saved_key = |id: &DeviceId| trust.find(Role::Client, id).map(|(_, secret)| secret);
    let session = match secure::server_handshake(stream.try_clone()?, stream.try_clone()?, trust.device_id(), &cfg.code, &saved_key) {
        Ok(s) => s,
        Err(e @ (AuthError::WrongCode | AuthError::StaleKey(_))) => {
            let locked = guard.lock().unwrap().fail();
            emit(Status::PairingFailed { addr: addr.clone(), locked_secs: locked.map_or(0, |d| d.as_secs()) });
            if let Some(d) = locked {
                log::warn!("too many wrong pairing codes; refusing connections for {}s", d.as_secs());
            }
            return Err(anyhow!("{addr}: {e}"));
        }
        Err(e) => return Err(e.into()),
    };
    guard.lock().unwrap().failures = 0;

    let (mut reader, mut writer, client_id) = (session.reader, session.writer, session.peer_id);
    let name = match session.first {
        Msg::Hello { name } => name,
        other => return Err(anyhow!("expected Hello, got {other:?}")),
    };
    // First pairing: issue a secret so this client can reconnect without the code.
    let pairing = if session.used_saved_key {
        let _ = trust.touch(Role::Client, &client_id, &name, None);
        None
    } else {
        let secret = trust::new_secret();
        trust.remember(Role::Client, &client_id, &name, &secret, None).context("save pairing")?;
        Some(secret.to_vec())
    };
    let newly_paired = pairing.is_some();
    writer.send(&Msg::Welcome { server_edge: cfg.edge.opposite(), server_name: cfg.name.clone(), pairing })?;
    log::info!(
        "client '{name}' connected from {addr} (encrypted, {})",
        if newly_paired { "newly paired" } else { "remembered" }
    );

    link::tune_socket(&stream);
    let link = link::spawn_writer(writer, stream.try_clone()?);
    let id = {
        let mut st = state.lock().unwrap();
        st.next_id += 1;
        let id = st.next_id;
        if let Some(old) = st.conn.take() {
            log::info!("replacing previous client '{}'", old.name);
            let _ = old.stream.shutdown(Shutdown::Both);
        }
        st.remote = false;
        st.drag_in_pending = false;
        st.conn = Some(Conn { id, link: link.clone(), stream: stream.try_clone()?, name: name.clone() });
        id
    };
    files::set_link(Some(link.clone()));
    emit(Status::PeerConnected { name: name.clone(), addr, newly_paired });

    // Keepalive, so a vanished client is noticed and control returns here.
    let ping = link.ctrl.clone();
    thread::spawn(move || {
        while ping.send(Msg::Ping).is_ok() {
            thread::sleep(PING_EVERY);
        }
    });
    drop(link);

    let result = loop {
        match reader.recv() {
            Ok(Msg::Leave { pos }) => {
                let mut st = state.lock().unwrap();
                if st.conn.as_ref().map(|c| c.id) == Some(id) && st.remote {
                    st.remote = false;
                    st.warp_pending = false;
                    // Still holding the button: the local drag simply carries on here.
                    st.carrying.clear();
                    st.crossed_with_left = false;
                    drop(st);
                    emit(Status::Focus { remote: false });
                    let (x, y) = g.layout().entry_point(g.edge, pos, 2.0);
                    platform::warp(x, y);
                }
            }
            Ok(Msg::Clipboard(text)) => clipboard::apply(clip, text),
            Ok(Msg::Ping) => {}
            Ok(Msg::DragOut) => state.lock().unwrap().drag_in_pending = true,
            Ok(m) if files::handle(&m) => {}
            Ok(other) => log::debug!("ignoring {other:?}"),
            Err(e) => break describe_disconnect(&e),
        }
    };

    let mut st = state.lock().unwrap();
    if st.conn.as_ref().map(|c| c.id) == Some(id) {
        files::set_link(None);
        st.drag_in_pending = false;
        if let Some(c) = st.conn.take() {
            let _ = c.stream.shutdown(Shutdown::Both);
        }
        if st.remote {
            st.remote = false;
            drop(st);
            let (cx, cy) = g.layout().home();
            platform::warp(cx, cy);
        }
        emit(Status::PeerDisconnected { name: name.clone(), reason: result.clone() });
    }
    log::info!("client '{name}' disconnected: {result}");
    Ok(())
}
