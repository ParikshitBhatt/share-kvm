//! ShareKVM on Android: the device is a *client*, controlled by a computer's
//! mouse and keyboard. This library runs the same encrypted session as the
//! desktop client (pairing, saved keys, protocol) and hands the Kotlin app
//! simple instructions through two callbacks:
//!
//! - `onCursor(x, y)`: move the on-screen pointer (pixels).
//! - `onEvent(json)`: everything else, e.g.
//!   `{"t":"show"}`, `{"t":"hide"}`,
//!   `{"t":"press","b":"left","x":..,"y":..}`, `{"t":"release",...}`,
//!   `{"t":"scroll","dx":..,"dy":..}`, `{"t":"text","s":"a"}`, `{"t":"key","k":"enter"}`,
//!   `{"t":"paste"}`, `{"t":"clipboard","s":"..."}`, `{"t":"status",...}`, `{"t":"log","s":"..."}`.

pub mod keys;

use anyhow::{anyhow, bail, Context, Result};
use keys::{Action, Keys};
use serde_json::{json, Value};
use sharekvm_core::link;
use sharekvm_core::protocol::{describe_disconnect, Button, EventType, Msg, DEFAULT_PORT};
use sharekvm_core::screens::{Layout, Rect, Step};
use sharekvm_core::secure::{self, AuthError};
use sharekvm_core::trust::{from_hex, to_hex, DeviceId, Role, TrustStore};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const PEER_TIMEOUT: Duration = Duration::from_secs(7);
const PING_EVERY: Duration = Duration::from_secs(2);
const RETRY_EVERY: Duration = Duration::from_secs(3);

/// Where instructions go. Implemented over JNI on Android, and by tests on the host.
pub trait Output: Send + Sync + 'static {
    fn cursor(&self, x: f64, y: f64);
    fn event(&self, v: Value);
}

#[derive(Debug, Clone)]
pub struct Config {
    pub data_dir: PathBuf,
    pub server: String,
    pub server_id: Option<DeviceId>,
    pub code: String,
    pub name: String,
}

impl Config {
    pub fn from_json(s: &str) -> Result<Self> {
        let v: Value = serde_json::from_str(s)?;
        let text = |k: &str| v[k].as_str().unwrap_or_default().trim().to_string();
        Ok(Config {
            data_dir: PathBuf::from(text("dataDir")),
            server: text("server"),
            server_id: from_hex(&text("serverId")),
            code: text("code"),
            name: Some(text("name")).filter(|n| !n.is_empty()).unwrap_or_else(|| "Android".into()),
        })
    }
}

/// Screen size in pixels, and how many pixels a desktop "point" of mouse
/// movement should cover here (density), so the pointer speed feels natural.
#[derive(Clone, Copy, Debug)]
pub struct Screen {
    pub w: f64,
    pub h: f64,
    pub scale: f64,
}

static SCREEN: Mutex<Screen> = Mutex::new(Screen { w: 1080.0, h: 1920.0, scale: 2.0 });

pub fn set_screen(w: f64, h: f64, density: f64) {
    *SCREEN.lock().unwrap() = Screen { w: w.max(1.0), h: h.max(1.0), scale: density.clamp(0.5, 6.0) };
}

fn screen() -> Screen {
    *SCREEN.lock().unwrap()
}

/// A running connection loop; dropping `stop` ends it.
struct Running {
    stop: Arc<AtomicBool>,
    socket: Arc<Mutex<Option<TcpStream>>>,
}

static RUNNING: Mutex<Option<Running>> = Mutex::new(None);

/// Starts (or restarts) the session in the background.
pub fn start(cfg: Config, out: Arc<dyn Output>) {
    stop();
    let stop = Arc::new(AtomicBool::new(false));
    let socket = Arc::new(Mutex::new(None));
    *RUNNING.lock().unwrap() = Some(Running { stop: stop.clone(), socket: socket.clone() });
    thread::Builder::new()
        .name("sharekvm".into())
        .spawn(move || run(cfg, out, stop, socket))
        .expect("spawn session thread");
}

pub fn stop() {
    if let Some(r) = RUNNING.lock().unwrap().take() {
        r.stop.store(true, Ordering::SeqCst);
        if let Some(s) = r.socket.lock().unwrap().take() {
            let _ = s.shutdown(Shutdown::Both); // unblocks the reader
        }
    }
}

