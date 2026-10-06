//! ShareKVM core: share one mouse and keyboard across machines on a LAN.
//!
//! The `desktop` feature (default) adds the macOS/Windows engine: input
//! hooks, injection, clipboard and mDNS. Without it, this is the portable
//! core the Android app builds on.

pub mod files;
pub mod input;
pub mod link;
pub mod protocol;
pub mod screens;
pub mod secure;
pub mod status;
pub mod trust;

#[cfg(feature = "desktop")]
pub mod client;
#[cfg(feature = "desktop")]
pub mod clipboard;
#[cfg(feature = "desktop")]
pub mod discovery;
#[cfg(all(feature = "desktop", target_os = "macos"))]
mod mac_hook;
#[cfg(all(feature = "desktop", target_os = "macos"))]
mod mac_keys;
#[cfg(feature = "desktop")]
pub mod platform;
#[cfg(feature = "desktop")]
pub mod server;

#[cfg(feature = "desktop")]
pub use client::{run_client, ClientConfig};
pub use protocol::{Edge, DEFAULT_PORT};
#[cfg(feature = "desktop")]
pub use server::{run_server, ServerConfig};
pub use status::{set_hook as set_status_hook, Status};
