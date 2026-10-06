//! Remembered devices.
//!
//! Every install has a random device ID. After a successful code pairing the
//! server issues a 256-bit secret for that pair; both sides save it here and
//! later reconnect by running SPAKE2 with the secret instead of the code.
//!
//! Files (in the data directory):
//! - `identity.json`: this install's device ID
//! - `paired.json`:   remembered peers, including their secrets (owner-only permissions)

use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub type DeviceId = [u8; 16];
pub type Secret = [u8; 32];

/// What a remembered peer is to us.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A computer we control (we are its server).
    Client,
    /// A computer that controls us.
    Server,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    /// Hex device ID.
    pub id: String,
    pub name: String,
    pub role: Role,
    /// Hex secret. Never shown in the UI.
    pub secret: String,
    /// Last address we reached it at (clients only record the server's).
    #[serde(default)]
    pub address: Option<String>,
    pub paired_at: u64,
    pub last_seen: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdentityFile {
    device_id: String,
}

pub struct TrustStore {
    dir: PathBuf,
    device_id: DeviceId,
    /// Serialises read-modify-write within this process.
    lock: Mutex<()>,
}

impl TrustStore {
    /// Opens (creating if needed) the store in `dir`.
    pub fn open(dir: impl Into<PathBuf>) -> io::Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let id_path = dir.join("identity.json");
        let device_id = match fs::read(&id_path).ok().and_then(|b| serde_json::from_slice::<IdentityFile>(&b).ok()) {
            Some(f) => from_hex(&f.device_id).ok_or_else(|| invalid("bad device id in identity.json"))?,
            None => {
                let mut id = [0u8; 16];
                OsRng.fill_bytes(&mut id);
                write_atomic(&id_path, &serde_json::to_vec_pretty(&IdentityFile { device_id: to_hex(&id) })?)?;
                id
            }
        };
        Ok(Self { dir, device_id, lock: Mutex::new(()) })
    }

    pub fn device_id(&self) -> DeviceId {
        self.device_id
    }

    /// All remembered peers. Re-read every time, since the desktop app edits the file too.
    pub fn peers(&self) -> Vec<Peer> {
        fs::read(self.dir.join("paired.json"))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn find(&self, role: Role, id: &DeviceId) -> Option<(Peer, Secret)> {
        let hex = to_hex(id);
        let peer = self.peers().into_iter().find(|p| p.role == role && p.id == hex)?;
        let secret = from_hex(&peer.secret)?;
        Some((peer, secret))
    }

    /// IDs of every server we hold a secret for.
    pub fn known_servers(&self) -> Vec<DeviceId> {
        self.peers()
            .iter()
            .filter(|p| p.role == Role::Server)
            .filter_map(|p| from_hex(&p.id))
            .collect()
    }

    /// Saves (or replaces) a pairing.
    pub fn remember(&self, role: Role, id: &DeviceId, name: &str, secret: &Secret, address: Option<String>) -> io::Result<()> {
        let now = now();
        self.update(|peers| {
            let hex = to_hex(id);
            let paired_at = peers.iter().find(|p| p.role == role && p.id == hex).map_or(now, |p| p.paired_at);
            peers.retain(|p| !(p.role == role && p.id == hex));
            peers.push(Peer {
                id: hex,
                name: name.to_string(),
                role,
                secret: to_hex(secret),
                address,
                paired_at,
                last_seen: now,
            });
        })
    }

    /// Records a successful reconnect.
    pub fn touch(&self, role: Role, id: &DeviceId, name: &str, address: Option<String>) -> io::Result<()> {
        let hex = to_hex(id);
        self.update(|peers| {
            if let Some(p) = peers.iter_mut().find(|p| p.role == role && p.id == hex) {
                p.last_seen = now();
                p.name = name.to_string();
                if address.is_some() {
                    p.address = address;
                }
            }
        })
    }

    /// Forgets a peer. Returns whether anything was removed.
    pub fn forget(&self, role: Role, id_hex: &str) -> io::Result<bool> {
        let mut removed = false;
        self.update(|peers| {
            let before = peers.len();
            peers.retain(|p| !(p.role == role && p.id == id_hex));
            removed = peers.len() != before;
        })?;
        Ok(removed)
    }

    fn update(&self, f: impl FnOnce(&mut Vec<Peer>)) -> io::Result<()> {
        let _guard = self.lock.lock().unwrap();
        let mut peers = self.peers();
        f(&mut peers);
        write_atomic(&self.dir.join("paired.json"), &serde_json::to_vec_pretty(&peers)?)
    }
}

pub fn new_secret() -> Secret {
    let mut s = [0u8; 32];
    OsRng.fill_bytes(&mut s);
    s
}

/// Default data directory for the engine when the app doesn't pass one.
pub fn default_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("SHAREKVM_HOME") {
        return d.into();
    }
    let home = || std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| ".".into());
    if cfg!(windows) {
        std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(home).join("sharekvm")
    } else if cfg!(target_os = "macos") {
        home().join("Library/Application Support/sharekvm")
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config")).join("sharekvm")
    }
}

pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn from_hex<const N: usize>(s: &str) -> Option<[u8; N]> {
    if s.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

/// Write via a temp file + rename so a crash never leaves a half-written file.
/// Secrets live in these files, so they are readable by the owner only.
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("sharekvm-test-{tag}-{}", to_hex(&new_secret()[..4])));
        let _ = fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn identity_is_stable_and_peers_roundtrip() {
        let dir = temp_dir("store");
        let a = TrustStore::open(&dir).unwrap();
        let b = TrustStore::open(&dir).unwrap();
        assert_eq!(a.device_id(), b.device_id());

        let peer_id = [5u8; 16];
        let secret = new_secret();
        a.remember(Role::Server, &peer_id, "Office PC", &secret, Some("10.0.0.2".into())).unwrap();
        let (p, s) = b.find(Role::Server, &peer_id).unwrap();
        assert_eq!(s, secret);
        assert_eq!(p.name, "Office PC");
        assert_eq!(b.known_servers(), vec![peer_id]);
        assert!(b.find(Role::Client, &peer_id).is_none(), "roles are separate");

        assert!(a.forget(Role::Server, &to_hex(&peer_id)).unwrap());
        assert!(b.find(Role::Server, &peer_id).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn hex_roundtrip() {
        let id = [0xab, 0x01, 0xff, 0x10];
        assert_eq!(from_hex::<4>(&to_hex(&id)), Some(id));
        assert_eq!(from_hex::<4>("zz"), None);
    }
}
