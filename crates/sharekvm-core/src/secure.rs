//! Encrypted transport.
//!
//! 1. **Pairing (SPAKE2).** Both sides run a password-authenticated key exchange
//!    keyed by the pairing code. The code never crosses the wire, a recorded
//!    session can't be used to brute-force it offline, and an active attacker
//!    gets one guess per connection (the server also locks out repeat failures).
//! 2. **Traffic (ChaCha20-Poly1305).** The SPAKE2 secret is expanded with HKDF
//!    into one key per direction. Every frame is sealed with a counter nonce, so
//!    tampering, replay or reordering breaks the connection.
//!
//! Computers that paired before skip the code: SPAKE2 runs with the 256-bit
//! secret saved after the first pairing (see `trust`).
//!
//! Wire format (all frames are `u32 BE length + payload`):
//! ```text
//! C -> S  "SKVM" | version u32 | client id[16] | n u8 | n x known server id[16]
//! S -> C  "SKVM" | version u32 | server id[16] | mode u8 | spake2 msg   or "RJCT" | reason
//!           mode 0 = pairing code, 1 = saved key
//! C -> S  spake2 msg
//! C -> S  sealed(Hello)
//! S -> C  sealed(Welcome)                        or  "RJCT" | "wrong pairing code"
//! ...     sealed(Msg) both ways
//! ```

use crate::protocol::{decode, encode, read_frame, write_frame, Msg, PROTOCOL_VERSION};
use crate::trust::{DeviceId, Secret};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hkdf::Hkdf;
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use std::fmt;
use std::io::{self, Read, Write};

const MAGIC: &[u8; 4] = b"SKVM";
const REJECT: &[u8; 4] = b"RJCT";
const MODE_CODE: u8 = 0;
const MODE_KEY: u8 = 1;
const MAX_KNOWN: usize = 64;

#[derive(Debug)]
pub enum AuthError {
    /// The peer's pairing code differs from ours.
    WrongCode,
    /// A saved key didn't match: one side forgot or replaced the pairing.
    StaleKey(DeviceId),
    /// Client: the server doesn't remember us and no pairing code was given.
    NeedCode,
    /// The server refused before key exchange (version mismatch, lockout).
    Rejected(String),
    Io(io::Error),
    Protocol(String),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::WrongCode => f.write_str("wrong pairing code"),
            AuthError::StaleKey(_) => f.write_str("saved pairing is no longer valid; pair again with the code"),
            AuthError::NeedCode => f.write_str("the other computer doesn't remember this one; enter its pairing code"),
            AuthError::Rejected(r) => write!(f, "rejected: {r}"),
            AuthError::Io(e) => write!(f, "{e}"),
            AuthError::Protocol(p) => write!(f, "protocol error: {p}"),
        }
    }
}

impl std::error::Error for AuthError {}

impl From<io::Error> for AuthError {
    fn from(e: io::Error) -> Self {
        AuthError::Io(e)
    }
}

/// One direction of an AEAD channel with a monotonically increasing nonce.
struct Sealer {
    cipher: ChaCha20Poly1305,
    counter: u64,
}

impl Sealer {
    fn new(key: &[u8; 32]) -> Self {
        Self { cipher: ChaCha20Poly1305::new(Key::from_slice(key)), counter: 0 }
    }

    fn next_nonce(&mut self) -> Nonce {
        let mut n = [0u8; 12];
        n[4..].copy_from_slice(&self.counter.to_be_bytes());
        self.counter += 1;
        *Nonce::from_slice(&n)
    }

    fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let nonce = self.next_nonce();
        self.cipher.encrypt(&nonce, plaintext).expect("chacha20poly1305 encrypt")
    }

    fn open(&mut self, ciphertext: &[u8]) -> Option<Vec<u8>> {
        let nonce = self.next_nonce();
        self.cipher.decrypt(&nonce, ciphertext).ok()
    }
}

pub struct SecureWriter<W: Write> {
    inner: W,
    sealer: Sealer,
}

impl<W: Write> SecureWriter<W> {
    pub fn send(&mut self, msg: &Msg) -> io::Result<()> {
        let ct = self.sealer.seal(&encode(msg)?);
        write_frame(&mut self.inner, &ct)
    }

    pub fn into_inner(self) -> W {
        self.inner
    }
}

