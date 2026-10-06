//! Wire protocol: length-prefixed frames carrying bincode messages.
//! On the network every message is encrypted; see `secure`.

// Desktop builds use rdev's event types directly; others use identical copies.
#[cfg(not(feature = "desktop"))]
pub use crate::input::{Button, EventType, Key};
#[cfg(feature = "desktop")]
pub use rdev::{Button, EventType, Key};
use serde::{Deserialize, Serialize};
use std::io::{self, Read, Write};
use std::str::FromStr;

pub const PROTOCOL_VERSION: u32 = 5;
pub const DEFAULT_PORT: u16 = 24801;
const MAX_FRAME: usize = 16 * 1024 * 1024;

/// A side of a screen.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }
}

impl FromStr for Edge {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "left" => Ok(Edge::Left),
            "right" => Ok(Edge::Right),
            "top" | "up" => Ok(Edge::Top),
            "bottom" | "down" => Ok(Edge::Bottom),
            _ => Err(format!("invalid edge '{s}' (expected left/right/top/bottom)")),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Msg {
    /// client -> server, first sealed message after pairing.
    Hello { name: String },
    /// server -> client. `server_edge` is the edge of the *client's* screen that faces the server.
    /// `pairing` carries a new 32-byte secret after a code pairing, for the client to save.
    Welcome { server_edge: Edge, server_name: String, pairing: Option<Vec<u8>> },
    /// server -> client: cursor crossed over; `pos` is the 0..1 position along the shared edge.
    Enter { pos: f64 },
    /// server -> client: relative mouse motion while the client is active.
    MouseDelta { dx: f64, dy: f64 },
    /// server -> client: key / button / wheel event to replay.
    Input(EventType),
    /// client -> server: cursor left the client through the shared edge.
    Leave { pos: f64 },
    /// server -> client: control was taken back (hotkey); release held keys.
    Release,
    /// either direction: clipboard text changed.
    Clipboard(String),
    /// either direction: keepalive.
    Ping,
    /// either direction: start of a file transfer, listing every entry.
    FileOffer { id: u64, entries: Vec<crate::files::FileEntry> },
    /// either direction: next bytes of entry `index`, in order.
    FileChunk {
        id: u64,
        index: u32,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// either direction: all chunks sent.
    FileEnd { id: u64 },
    /// receiver -> sender: everything arrived and was saved.
    FileReceived { id: u64 },
    /// either direction: transfer stopped (by a user or an error).
    FileCancel { id: u64, reason: String },
    /// client -> server: files are being dragged off the client's screen toward the server.
    DragOut,
    /// server -> client: those files were dropped on the server; send them.
    DropHere,
}

pub fn encode(msg: &Msg) -> io::Result<Vec<u8>> {
    bincode::serialize(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn decode(bytes: &[u8]) -> io::Result<Msg> {
    bincode::deserialize(bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

pub fn write_frame<W: Write>(w: &mut W, payload: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    w.write_all(&frame)?;
    w.flush()
}

/// Human-readable reason for a dropped connection.
pub fn describe_disconnect(e: &io::Error) -> String {
    match e.kind() {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted => {
            "connection closed".into()
        }
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => "no response from the other computer".into(),
        _ => e.to_string(),
    }
}

pub fn read_frame<R: Read>(r: &mut R) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &encode(&Msg::Input(EventType::KeyPress(Key::KeyA))).unwrap()).unwrap();
        write_frame(&mut buf, &encode(&Msg::MouseDelta { dx: 3.0, dy: -2.0 }).unwrap()).unwrap();
        let mut r = &buf[..];
        assert!(matches!(decode(&read_frame(&mut r).unwrap()).unwrap(), Msg::Input(EventType::KeyPress(Key::KeyA))));
        assert!(matches!(decode(&read_frame(&mut r).unwrap()).unwrap(), Msg::MouseDelta { dx, dy } if dx == 3.0 && dy == -2.0));
    }

    /// The Android build uses copies of rdev's types; they must encode identically.
    #[cfg(feature = "desktop")]
    #[test]
    fn vendored_input_types_match_rdev_on_the_wire() {
        use crate::input as v;
        let pairs: Vec<(rdev::EventType, v::EventType)> = vec![
            (rdev::EventType::KeyPress(rdev::Key::Alt), v::EventType::KeyPress(v::Key::Alt)),
            (rdev::EventType::KeyRelease(rdev::Key::KeyZ), v::EventType::KeyRelease(v::Key::KeyZ)),
            (rdev::EventType::KeyPress(rdev::Key::Function), v::EventType::KeyPress(v::Key::Function)),
            (rdev::EventType::KeyPress(rdev::Key::KpDelete), v::EventType::KeyPress(v::Key::KpDelete)),
            (rdev::EventType::KeyPress(rdev::Key::Unknown(77)), v::EventType::KeyPress(v::Key::Unknown(77))),
            (rdev::EventType::ButtonPress(rdev::Button::Right), v::EventType::ButtonPress(v::Button::Right)),
            (rdev::EventType::ButtonRelease(rdev::Button::Unknown(9)), v::EventType::ButtonRelease(v::Button::Unknown(9))),
            (rdev::EventType::MouseMove { x: 1.5, y: -2.0 }, v::EventType::MouseMove { x: 1.5, y: -2.0 }),
            (rdev::EventType::Wheel { delta_x: -1, delta_y: 3 }, v::EventType::Wheel { delta_x: -1, delta_y: 3 }),
        ];
        for (a, b) in pairs {
            assert_eq!(bincode::serialize(&a).unwrap(), bincode::serialize(&b).unwrap(), "{a:?}");
        }
        // Every variant index decodes to the same Key/Button in both (order is the encoding).
        for i in 0u32..256 {
            let bytes = [i.to_le_bytes(), 7u32.to_le_bytes()].concat(); // index + payload for Unknown(u32)
            let a = bincode::deserialize::<rdev::Key>(&bytes).ok().map(|k| format!("{k:?}"));
            let b = bincode::deserialize::<v::Key>(&bytes).ok().map(|k| format!("{k:?}"));
            assert_eq!(a, b, "Key variant {i}");
            let bytes = [&i.to_le_bytes()[..], &[7u8]].concat();
            let a = bincode::deserialize::<rdev::Button>(&bytes).ok().map(|k| format!("{k:?}"));
            let b = bincode::deserialize::<v::Button>(&bytes).ok().map(|k| format!("{k:?}"));
            assert_eq!(a, b, "Button variant {i}");
        }
    }
}
