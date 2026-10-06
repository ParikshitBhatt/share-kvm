//! Client: the computer that connects to a host (and reconnects, and finds it
//! again by device ID if its address changes).
//!
//! Control is two-way and lives in `peer`: once connected, either computer's
//! mouse and keyboard can drive the other.

use crate::clipboard;
use crate::discovery;
use crate::files;
use crate::hook;
use crate::link;
use crate::peer::Peer;
use crate::platform;
use crate::protocol::{describe_disconnect, Edge, Msg, DEFAULT_PORT};
use crate::secure::{self, AuthError};
use crate::status::{emit, Status};
use crate::trust::{from_hex, to_hex, DeviceId, Role, TrustStore};
use anyhow::{anyhow, bail, Context, Result};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
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

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Connects to the server and reconnects forever, while this computer's
/// input hook runs on the main thread (macOS requires it).
pub fn run_client(cfg: ClientConfig) -> Result<()> {
    platform::init();
    let trust = TrustStore::open(&cfg.data_dir).with_context(|| format!("open {}", cfg.data_dir.display()))?;
    // The real edge arrives with the server's Welcome.
    let peer = Peer::new(Edge::Left, false, cfg.swap_ctrl_meta);
    let clip = clipboard::new_last();
    {
        let peer = peer.clone();
        clipboard::spawn_watcher(clip.clone(), move |text| peer.send(Msg::Clipboard(text)));
    }
    {
        let peer = peer.clone();
        thread::Builder::new().name("session".into()).spawn(move || {
            if let Err(e) = connect_forever(&cfg, &trust, &clip, &peer) {
                // Retrying can't fix this (e.g. a wrong pairing code): report and stop.
                eprintln!("Error: {e:#}");
                std::process::exit(1);
            }
        })?;
    }
    hook::run(move |h| peer.on_local(h))
        .map_err(|e| anyhow!("{e}. On macOS, grant Accessibility and Input Monitoring permission to this app/terminal."))
}

fn connect_forever(cfg: &ClientConfig, trust: &TrustStore, clip: &clipboard::Last, peer: &Arc<Peer>) -> Result<()> {
    let mut server = with_port(&cfg.server);
    loop {
        match session(cfg, &server, trust, clip, peer) {
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
                let id = cfg.server_id.or_else(|| remembered_id_for(trust, &server));
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

fn session(cfg: &ClientConfig, server: &str, trust: &TrustStore, clip: &clipboard::Last, peer: &Arc<Peer>) -> Result<()> {
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
    let id = NEXT_ID.fetch_add(1, Ordering::SeqCst);
    peer.connected(id, tx.clone(), server_edge);
    files::set_link(Some(tx.clone()));
    {
        let ping = tx.ctrl.clone();
        thread::spawn(move || {
            while ping.send(Msg::Ping).is_ok() {
                thread::sleep(PING_EVERY);
            }
        });
    }
    drop(tx);

    let err = loop {
        match reader.recv() {
            Ok(m) if peer.on_remote(id, &m) => {}
            Ok(Msg::Clipboard(text)) => clipboard::apply(clip, text),
            Ok(Msg::Ping) => {}
            Ok(m) if files::handle(&m) => {}
            Ok(other) => log::debug!("ignoring {other:?}"),
            Err(e) => break describe_disconnect(&e),
        }
    };

    peer.disconnected(id);
    files::set_link(None);
    let _ = stream.shutdown(Shutdown::Both);
    emit(Status::PeerDisconnected { name: addr.to_string(), reason: err.clone() });
    Err(anyhow!("disconnected: {err}"))
}