pub struct SecureReader<R: Read> {
    inner: R,
    sealer: Sealer,
}

impl<R: Read> SecureReader<R> {
    pub fn recv(&mut self) -> io::Result<Msg> {
        let frame = read_frame(&mut self.inner)?;
        let pt = self
            .sealer
            .open(&frame)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "decryption failed (tampered or wrong key)"))?;
        decode(&pt)
    }
}

fn preamble(body: &[&[u8]]) -> Vec<u8> {
    let mut v = [&MAGIC[..], &PROTOCOL_VERSION.to_be_bytes()].concat();
    for part in body {
        v.extend_from_slice(part);
    }
    v
}

/// Returns the bytes after magic + version, or a reason to reject the peer.
fn parse_preamble(frame: &[u8]) -> Result<&[u8], String> {
    if frame.len() < 8 || &frame[..4] != MAGIC {
        return Err("not a ShareKVM peer, or an older version".into());
    }
    let version = u32::from_be_bytes(frame[4..8].try_into().unwrap());
    if version != PROTOCOL_VERSION {
        return Err(format!("version mismatch (this side {PROTOCOL_VERSION}, other side {version}); update both computers"));
    }
    Ok(&frame[8..])
}

fn reject_reason(frame: &[u8]) -> Option<String> {
    frame.strip_prefix(&REJECT[..]).map(|r| String::from_utf8_lossy(r).into_owned())
}

/// Sends a plaintext refusal. Only used before a sealed session exists.
pub fn send_reject<W: Write>(w: &mut W, reason: &str) -> io::Result<()> {
    write_frame(w, &[&REJECT[..], reason.as_bytes()].concat())
}

fn derive(secret: &[u8]) -> ([u8; 32], [u8; 32]) {
    let hk = Hkdf::<Sha256>::new(Some(b"sharekvm/v3"), secret);
    let (mut c2s, mut s2c) = ([0u8; 32], [0u8; 32]);
    hk.expand(b"client->server", &mut c2s).expect("hkdf length");
    hk.expand(b"server->client", &mut s2c).expect("hkdf length");
    (c2s, s2c)
}

/// SPAKE2 password, domain-separated so a code can never collide with a key.
fn password(mode: u8, code: &str, key: Option<&Secret>) -> Vec<u8> {
    match (mode, key) {
        (MODE_KEY, Some(k)) => [&b"key:"[..], k].concat(),
        _ => [&b"code:"[..], code.trim().as_bytes()].concat(),
    }
}

/// Binds the exchange to both device IDs, so a key can't be replayed for another pair.
fn identities(client: &DeviceId, server: &DeviceId) -> (Identity, Identity) {
    (
        Identity::new(&[&b"sharekvm client "[..], client].concat()),
        Identity::new(&[&b"sharekvm server "[..], server].concat()),
    )
}

fn id16(b: &[u8]) -> Option<DeviceId> {
    b.get(..16)?.try_into().ok()
}

pub struct Session<R: Read, W: Write> {
    pub reader: SecureReader<R>,
    pub writer: SecureWriter<W>,
    /// The peer's first sealed message (Hello for servers, Welcome for clients).
    pub first: Msg,
    pub peer_id: DeviceId,
    /// True if a saved key was used instead of the pairing code.
    pub used_saved_key: bool,
}

