//! Text clipboard sync by polling. `last` holds the most recent value seen or
//! applied, so a value received from the peer is not echoed back to it.

use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

const POLL: Duration = Duration::from_millis(400);
const MAX_LEN: usize = 4 * 1024 * 1024;

pub type Last = Arc<Mutex<Option<String>>>;

fn read() -> Option<String> {
    arboard::Clipboard::new().ok()?.get_text().ok()
}

pub fn new_last() -> Last {
    Arc::new(Mutex::new(read()))
}

/// Polls the clipboard and calls `on_change` with new text.
pub fn spawn_watcher(last: Last, on_change: impl Fn(String) + Send + 'static) {
    thread::Builder::new()
        .name("clipboard".into())
        .spawn(move || loop {
            thread::sleep(POLL);
            let Some(text) = read() else { continue };
            if text.len() > MAX_LEN {
                continue;
            }
            let mut l = last.lock().unwrap();
            if l.as_deref() != Some(text.as_str()) {
                *l = Some(text.clone());
                drop(l);
                on_change(text);
            }
        })
        .expect("spawn clipboard thread");
}

/// Sets the local clipboard to text received from the peer.
pub fn apply(last: &Last, text: String) {
    let mut l = last.lock().unwrap();
    if l.as_deref() == Some(text.as_str()) {
        return;
    }
    match arboard::Clipboard::new().and_then(|mut c| c.set_text(text.clone())) {
        Ok(()) => *l = Some(text),
        Err(e) => log::warn!("clipboard set failed: {e}"),
    }
}