fn status(out: &dyn Output, state: &str, extra: Value) {
    let mut v = json!({ "t": "status", "state": state });
    if let (Some(obj), Value::Object(more)) = (v.as_object_mut(), extra) {
        obj.extend(more);
    }
    out.event(v);
}

fn run(cfg: Config, out: Arc<dyn Output>, stop: Arc<AtomicBool>, socket: Arc<Mutex<Option<TcpStream>>>) {
    let trust = match TrustStore::open(&cfg.data_dir) {
        Ok(t) => t,
        Err(e) => return status(&*out, "error", json!({ "message": format!("Storage problem: {e}"), "needsUser": true })),
    };
    let server = if cfg.server.contains(':') { cfg.server.clone() } else { format!("{}:{DEFAULT_PORT}", cfg.server) };
    while !stop.load(Ordering::SeqCst) {
        status(&*out, "connecting", json!({ "addr": server }));
        match session(&cfg, &server, &trust, &*out, &stop, &socket) {
            Ok(()) => {}
            Err(e) => {
                let auth = e.downcast_ref::<AuthError>();
                let (message, fatal) = match auth {
                    Some(AuthError::NeedCode) => {
                        ("The computer doesn't know this device yet. Enter its pairing code.".to_string(), true)
                    }
                    Some(AuthError::WrongCode) => ("Wrong pairing code.".to_string(), true),
                    Some(AuthError::Rejected(r)) => (format!("Refused: {r}"), true),
                    Some(AuthError::StaleKey(id)) => {
                        let _ = trust.forget(Role::Server, &to_hex(id));
                        ("The computer forgot this device. Enter its pairing code.".to_string(), true)
                    }
                    _ => (format!("{e:#}"), false),
                };
                log::warn!("session ended: {message}");
                out.event(json!({ "t": "hide" }));
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                if fatal {
                    return status(&*out, "error", json!({ "message": message, "needsUser": true }));
                }
                status(&*out, "connecting", json!({ "addr": server, "message": message }));
            }
        }
        // Wait before retrying, but wake up promptly if stopped.
        for _ in 0..(RETRY_EVERY.as_millis() / 100) {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
    status(&*out, "stopped", json!({}));
}

fn session(
    cfg: &Config,
    server: &str,
    trust: &TrustStore,
    out: &dyn Output,
    stop: &AtomicBool,
    socket_slot: &Mutex<Option<TcpStream>>,
) -> Result<()> {
    let addr = server.to_socket_addrs()?.next().ok_or_else(|| anyhow!("can't find {server}"))?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).with_context(|| format!("can't reach {server}"))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(PEER_TIMEOUT))?;
    *socket_slot.lock().unwrap() = Some(stream.try_clone()?);
    if stop.load(Ordering::SeqCst) {
        bail!("stopped");
    }

    let saved_key = |id: &DeviceId| trust.find(Role::Server, id).map(|(_, secret)| secret);
    let session = secure::client_handshake(
        stream.try_clone()?,
        stream.try_clone()?,
        trust.device_id(),
        &trust.known_servers(),
        &cfg.code,
        &saved_key,
        &Msg::Hello { name: cfg.name.clone() },
    )?;
    let (mut reader, writer, server_id) = (session.reader, session.writer, session.peer_id);
    let (server_edge, server_name, pairing) = match session.first {
        Msg::Welcome { server_edge, server_name, pairing } => (server_edge, server_name, pairing),
        other => bail!("unexpected reply {other:?}"),
    };
    let newly_paired = match pairing.as_deref().map(<[u8; 32]>::try_from) {
        Some(Ok(secret)) => {
            trust.remember(Role::Server, &server_id, &server_name, &secret, Some(server.to_string()))?;
            true
        }
        Some(Err(_)) => bail!("malformed pairing key"),
        None => {
            let _ = trust.touch(Role::Server, &server_id, &server_name, Some(server.to_string()));
            false
        }
    };
    log::info!("connected to '{server_name}' (encrypted, {})", if newly_paired { "newly paired" } else { "remembered" });
    status(
        out,
        "connected",
        json!({ "peer": server_name, "addr": server, "serverId": to_hex(&server_id), "newlyPaired": newly_paired }),
    );

    link::tune_socket(&stream);
    let tx = link::spawn_writer(writer, stream.try_clone()?);
    {
        let ping = tx.ctrl.clone();
        thread::spawn(move || {
            while ping.send(Msg::Ping).is_ok() {
                thread::sleep(PING_EVERY);
            }
        });
    }

    let mut keys = Keys::default();
    let mut active = false;
    let (mut x, mut y) = (0.0, 0.0);
    let layout = |s: Screen| Layout::new(vec![Rect::new(0.0, 0.0, s.w, s.h)], 0);

    let err = loop {
        let msg = match reader.recv() {
            Ok(m) => m,
            Err(e) => break describe_disconnect(&e),
        };
        match msg {
            Msg::Enter { pos } => {
                (x, y) = layout(screen()).entry_point(server_edge, pos, 1.0);
                active = true;
                out.cursor(x, y);
                out.event(json!({ "t": "show" }));
                status(out, "active", json!({ "peer": server_name }));
            }
            Msg::MouseDelta { dx, dy } if active => {
                let s = screen();
                match layout(s).step(server_edge, (x, y), (x + dx * s.scale, y + dy * s.scale)) {
                    Step::Move(nx, ny) => {
                        (x, y) = (nx, ny);
                        out.cursor(x, y);
                    }
                    Step::Leave(pos) => {
                        tx.send(Msg::Leave { pos });
                        active = false;
                        keys.reset();
                        out.event(json!({ "t": "release_all" }));
                        out.event(json!({ "t": "hide" }));
                        status(out, "connected", json!({ "peer": server_name }));
                    }
                }
            }
            Msg::Input(ev) if active => input(out, &mut keys, ev, x, y),
            Msg::Release => {
                active = false;
                keys.reset();
                out.event(json!({ "t": "release_all" }));
                out.event(json!({ "t": "hide" }));
                status(out, "connected", json!({ "peer": server_name }));
            }
            Msg::Clipboard(text) => out.event(json!({ "t": "clipboard", "s": text })),
            Msg::FileOffer { id, .. } => {
                tx.send(Msg::FileCancel { id, reason: "this Android device can't receive files yet".into() });
            }
            _ => {}
        }
        if stop.load(Ordering::SeqCst) {
            break "stopped".to_string();
        }
    };
    let _ = stream.shutdown(Shutdown::Both);
    Err(anyhow!("disconnected: {err}"))
}