/// Client side of the handshake. `saved_key` looks up our secret for a server ID.
pub fn client_handshake<R: Read, W: Write>(
    mut r: R,
    mut w: W,
    device_id: DeviceId,
    known_servers: &[DeviceId],
    code: &str,
    saved_key: &dyn Fn(&DeviceId) -> Option<Secret>,
    hello: &Msg,
) -> Result<Session<R, W>, AuthError> {
    let known: Vec<u8> = known_servers.iter().take(MAX_KNOWN).flatten().copied().collect();
    let count = [(known.len() / 16) as u8];
    write_frame(&mut w, &preamble(&[&device_id, &count, &known]))?;

    let reply = read_frame(&mut r)?;
    if let Some(reason) = reject_reason(&reply) {
        return Err(AuthError::Rejected(reason));
    }
    let body = parse_preamble(&reply).map_err(AuthError::Protocol)?;
    let (server_id, mode, server_msg) = match (id16(body), body.get(16)) {
        (Some(id), Some(&mode)) => (id, mode, &body[17..]),
        _ => return Err(AuthError::Protocol("short server hello".into())),
    };
    let key = match mode {
        MODE_KEY => Some(saved_key(&server_id).ok_or_else(|| AuthError::Protocol("server expects a saved key we don't have".into()))?),
        MODE_CODE if code.trim().is_empty() => return Err(AuthError::NeedCode),
        MODE_CODE => None,
        other => return Err(AuthError::Protocol(format!("unknown pairing mode {other}"))),
    };

    let (id_a, id_b) = identities(&device_id, &server_id);
    let (spake, msg) = Spake2::<Ed25519Group>::start_a(&Password::new(&password(mode, code, key.as_ref())), &id_a, &id_b);
    write_frame(&mut w, &msg)?;
    let secret = spake.finish(server_msg).map_err(|e| AuthError::Protocol(format!("key exchange failed: {e:?}")))?;
    let (c2s, s2c) = derive(&secret);
    let mut writer = SecureWriter { inner: w, sealer: Sealer::new(&c2s) };
    let mut reader = SecureReader { inner: r, sealer: Sealer::new(&s2c) };

    writer.send(hello)?;
    // A mismatch means the server can't read our Hello and answers in plaintext.
    let frame = read_frame(&mut reader.inner)?;
    let first = match reader.sealer.open(&frame) {
        Some(pt) => decode(&pt)?,
        None if reject_reason(&frame).is_some() => {
            return Err(if mode == MODE_KEY { AuthError::StaleKey(server_id) } else { AuthError::WrongCode })
        }
        None => return Err(AuthError::Protocol("could not decrypt server reply".into())),
    };
    Ok(Session { reader, writer, first, peer_id: server_id, used_saved_key: mode == MODE_KEY })
}

