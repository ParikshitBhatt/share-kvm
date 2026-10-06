use anyhow::Result;
use clap::{Parser, Subcommand};
use sharekvm_core::{files, run_client, run_server, trust, ClientConfig, Edge, ServerConfig, DEFAULT_PORT};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "sharekvm", version, about = "Share one mouse and keyboard between computers")]
struct Cli {
    /// Print status events as JSON lines on stdout (used by the desktop app).
    #[arg(long, global = true)]
    status_json: bool,
    /// Where this computer's identity and remembered pairings are stored.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Where received files are saved (default: Downloads/ShareKVM).
    #[arg(long, global = true)]
    download_dir: Option<PathBuf>,
    /// Read JSON commands on stdin, one per line (used by the desktop app):
    /// {"cmd":"send_files","paths":[...]}  {"cmd":"cancel_transfer","id":N}
    #[arg(long, global = true)]
    commands: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run on the computer whose mouse and keyboard you use.
    Server {
        /// Screen edge that leads to the other computer: left, right, top, bottom.
        #[arg(long, default_value = "right")]
        edge: Edge,
        /// Pairing code new computers must enter. Remembered computers don't need it.
        #[arg(long)]
        code: String,
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Name shown on the other computer.
        #[arg(long)]
        name: Option<String>,
    },
    /// Run on the computer you want to control.
    Client {
        /// Server address, e.g. 192.168.1.20 or 192.168.1.20:24801.
        server: String,
        /// Server device ID (from `sharekvm discover`), to find it again if its address changes.
        #[arg(long)]
        server_id: Option<String>,
        /// Pairing code shown on the server. Only needed the first time.
        #[arg(long, default_value = "")]
        code: String,
        /// Name shown on the server.
        #[arg(long)]
        name: Option<String>,
        /// Swap Ctrl and Cmd/Win (useful when a Mac controls Windows or vice versa).
        #[arg(long)]
        swap_ctrl_meta: bool,
    },
    /// List ShareKVM computers on the local network.
    Discover {
        /// How long to listen, in seconds.
        #[arg(long, default_value_t = 3)]
        seconds: u64,
    },
}

fn hostname(name: Option<String>) -> String {
    name.or_else(|| std::env::var("COMPUTERNAME").ok())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .or_else(|| {
            let out = std::process::Command::new("hostname").output().ok()?;
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string()).filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "computer".into())
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    if cli.status_json {
        sharekvm_core::set_status_hook(|s| {
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            let _ = serde_json::to_writer(&mut out, s);
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        });
    }
    let data_dir = cli.data_dir.unwrap_or_else(trust::default_dir);
    files::init(cli.download_dir.unwrap_or_else(files::default_download_dir));
    if cli.commands {
        std::thread::spawn(read_commands);
    }
    match cli.cmd {
        Cmd::Server { edge, code, port, name } => {
            run_server(ServerConfig { port, code, edge, name: hostname(name), data_dir })
        }
        Cmd::Client { server, server_id, code, name, swap_ctrl_meta } => {
            let server_id = match server_id {
                Some(s) => Some(trust::from_hex(&s).ok_or_else(|| anyhow::anyhow!("--server-id must be 32 hex characters"))?),
                None => None,
            };
            run_client(ClientConfig { server, server_id, code, name: hostname(name), swap_ctrl_meta, data_dir })
        }
        Cmd::Discover { seconds } => {
            let seen = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
            let seen2 = seen.clone();
            let daemon = sharekvm_core::discovery::browse(move |ev| {
                if let sharekvm_core::discovery::Event::Found(f) = ev {
                    if seen2.lock().unwrap().insert(f.id.clone()) {
                        println!("{:<24} {:<22} id {}", f.name, f.socket_addr().map(|a| a.to_string()).unwrap_or_default(), f.id);
                    }
                }
            })?;
            std::thread::sleep(std::time::Duration::from_secs(seconds));
            let _ = daemon.shutdown();
            if seen.lock().unwrap().is_empty() {
                println!("No ShareKVM computers found. Is one sharing its mouse, on the same network?");
            }
            Ok(())
        }
    }
}

fn read_commands() {
    use std::io::BufRead;
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let Ok(cmd) = serde_json::from_str::<serde_json::Value>(&line) else {
            log::warn!("ignoring bad command: {line}");
            continue;
        };
        match cmd["cmd"].as_str() {
            Some("send_files") => {
                let paths: Vec<PathBuf> = cmd["paths"]
                    .as_array()
                    .map(|a| a.iter().filter_map(|p| p.as_str()).map(PathBuf::from).collect())
                    .unwrap_or_default();
                if !paths.is_empty() {
                    files::send_paths(paths);
                }
            }
            Some("quit") => {
                // Say goodbye on mDNS so other computers drop us from their lists now.
                sharekvm_core::discovery::stop_advertising();
                std::process::exit(0);
            }
            Some("cancel_transfer") => {
                if let Some(id) = cmd["id"].as_u64() {
                    files::cancel(id);
                }
            }
            _ => log::warn!("unknown command: {line}"),
        }
    }
    // stdin closed: the desktop app is gone (quit, crashed or killed). Don't
    // linger as an orphan still hooking the mouse and keyboard.
    log::info!("app disconnected; exiting");
    sharekvm_core::discovery::stop_advertising();
    std::process::exit(0);
}
