//! Headless observer client: runs the real client session (pairing, keys,
//! protocol) and prints what a controlled device would be told to do.
//! Usage: observe <server:port> <code> [width height]
use serde_json::Value;
use sharekvm_android::{set_screen, start, Config, Output};
use std::sync::Arc;

struct Print;
impl Output for Print {
    fn cursor(&self, x: f64, y: f64) {
        println!("cursor {x:.0} {y:.0}");
    }
    fn event(&self, v: Value) {
        println!("event {v}");
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (w, h) = (a.get(3).and_then(|v| v.parse().ok()).unwrap_or(1920.0), a.get(4).and_then(|v| v.parse().ok()).unwrap_or(1080.0));
    set_screen(w, h, 1.0);
    let dir = std::env::temp_dir().join("sharekvm-observer");
    start(
        Config { data_dir: dir, server: a[1].clone(), server_id: None, code: a.get(2).cloned().unwrap_or_default(), name: "Observer".into() },
        Arc::new(Print),
    );
    std::thread::park();
}