/// Server side. Uses the saved key if both sides remember each other, else the code.
pub fn server_handshake<R: Read, W: Write>(
    mut r: R,
    mut w: W,
    server_id: DeviceId,
    code: &str,
    saved_key: &dyn Fn(&DeviceId) -> Option<Secret>,
) -> Result<Session<R, W>, AuthError> {
    let hello = read_frame(&mut r)?;
    let body = match parse_preamble(&hello) {
        Ok(b) => b,
        Err(reason) => {
            let _ = send_reject(&mut w, &reason);
            return Err(AuthError::Rejected(reason));
        }
    };
    let client_id = id16(body).ok_or_else(|| AuthError::Protocol("short client hello".into()))?;
    let n = *body.get(16).unwrap_or(&0) as usize;
    let known = body.get(17..17 + n * 16).ok_or_else(|| AuthError::Protocol("short client hello".into()))?;
    let client_knows_us = known.chunks_exact(16).any(|id| id == server_id);

    let key = if client_knows_us { saved_key(&client_id) } else { None };
    let mode = if key.is_some() { MODE_KEY } else { MODE_CODE };
    let (id_a, id_b) = identities(&client_id, &server_id);
    let (spake, msg) = Spake2::<Ed25519Group>::start_b(&Password::new(&password(mode, code, key.as_ref())), &id_a, &id_b);
    write_frame(&mut w, &preamble(&[&server_id, &[mode], &msg]))?;

    let client_msg = read_frame(&mut r)?;
    let secret = spake.finish(&client_msg).map_err(|e| AuthError::Protocol(format!("key exchange failed: {e:?}")))?;
    let (c2s, s2c) = derive(&secret);
    let mut reader = SecureReader { inner: r, sealer: Sealer::new(&c2s) };
    let mut writer = SecureWriter { inner: w, sealer: Sealer::new(&s2c) };

    let frame = read_frame(&mut reader.inner)?;
    let first = match reader.sealer.open(&frame) {
        Some(pt) => decode(&pt)?,
        None => {
            let _ = send_reject(&mut writer.inner, "wrong pairing code");
            return Err(if mode == MODE_KEY { AuthError::StaleKey(client_id) } else { AuthError::WrongCode });
        }
    };
    Ok(Session { reader, writer, first, peer_id: client_id, used_saved_key: mode == MODE_KEY })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Edge;
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    const SERVER: DeviceId = [1; 16];
    const CLIENT: DeviceId = [2; 16];

    struct Side {
        code: &'static str,
        key: Option<Secret>,
    }

    /// Runs one connection. Returns (server result, client result) as (used_saved_key, echoed msg).
    fn connect(server: Side, client: Side) -> (Result<bool, AuthError>, Result<(bool, Msg), AuthError>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            let lookup = |id: &DeviceId| (*id == CLIENT).then_some(server.key).flatten();
            let mut sess = server_handshake(s.try_clone().unwrap(), s, SERVER, server.code, &lookup)?;
            assert!(matches!(sess.first, Msg::Hello { ref name } if name == "laptop"));
            assert_eq!(sess.peer_id, CLIENT);
            sess.writer.send(&Msg::Welcome { server_edge: Edge::Left, server_name: "desk".into(), pairing: None })?;
            let next = sess.reader.recv()?;
            sess.writer.send(&next)?;
            Ok(sess.used_saved_key)
        });
        let s = TcpStream::connect(addr).unwrap();
        let known: Vec<DeviceId> = client.key.iter().map(|_| SERVER).collect();
        let lookup = |id: &DeviceId| (*id == SERVER).then_some(client.key).flatten();
        let cli = client_handshake(s.try_clone().unwrap(), s, CLIENT, &known, client.code, &lookup, &Msg::Hello { name: "laptop".into() })
            .and_then(|mut sess| {
                assert!(matches!(sess.first, Msg::Welcome { server_edge: Edge::Left, .. }));
                sess.writer.send(&Msg::Clipboard("secret text".into()))?;
                Ok((sess.used_saved_key, sess.reader.recv()?))
            });
        (srv.join().unwrap(), cli)
    }

    #[test]
    fn right_code_connects_and_encrypts() {
        let (server, client) = connect(Side { code: "482913", key: None }, Side { code: "482913", key: None });
        assert!(!server.unwrap());
        let (saved, echoed) = client.unwrap();
        assert!(!saved);
        assert!(matches!(echoed, Msg::Clipboard(t) if t == "secret text"));
    }

    #[test]
    fn wrong_code_is_rejected_on_both_sides() {
        let (server, client) = connect(Side { code: "482913", key: None }, Side { code: "000000", key: None });
        assert!(matches!(server, Err(AuthError::WrongCode)));
        assert!(matches!(client, Err(AuthError::WrongCode)));
    }

    #[test]
    fn saved_key_connects_without_code() {
        let key = [42u8; 32];
        let (server, client) = connect(Side { code: "482913", key: Some(key) }, Side { code: "", key: Some(key) });
        assert!(server.unwrap(), "server should use the saved key");
        assert!(client.unwrap().0, "client should use the saved key");
    }

    #[test]
    fn forgotten_client_needs_code() {
        // Client still has a key, but the server forgot it: falls back to the code.
        let (server, client) = connect(Side { code: "482913", key: None }, Side { code: "", key: Some([42; 32]) });
        assert!(matches!(client, Err(AuthError::NeedCode)));
        assert!(matches!(server, Err(AuthError::Io(_))), "not counted as a wrong guess");
        let (server, client) = connect(Side { code: "482913", key: None }, Side { code: "482913", key: Some([42; 32]) });
        assert!(!server.unwrap());
        assert!(!client.unwrap().0);
    }

    #[test]
    fn mismatched_keys_are_stale() {
        let (server, client) = connect(Side { code: "482913", key: Some([1; 32]) }, Side { code: "", key: Some([2; 32]) });
        assert!(matches!(server, Err(AuthError::StaleKey(_))));
        assert!(matches!(client, Err(AuthError::StaleKey(_))));
    }

    #[test]
    fn tampering_is_detected() {
        let mut a = Sealer::new(&[7u8; 32]);
        let mut b = Sealer::new(&[7u8; 32]);
        let mut ct = a.seal(b"hello");
        ct[0] ^= 1;
        assert!(b.open(&ct).is_none());
    }

    #[test]
    fn replayed_frame_is_rejected() {
        let mut a = Sealer::new(&[9u8; 32]);
        let mut b = Sealer::new(&[9u8; 32]);
        let ct = a.seal(b"one");
        assert_eq!(b.open(&ct).unwrap(), b"one");
        assert!(b.open(&ct).is_none(), "same ciphertext must not open twice");
    }
}