fn input(out: &dyn Output, keys: &mut Keys, ev: EventType, x: f64, y: f64) {
    let button = |b: Button| match b {
        Button::Left => Some("left"),
        Button::Right => Some("right"),
        _ => None,
    };
    match ev {
        EventType::ButtonPress(b) => {
            if let Some(b) = button(b) {
                out.event(json!({ "t": "press", "b": b, "x": x, "y": y }));
            }
        }
        EventType::ButtonRelease(b) => {
            if let Some(b) = button(b) {
                out.event(json!({ "t": "release", "b": b, "x": x, "y": y }));
            }
        }
        EventType::Wheel { delta_x, delta_y } => {
            out.event(json!({ "t": "scroll", "dx": delta_x, "dy": delta_y, "x": x, "y": y }));
        }
        other => match keys.handle(other) {
            Some(Action::Text(s)) => out.event(json!({ "t": "text", "s": s })),
            Some(Action::Key(k)) => out.event(json!({ "t": "key", "k": k })),
            Some(Action::Paste) => out.event(json!({ "t": "paste" })),
            None => {}
        },
    }
}

/// This install's device ID (hex), for display.
pub fn device_id(data_dir: &str) -> String {
    TrustStore::open(data_dir).map(|t| to_hex(&t.device_id())).unwrap_or_default()
}

/// Remembered computers as JSON (no secrets).
pub fn paired_json(data_dir: &str) -> String {
    let peers: Vec<Value> = TrustStore::open(data_dir)
        .map(|t| t.peers())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.role == Role::Server)
        .map(|p| json!({ "id": p.id, "name": p.name, "address": p.address, "lastSeen": p.last_seen }))
        .collect();
    Value::Array(peers).to_string()
}

