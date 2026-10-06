//! Runs the `sharekvm` engine as a child process and turns its JSON status
//! lines into a snapshot the UI can render.
//!
//! The engine is a separate process because on macOS its input hook must own
//! the process's main thread, which Tauri needs for the window.

use crate::settings::Settings;
use serde::Serialize;
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use tauri::{AppHandle, Emitter};

const MAX_LOG: usize = 200;

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub running: bool,
    /// "stopped" | "waiting" | "connecting" | "connected" | "active" | "error"
    pub state: String,
    pub mode: String,
    pub peer: Option<String>,
    pub message: Option<String>,
    pub log: Vec<String>,
    /// While `state` is "active": true if this computer drives the other, false if it's being driven.
    pub controlling: bool,
    /// The user has to do something (enter a code, fix a setting); don't auto-retry.
    pub needs_user: bool,
    /// Which engine process this snapshot belongs to; stale threads check it.
    #[serde(skip)]
    generation: u64,
}

#[derive(Default)]
pub struct Engine {
    child: Option<Child>,
    /// Commands to the engine (send files, cancel), one JSON object per line.
    stdin: Option<ChildStdin>,
    generation: u64,
    snap: Arc<Mutex<Snapshot>>,
}

impl Engine {
    pub fn snapshot(&self) -> Snapshot {
        self.snap.lock().unwrap().clone()
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Shows a problem that stops the engine from starting (e.g. a missing address).
    pub fn fail(&mut self, app: &AppHandle, mode: &str, message: String) {
        self.stop(app);
        let mut s = self.snap.lock().unwrap();
        let log = std::mem::take(&mut s.log);
        *s = Snapshot { state: "error".into(), mode: mode.into(), message: Some(message), needs_user: true, log, ..Default::default() };
        emit(app, &s);
    }

    pub fn start(&mut self, app: &AppHandle, s: &Settings, data_dir: &Path) -> Result<(), String> {
        s.validate()?;
        self.stop(app);

        let mut cmd = Command::new(engine_path()?);
        cmd.arg("--status-json").arg("--commands").arg("--data-dir").arg(data_dir);
        if s.mode == "server" {
            cmd.args(["server", "--edge", &s.edge, "--code", s.code.trim(), "--port", &s.port.to_string(), "--name", &s.name]);
        } else {
            cmd.args(["client", s.server_address.trim(), "--code", s.client_code.trim(), "--name", &s.name]);
            if s.server_id.len() == 32 {
                cmd.args(["--server-id", &s.server_id]);
            }
            if s.swap_ctrl_meta {
                cmd.arg("--swap-ctrl-meta");
            }
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }
        let mut child = cmd.spawn().map_err(|e| format!("Could not start the engine: {e}"))?;

        self.generation += 1;
        let generation = self.generation;
        {
            let mut snap = self.snap.lock().unwrap();
            *snap = Snapshot {
                running: true,
                state: if s.mode == "server" { "waiting" } else { "connecting" }.into(),
                mode: s.mode.clone(),
                generation,
                ..Default::default()
            };
            emit(app, &snap);
        }

        let stderr = child.stderr.take().expect("piped stderr");
        let snap = self.snap.clone();
        let app2 = app.clone();
        thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let mut s = snap.lock().unwrap();
                if s.generation != generation {
                    break;
                }
                s.log.push(line.clone());
                if s.log.len() > MAX_LOG {
                    s.log.remove(0);
                }
                let _ = app2.emit("engine-log", line);
            }
        });

        let stdout = child.stdout.take().expect("piped stdout");
        let snap = self.snap.clone();
        let app2 = app.clone();
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                let mut s = snap.lock().unwrap();
                if s.generation != generation {
                    return;
                }
                let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
                if kind.starts_with("transfer_") {
                    drop(s);
                    let _ = app2.emit("transfer", v);
                    continue;
                }
                apply_event(&mut s, &v);
                emit(&app2, &s);
                if v.get("type").and_then(Value::as_str) == Some("connected") {
                    let _ = app2.emit("client-connected", v.clone());
                }
                if v.get("newly_paired").and_then(Value::as_bool) == Some(true) {
                    let _ = app2.emit("paired-changed", ());
                    if v.get("type").and_then(Value::as_str) == Some("connected") {
                        let _ = app2.emit("client-paired", ());
                    }
                }
            }
            // stdout closed: the engine exited (or was stopped).
            let mut s = snap.lock().unwrap();
            if s.generation == generation && s.running {
                let _ = app2.emit("engine-exited", serde_json::json!({ "generation": generation, "needsUser": s.needs_user }));
                s.running = false;
                s.peer = None;
                // Keep a specific reason (e.g. a refused pairing code) if we already have one.
                if s.state != "error" || s.message.is_none() {
                    s.message = Some(exit_reason(&s.log));
                }
                s.state = "error".into();
                emit(&app2, &s);
            }
        });

        self.stdin = child.stdin.take();
        self.child = Some(child);
        Ok(())
    }

    pub fn command(&mut self, cmd: &Value) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or("ShareKVM isn't running.")?;
        writeln!(stdin, "{cmd}").and_then(|_| stdin.flush()).map_err(|e| format!("Engine not responding: {e}"))
    }

    pub fn stop(&mut self, app: &AppHandle) {
        if let Some(mut stdin) = self.stdin.take() {
            let _ = writeln!(stdin, "{}", serde_json::json!({ "cmd": "quit" }));
            let _ = stdin.flush();
        }
        if let Some(mut child) = self.child.take() {
            // Give it a moment to say goodbye on the network, then make sure it's gone.
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(800);
            while std::time::Instant::now() < deadline && matches!(child.try_wait(), Ok(None)) {
                thread::sleep(std::time::Duration::from_millis(20));
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        let mut s = self.snap.lock().unwrap();
        if s.running || s.state != "stopped" {
            s.running = false;
            s.state = "stopped".into();
            s.peer = None;
            s.message = None;
            s.needs_user = false;
            emit(app, &s);
        }
    }
}

fn emit(app: &AppHandle, s: &Snapshot) {
    let _ = app.emit("engine-status", s.clone());
}

fn apply_event(s: &mut Snapshot, v: &Value) {
    let str_field = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    match v.get("type").and_then(Value::as_str) {
        Some("listening") => {
            s.state = "waiting".into();
            s.message = None;
        }
        Some("connecting") => {
            if s.state != "connecting" {
                s.state = "connecting".into();
            }
        }
        Some("peer_connected") => {
            s.state = "connected".into();
            s.peer = str_field("name");
            s.message = None;
        }
        Some("connected") => {
            s.state = "connected".into();
            s.peer = str_field("server_name").or_else(|| str_field("addr"));
            s.message = None;
        }
        Some("needs_code") => {
            s.state = "error".into();
            s.needs_user = true;
            s.message = Some(
                "The other computer doesn't remember this one. Enter the pairing code shown on it."
                    .into(),
            );
        }
        Some("peer_disconnected") => {
            s.state = if s.mode == "server" { "waiting" } else { "connecting" }.into();
            s.peer = None;
            s.message = str_field("reason").map(|r| format!("Disconnected: {r}"));
        }
        Some("pairing_failed") => {
            let who = str_field("addr").unwrap_or_default();
            let who = who.rsplit_once(':').map_or(who.as_str(), |(ip, _)| ip).to_string();
            let locked = v.get("locked_secs").and_then(Value::as_u64).unwrap_or(0);
            s.message = Some(if locked > 0 {
                format!("Several wrong pairing codes were tried from {who}. New connections are paused for {locked} seconds. If that wasn't you, generate a new code.")
            } else {
                format!("Someone at {who} tried to connect with the wrong pairing code.")
            });
        }
        Some("rejected") => {
            s.state = "error".into();
            s.needs_user = true;
            s.message = str_field("reason").map(|r| format!("Connection refused: {r}"));
        }
        Some("focus") => {
            let remote = v.get("remote").and_then(Value::as_bool).unwrap_or(false);
            s.controlling = v.get("controlling").and_then(Value::as_bool).unwrap_or(false);
            s.state = if remote { "active" } else { "connected" }.into();
        }
        _ => {}
    }
}

/// Best human-readable reason from the engine's last log lines.
fn exit_reason(log: &[String]) -> String {
    log.iter()
        .rev()
        .find(|l| l.contains("Error") || l.contains("ERROR") || l.contains("error"))
        .map(|l| l.trim_start_matches("Error: ").to_string())
        .unwrap_or_else(|| "The engine stopped unexpectedly.".into())
}

/// The engine binary sits next to this app's executable, both in dev
/// (`target/debug`) and in a bundle (Tauri `externalBin`).
fn engine_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = exe.parent().ok_or("no exe dir")?;
    let name = if cfg!(windows) { "sharekvm.exe" } else { "sharekvm" };
    let p = dir.join(name);
    if p.exists() {
        Ok(p)
    } else {
        Err(format!("Engine not found at {}. Build it with `cargo build -p sharekvm`.", p.display()))
    }
}
