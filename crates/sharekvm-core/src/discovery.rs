//! LAN discovery over mDNS / DNS-SD (the protocol behind Bonjour).
//!
//! Servers advertise `_sharekvm._tcp.local.` with TXT records:
//! `id` (device ID, hex), `name` (display name), `v` (protocol version).
//! Clients browse to list servers, or look one up by device ID when its IP changes.
//!
//! Discovery only finds computers. Anything advertised is untrusted until the
//! encrypted pairing handshake succeeds.

use crate::protocol::PROTOCOL_VERSION;
use crate::trust::{to_hex, DeviceId};
use anyhow::{Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use serde::Serialize;
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const SERVICE: &str = "_sharekvm._tcp.local.";

/// A server seen on the network.
#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Found {
    /// Device ID (hex), matches `trust::Peer::id` once paired.
    pub id: String,
    pub name: String,
    /// Best address first ("192.168.1.20").
    pub addresses: Vec<String>,
    pub port: u16,
    pub version: u32,
    /// mDNS instance name, used to match removals.
    pub fullname: String,
}

impl Found {
    pub fn socket_addr(&self) -> Option<SocketAddr> {
        let ip: IpAddr = self.addresses.first()?.parse().ok()?;
        Some(SocketAddr::new(ip, self.port))
    }
}

#[derive(Debug)]
pub enum Event {
    Found(Found),
    Lost { fullname: String },
}

struct Advert {
    daemon: ServiceDaemon,
    fullname: String,
}

static ADVERT: Mutex<Option<Advert>> = Mutex::new(None);

/// Starts announcing this server. Replaces any earlier announcement.
pub fn advertise(name: &str, device_id: &DeviceId, port: u16) -> Result<()> {
    let hex = to_hex(device_id);
    // Instance names are DNS labels (max 63 bytes); the id suffix keeps them unique.
    let short: String = name.chars().filter(|c| !c.is_control() && *c != '.').take(40).collect();
    let instance = format!("{} ({})", short.trim(), &hex[..6]);
    let host = format!("sharekvm-{}.local.", &hex[..12]);
    let version = PROTOCOL_VERSION.to_string();
    let props = [("id", hex.as_str()), ("name", name), ("v", version.as_str())];
    let info = ServiceInfo::new(SERVICE, &instance, &host, "", port, &props[..])
        .context("build mDNS record")?
        .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    let daemon = ServiceDaemon::new().context("start mDNS")?;
    daemon.register(info).context("register mDNS service")?;
    log::info!("advertising '{instance}' on the local network");
    if let Some(old) = ADVERT.lock().unwrap().replace(Advert { daemon, fullname }) {
        stop(old);
    }
    Ok(())
}

/// Sends an mDNS "goodbye" so browsers drop us right away. Call before exiting.
pub fn stop_advertising() {
    if let Some(a) = ADVERT.lock().unwrap().take() {
        stop(a);
    }
}

fn stop(a: Advert) {
    if let Ok(rx) = a.daemon.unregister(&a.fullname) {
        let _ = rx.recv_timeout(Duration::from_millis(500));
    }
    let _ = a.daemon.shutdown();
}

/// Browses for servers until the returned daemon is dropped/shut down.
pub fn browse(mut on_event: impl FnMut(Event) + Send + 'static) -> Result<ServiceDaemon> {
    let daemon = ServiceDaemon::new().context("start mDNS")?;
    let rx = daemon.browse(SERVICE).context("browse mDNS")?;
    std::thread::Builder::new().name("mdns-browse".into()).spawn(move || {
        while let Ok(ev) = rx.recv() {
            match ev {
                ServiceEvent::ServiceResolved(info) => {
                    if let Some(found) = to_found(&info) {
                        on_event(Event::Found(found));
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => on_event(Event::Lost { fullname }),
                _ => {}
            }
        }
    })?;
    Ok(daemon)
}

/// Looks up a server by device ID, waiting up to `timeout`.
pub fn find(device_id: &DeviceId, timeout: Duration) -> Option<Found> {
    let want = to_hex(device_id);
    let daemon = ServiceDaemon::new().ok()?;
    let rx = daemon.browse(SERVICE).ok()?;
    let deadline = Instant::now() + timeout;
    let mut result = None;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                if let Some(f) = to_found(&info).filter(|f| f.id == want) {
                    result = Some(f);
                    break;
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    result
}

fn to_found(info: &ServiceInfo) -> Option<Found> {
    let id = info.get_property_val_str("id")?.to_string();
    if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // IPv6 link-local (fe80::) addresses need an interface scope to connect, which
    // a plain address string can't carry, so skip them. Addresses arrive in several
    // updates; returning None waits for one with a usable address.
    let mut ips: Vec<IpAddr> = info
        .get_addresses()
        .iter()
        .copied()
        .filter(|ip| !ip.is_loopback() && !matches!(ip, IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) == 0xfe80))
        .collect();
    // Prefer routable IPv4, then link-local IPv4, then global IPv6.
    ips.sort_by_key(|ip| match ip {
        IpAddr::V4(v4) if !v4.is_link_local() => 0,
        IpAddr::V4(_) => 1,
        IpAddr::V6(_) => 2,
    });
    if ips.is_empty() {
        return None;
    }
    Some(Found {
        id,
        name: info.get_property_val_str("name").unwrap_or("Unknown computer").to_string(),
        addresses: ips.iter().map(|ip| ip.to_string()).collect(),
        port: info.get_port(),
        version: info.get_property_val_str("v").and_then(|v| v.parse().ok()).unwrap_or(0),
        fullname: info.get_fullname().to_string(),
    })
}