pub fn forget(data_dir: &str, id: &str) -> bool {
    TrustStore::open(data_dir).and_then(|t| t.forget(Role::Server, id)).unwrap_or(false)
}

// ---------------------------------------------------------------- JNI

#[cfg(target_os = "android")]
mod jni_bridge {
    use super::*;
    use jni::objects::{GlobalRef, JClass, JObject, JString, JValue};
    use jni::sys::{jboolean, jfloat, jint, jstring, JNI_FALSE, JNI_TRUE};
    use jni::{JNIEnv, JavaVM};

    struct JavaOutput {
        vm: JavaVM,
        callbacks: GlobalRef,
    }

    impl JavaOutput {
        fn with_env(&self, f: impl FnOnce(&mut JNIEnv) -> jni::errors::Result<()>) {
            if let Ok(mut env) = self.vm.attach_current_thread_permanently() {
                if f(&mut env).is_err() || env.exception_check().unwrap_or(false) {
                    let _ = env.exception_describe();
                    let _ = env.exception_clear();
                }
            }
        }
    }

    impl Output for JavaOutput {
        fn cursor(&self, x: f64, y: f64) {
            self.with_env(|env| {
                env.call_method(&self.callbacks, "onCursor", "(FF)V", &[JValue::Float(x as f32), JValue::Float(y as f32)])
                    .map(|_| ())
            });
        }

        fn event(&self, v: Value) {
            self.with_env(|env| {
                let s = env.new_string(v.to_string())?;
                let r = env.call_method(&self.callbacks, "onEvent", "(Ljava/lang/String;)V", &[JValue::Object(&s)]);
                // This thread stays attached, so free local refs ourselves.
                let _ = env.delete_local_ref(s);
                r.map(|_| ())
            });
        }
    }

    fn string(env: &mut JNIEnv, s: &JString) -> String {
        env.get_string(s).map(|s| s.into()).unwrap_or_default()
    }

    fn init_logging() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            android_logger::init_once(android_logger::Config::default().with_max_level(log::LevelFilter::Info).with_tag("ShareKVM"));
        });
    }

    #[no_mangle]
    pub extern "system" fn Java_com_sharekvm_android_Native_start(mut env: JNIEnv, _: JClass, config: JString, callbacks: JObject) {
        init_logging();
        let cfg = string(&mut env, &config);
        let (Ok(vm), Ok(callbacks)) = (env.get_java_vm(), env.new_global_ref(callbacks)) else { return };
        let out: Arc<dyn Output> = Arc::new(JavaOutput { vm, callbacks });
        match Config::from_json(&cfg) {
            Ok(cfg) => start(cfg, out),
            Err(e) => status(&*out, "error", json!({ "message": format!("Bad settings: {e}"), "needsUser": true })),
        }
    }

    #[no_mangle]
    pub extern "system" fn Java_com_sharekvm_android_Native_stop(_: JNIEnv, _: JClass) {
        stop();
    }

    #[no_mangle]
    pub extern "system" fn Java_com_sharekvm_android_Native_setScreen(_: JNIEnv, _: JClass, w: jint, h: jint, density: jfloat) {
        set_screen(w as f64, h as f64, density as f64);
    }

    #[no_mangle]
    pub extern "system" fn Java_com_sharekvm_android_Native_deviceId(mut env: JNIEnv, _: JClass, dir: JString) -> jstring {
        let dir = string(&mut env, &dir);
        env.new_string(device_id(&dir)).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
    }

    #[no_mangle]
    pub extern "system" fn Java_com_sharekvm_android_Native_paired(mut env: JNIEnv, _: JClass, dir: JString) -> jstring {
        let dir = string(&mut env, &dir);
        env.new_string(paired_json(&dir)).map(|s| s.into_raw()).unwrap_or(std::ptr::null_mut())
    }

    #[no_mangle]
    pub extern "system" fn Java_com_sharekvm_android_Native_forget(mut env: JNIEnv, _: JClass, dir: JString, id: JString) -> jboolean {
        let (dir, id) = (string(&mut env, &dir), string(&mut env, &id));
        if forget(&dir, &id) {
            JNI_TRUE
        } else {
            JNI_FALSE
        }
    }
}
