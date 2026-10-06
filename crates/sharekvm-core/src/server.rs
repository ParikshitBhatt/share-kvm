//! Server: the computer that hosts the connection (listens, pairs, advertises).
//!
//! Control itself is two-way and lives in `peer`: once connected, either
//! computer's mouse and keyboard can drive the other.

use crate::clipboard;
use crate::discovery;
use crate::files;
use crate::hook;
use crate::link;
use crate::peer::Peer;
use crate::platform;
use crate::protocol::{describe_disconnect, Edge, Msg};
use crate::secure::{self, AuthError};
use crate::status::{emit, Status};
use crate::trust::{self, DeviceId, Role, TrustStore};
use anyhow::{anyhow, Context, Result};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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

/// The connected client, so a newer connection can replace it.
struct Conn {
    id: u64,
    stream: TcpStream,
    name: String,
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

pub fn run_server(cfg: ServerConfig) -> Result<()> {
    platform::init();
    let peer = Peer::new(cfg.edge, true, false);
    let (w, h) = peer.bbox_size();
    let conn: Arc<Mutex<Option<Conn>>> = Arc::default();
    let next_id = Arc::new(AtomicU64::new(1));
    let guard = Arc::new(Mutex::new(AuthGuard::default()));
    let trust = Arc::new(TrustStore::open(&cfg.data_dir).with_context(|| format!("open {}", cfg.data_dir.display()))?);
    let clip = clipboard::new_last();
    {
        let peer = peer.clone();
        clipboard::spawn_watcher(clip.clone(), move |text| peer.send(Msg::Clipboard(text)));
    }

    let listener = TcpListener::bind(("0.0.0.0", cfg.port)).with_context(|| format!("bind port {}", cfg.port))?;
    log::info!("listening on port {}; the other computer is beyond the {:?} edge", cfg.port, cfg.edge);
    emit(Status::Listening { port: cfg.port, width: w, height: h });
    // Discovery is a convenience: if mDNS is blocked, connecting by address still works.
    if let Err(e) = discovery::advertise(&cfg.name, &trust.device_id(), cfg.port) {
        log::warn!("not discoverable on the network: {e:#}");
    }
    {
        let (peer, clip) = (peer.clone(), clip.clone());
        thread::Builder::new().name("accept".into()).spawn(move || {
            for stream in listener.incoming().flatten() {
                let (peer, clip, cfg, guard, trust, conn, next_id) =
                    (peer.clone(), clip.clone(), cfg.clone(), guard.clone(), trust.clone(), conn.clone(), next_id.clone());
                thread::spawn(move || {
                    let addr = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
                    if let Err(e) = handle_client(stream, &cfg, &peer, &conn, &next_id, &guard, &trust, &clip) {
                        log::warn!("client {addr}: {e:#}");
                    }
                });
            }
        })?;
    }

    // The hook must run on the main thread (macOS requires it).
    hook::run(move |h| peer.on_local(h))
        .map_err(|e| anyhow!("{e}. On macOS, grant Accessibility and Input Monitoring permission to this app/terminal."))
}

#[allow(clippy::too_many_arguments)]
fn handle_client(
    stream: TcpStream,
    cfg: &ServerConfig,
    peer: &Arc<Peer>,
    conn: &Mutex<Option<Conn>>,
    next_id: &AtomicU64,
    guard: &Mutex<AuthGuard>,
    trust: &TrustStore,
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
    let id = next_id.fetch_add(1, Ordering::SeqCst);
    if let Some(old) = conn.lock().unwrap().replace(Conn { id, stream: stream.try_clone()?, name: name.clone() }) {
        log::info!("replacing previous client '{}'", old.name);
        let _ = old.stream.shutdown(Shutdown::Both);
    }
    peer.connected(id, link.clone(), cfg.edge);
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
            Ok(m) if peer.on_remote(id, &m) => {}
            Ok(Msg::Clipboard(text)) => clipboard::apply(clip, text),
            Ok(Msg::Ping) => {}
            Ok(m) if files::handle(&m) => {}
            Ok(other) => log::debug!("ignoring {other:?}"),
            Err(e) => break describe_disconnect(&e),
        }
    };

    let mut c = conn.lock().unwrap();
    if c.as_ref().map(|c| c.id) == Some(id) {
        if let Some(c) = c.take() {
            let _ = c.stream.shutdown(Shutdown::Both);
        }
        drop(c);
        files::set_link(None);
        peer.disconnected(id);
        emit(Status::PeerDisconnected { name: name.clone(), reason: result.clone() });
    }
    log::info!("client '{name}' disconnected: {result}");
    Ok(())
}
