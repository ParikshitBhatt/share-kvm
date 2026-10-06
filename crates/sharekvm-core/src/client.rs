//! Client: a machine controlled by the server's mouse and keyboard.
//!
//! The client keeps a virtual cursor position. Deltas from the server move it;
//! moving past the edge that faces the server hands control back.

use crate::clipboard;
use crate::discovery;
use crate::files;
use crate::link::{self, Link};
use crate::platform::{self, Injector};
use crate::protocol::{describe_disconnect, Msg, DEFAULT_PORT};
use crate::screens::{Layout, Step};
use crate::secure::{self, AuthError};
use crate::status::{emit, Status};
use crate::trust::{from_hex, to_hex, DeviceId, Role, TrustStore};
use anyhow::{anyhow, bail, Context, Result};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const PING_EVERY: Duration = Duration::from_secs(2);
const PEER_TIMEOUT: Duration = Duration::from_secs(7);
const RETRY_EVERY: Duration = Duration::from_secs(2);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Debug)]
pub struct ClientConfig {
    /// "host" or "host:port".
    pub server: String,
    /// The server's device ID, if known. Used to find it again on the LAN if its address changes.
    pub server_id: Option<DeviceId>,
    /// Pairing code; only needed until the server remembers this computer.
    pub code: String,
    pub name: String,
    pub swap_ctrl_meta: bool,
    /// Where the device identity and remembered pairings live.
    pub data_dir: PathBuf,
}

type CurrentTx = Arc<Mutex<Option<Link>>>;

/// Connects to the server and reconnects forever.
pub fn run_client(cfg: ClientConfig) -> Result<()> {
    platform::init();
    let trust = TrustStore::open(&cfg.data_dir).with_context(|| format!("open {}", cfg.data_dir.display()))?;
    let clip = clipboard::new_last();
    let current: CurrentTx = Arc::default();
    {
        let current = current.clone();
        clipboard::spawn_watcher(clip.clone(), move |text| {
            if let Some(tx) = &*current.lock().unwrap() {
                let _ = tx.send(Msg::Clipboard(text));
            }
        });
    }

    let mut server = with_port(&cfg.server);
    loop {
        match session(&cfg, &server, &trust, &clip, &current) {
            // Retrying can't fix these; let the user act.
            Err(e)
                if matches!(
                    e.downcast_ref::<AuthError>(),
                    Some(AuthError::WrongCode | AuthError::Rejected(_) | AuthError::NeedCode)
                ) =>
            {
                return Err(e)
            }
            // The stale key was dropped; retry right away with the code (if any).
            Err(e) if matches!(e.downcast_ref::<AuthError>(), Some(AuthError::StaleKey(_))) => {
                log::warn!("{e}");
                continue;
            }
            Err(e) => {
                log::warn!("{e:#}");
                // Maybe the server got a new IP address: look it up by device ID.
                let id = cfg.server_id.or_else(|| remembered_id_for(&trust, &server));
                if let Some(found) = id.and_then(|id| discovery::find(&id, LOOKUP_TIMEOUT)) {
                    if let Some(addr) = found.socket_addr().map(|a| a.to_string()).filter(|a| *a != server) {
                        log::info!("'{}' is now at {addr}", found.name);
                        server = addr;
                        continue;
                    }
                }
                log::info!("retrying in {}s", RETRY_EVERY.as_secs());
            }
            Ok(()) => {}
        }
        *current.lock().unwrap() = None;
        thread::sleep(RETRY_EVERY);
    }
}

fn with_port(server: &str) -> String {
    let server = server.trim();
    // Bare IPv6 addresses contain ':' too; only treat a single ':' as host:port.
    if server.matches(':').count() == 1 || server.starts_with('[') {
        server.to_string()
    } else if server.contains(':') {
        format!("[{server}]:{DEFAULT_PORT}")
    } else {
        format!("{server}:{DEFAULT_PORT}")
    }
}

/// Device ID of the remembered server last seen at `addr`.
fn remembered_id_for(trust: &TrustStore, addr: &str) -> Option<DeviceId> {
    trust
        .peers()
        .into_iter()
        .find(|p| p.role == Role::Server && p.address.as_deref() == Some(addr))
        .and_then(|p| from_hex(&p.id))
}

