//! Status events for front-ends (the CLI prints them as JSON lines for the desktop app).

use crate::protocol::Edge;
use serde::Serialize;
use std::sync::OnceLock;

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Status {
    /// Server is accepting connections.
    Listening { port: u16, width: f64, height: f64 },
    /// Server: a client paired successfully.
    PeerConnected { name: String, addr: String, newly_paired: bool },
    /// Either side: the peer went away.
    PeerDisconnected { name: String, reason: String },
    /// Client: trying to reach the server.
    Connecting { addr: String },
    /// Client: paired with the server.
    Connected { addr: String, server_id: String, server_edge: Edge, server_name: String, newly_paired: bool },
    /// Server: someone tried a wrong pairing code. `locked_secs` > 0 if pairing is now paused.
    PairingFailed { addr: String, locked_secs: u64 },
    /// Client: the server doesn't remember this computer and no code was given.
    NeedsCode,
    /// Client: the server refused the pairing code or version.
    Rejected { reason: String },
    /// A file transfer moved forward. `direction` is "send" or "receive".
    TransferProgress { id: u64, direction: String, label: String, done: u64, total: u64 },
    /// A transfer finished. `path` is where received files were saved.
    TransferDone { id: u64, direction: String, label: String, path: Option<String> },
    TransferFailed { id: u64, direction: String, label: String, reason: String },
    /// Who has control changed. `remote`: control is away from local use.
    /// `controlling`: true if this computer drives the other, false if it is
    /// (or was) being driven by it.
    Focus { remote: bool, controlling: bool },
}

type Hook = Box<dyn Fn(&Status) + Send + Sync>;
static HOOK: OnceLock<Hook> = OnceLock::new();

/// Install a status listener. Only the first call takes effect.
pub fn set_hook(f: impl Fn(&Status) + Send + Sync + 'static) {
    let _ = HOOK.set(Box::new(f));
}

pub(crate) fn emit(s: Status) {
    if let Some(h) = HOOK.get() {
        h(&s);
    }
}
