//! Persistent settings, stored as JSON in the OS app-config directory.

use serde::{Deserialize, Serialize};
use std::path::Path;

pub const DEFAULT_PORT: u16 = 24801;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default, rename_all = "camelCase")]
pub struct Settings {
    /// "server" (shares its mouse and keyboard) or "client" (is controlled).
    pub mode: String,
    /// Server: edge of this screen that leads to the client.
    pub edge: String,
    pub port: u16,
    /// Server: pairing code new computers must enter.
    pub code: String,
    /// Client: code typed in for the first pairing; cleared once paired.
    pub client_code: String,
    /// Client: server host or host:port.
    pub server_address: String,
    /// Client: the server's device ID, so it can be found again if its address changes.
    pub server_id: String,
    /// This computer's name, shown on the other one.
    pub name: String,
    pub swap_ctrl_meta: bool,
    /// Sharing on/off. When on, the engine runs whenever ShareKVM is running.
    pub enabled: bool,
    /// Start ShareKVM (hidden, in the menu bar / tray) when the user logs in.
    pub launch_at_login: bool,
    /// Open Finder/Explorer on files as they arrive.
    pub reveal_received: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            mode: "server".into(),
            edge: "right".into(),
            port: DEFAULT_PORT,
            code: new_code(),
            client_code: String::new(),
            server_address: String::new(),
            server_id: String::new(),
            name: hostname(),
            swap_ctrl_meta: false,
            enabled: true,
            // Dev builds don't register themselves as login items unless asked to.
            launch_at_login: !cfg!(debug_assertions),
            reveal_received: true,
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Self {
        std::fs::read(path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(path, json).map_err(|e| e.to_string())
    }

    /// True if switching from `self` to `other` needs the engine restarted.
    pub fn engine_differs(&self, other: &Settings) -> bool {
        (&self.mode, &self.edge, self.port, &self.code, &self.client_code, &self.server_address, &self.server_id, &self.name, self.swap_ctrl_meta)
            != (&other.mode, &other.edge, other.port, &other.code, &other.client_code, &other.server_address, &other.server_id, &other.name, other.swap_ctrl_meta)
    }

    pub fn validate(&self) -> Result<(), String> {
        match self.mode.as_str() {
            "server" => {
                if !["left", "right", "top", "bottom"].contains(&self.edge.as_str()) {
                    return Err("Pick which side the other computer is on.".into());
                }
            }
            "client" => {
                if self.server_address.trim().is_empty() {
                    return Err("Enter the address of the computer to connect to.".into());
                }
            }
            _ => return Err("Unknown mode.".into()),
        }
        if self.mode == "server" && self.code.trim().len() < 4 {
            return Err("The pairing code must be at least 4 characters.".into());
        }
        Ok(())
    }
}

pub fn new_code() -> String {
    use rand::Rng;
    format!("{:06}", rand::thread_rng().gen_range(0..1_000_000))
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "My computer".into())
}