fn session(cfg: &ClientConfig, server: &str, trust: &TrustStore, clip: &clipboard::Last, current: &CurrentTx) -> Result<()> {
    let addr_str = server.to_string();
    emit(Status::Connecting { addr: addr_str.clone() });
    let addr = addr_str.to_socket_addrs()?.next().ok_or_else(|| anyhow!("cannot resolve {addr_str}"))?;
    let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5)).with_context(|| format!("connect {addr}"))?;
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(PEER_TIMEOUT))?;

    let hello = Msg::Hello { name: cfg.name.clone() };
    let saved_key = |id: &DeviceId| trust.find(Role::Server, id).map(|(_, secret)| secret);
    let session = secure::client_handshake(
        stream.try_clone()?,
        stream.try_clone()?,
        trust.device_id(),
        &trust.known_servers(),
        &cfg.code,
        &saved_key,
        &hello,
    );
    let session = match session {
        Ok(s) => s,
        Err(AuthError::StaleKey(server_id)) => {
            // The server no longer accepts our saved key (it re-paired or forgot us). Drop it.
            let _ = trust.forget(Role::Server, &crate::trust::to_hex(&server_id));
            return Err(AuthError::StaleKey(server_id).into());
        }
        Err(AuthError::NeedCode) => {
            emit(Status::NeedsCode);
            return Err(AuthError::NeedCode.into());
        }
        Err(e @ (AuthError::WrongCode | AuthError::Rejected(_))) => {
            let reason = match &e {
                AuthError::Rejected(r) => r.clone(),
                _ => "wrong pairing code".into(),
            };
            emit(Status::Rejected { reason });
            return Err(e.into());
        }
        Err(e) => return Err(e.into()),
    };
    let (mut reader, writer, server_id) = (session.reader, session.writer, session.peer_id);
    let (server_edge, server_name, pairing) = match session.first {
        Msg::Welcome { server_edge, server_name, pairing } => (server_edge, server_name, pairing),
        other => bail!("unexpected reply {other:?}"),
    };
    let newly_paired = match pairing.as_deref().map(<[u8; 32]>::try_from) {
        Some(Ok(secret)) => {
            trust.remember(Role::Server, &server_id, &server_name, &secret, Some(addr_str.clone())).context("save pairing")?;
            true
        }
        Some(Err(_)) => bail!("server sent a malformed pairing key"),
        None => {
            let _ = trust.touch(Role::Server, &server_id, &server_name, Some(addr_str.clone()));
            false
        }
    };
    log::info!(
        "connected to '{server_name}' at {addr} (encrypted, {}); server is beyond my {server_edge:?} edge",
        if newly_paired { "newly paired" } else { "remembered" }
    );
    emit(Status::Connected { addr: addr_str.clone(), server_id: to_hex(&server_id), server_edge, server_name, newly_paired });

    link::tune_socket(&stream);
    let tx = link::spawn_writer(writer, stream.try_clone()?);
    *current.lock().unwrap() = Some(tx.clone());
    files::set_link(Some(tx.clone()));
    {
        let ping = tx.ctrl.clone();
        thread::spawn(move || {
            while ping.send(Msg::Ping).is_ok() {
                thread::sleep(PING_EVERY);
            }
        });
    }

    let mut layout = Layout::detect();
    let mut inj = Injector::new(cfg.swap_ctrl_meta);
    let mut active = false;
    let (mut x, mut y) = layout.home();
    // Files dragged off this screen toward the server, sent once dropped there.
    let mut dragging_out: Vec<PathBuf> = Vec::new();

    let err = loop {
        let msg = match reader.recv() {
            Ok(m) => m,
            Err(e) => break describe_disconnect(&e),
        };
        match msg {
            Msg::Enter { pos } => {
                // Re-read monitors on every entry, so plugging one in or out is picked up.
                let fresh = Layout::detect();
                if fresh != layout {
                    log::info!("using {} monitor(s)", fresh.monitors.len());
                    layout = fresh;
                }
                (x, y) = layout.entry_point(server_edge, pos, 1.0);
                active = true;
                dragging_out.clear(); // came back without dropping on the server
                inj.move_to(x, y);
                emit(Status::Focus { remote: true });
            }
            Msg::MouseDelta { dx, dy } if active => {
                // Moves arrive in logical pixels; scale to the monitor the cursor is on.
                let s = layout.scale_at(x, y);
                let (nx, ny) = match layout.step(server_edge, (x, y), (x + dx * s, y + dy * s)) {
                    Step::Move(nx, ny) => (nx, ny),
                    Step::Leave(pos) => {
                    dragging_out = inj.dragged_files();
                    tx.send(Msg::Leave { pos });
                    if !dragging_out.is_empty() {
                        log::info!("carrying {} dragged item(s) to the server", dragging_out.len());
                        tx.send(Msg::DragOut);
                    }
                    inj.release_all(); // cancels the local drag (Escape) before releasing
                    active = false;
                    emit(Status::Focus { remote: false });
                    continue;
                    }
                };
                (x, y) = (nx, ny);
                inj.move_to(x, y);
            }
            Msg::Input(ev) if active => inj.input(ev),
            Msg::Release => {
                inj.release_all();
                active = false;
                emit(Status::Focus { remote: false });
            }
            Msg::Clipboard(text) => clipboard::apply(clip, text),
            Msg::DropHere if !dragging_out.is_empty() => {
                files::send_paths(std::mem::take(&mut dragging_out));
            }
            m if files::handle(&m) => {}
            Msg::Ping | Msg::MouseDelta { .. } | Msg::Input(_) | Msg::DropHere => {}
            other => log::debug!("ignoring {other:?}"),
        }
    };

    inj.release_all();
    files::set_link(None);
    let _ = stream.shutdown(Shutdown::Both);
    emit(Status::PeerDisconnected { name: addr.to_string(), reason: err.clone() });
    Err(anyhow!("disconnected: {err}"))
}
