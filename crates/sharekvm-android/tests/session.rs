//! Runs the Android session against a fake desktop server over real TCP:
//! pairing with a code, then reconnecting with the saved key, then input.

use serde_json::Value;
use sharekvm_android::{set_screen, start, stop, Config, Output};
use sharekvm_core::protocol::{Button, Edge, EventType, Key, Msg};
use sharekvm_core::secure::server_handshake;
use sharekvm_core::trust::{self, DeviceId};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct Recorder {
    cursor: Mutex<Vec<(f64, f64)>>,
    events: Mutex<Vec<Value>>,
}

impl Output for Recorder {
    fn cursor(&self, x: f64, y: f64) {
        self.cursor.lock().unwrap().push((x, y));
    }
    fn event(&self, v: Value) {
        self.events.lock().unwrap().push(v);
    }
}

impl Recorder {
    fn wait_for(&self, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(v) = self.events.lock().unwrap().iter().find(|v| pred(v)) {
                return v.clone();
            }
            assert!(Instant::now() < deadline, "timed out; events: {:?}", self.events.lock().unwrap());
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn has(&self, t: &str, field: &str, value: &str) -> bool {
        self.events.lock().unwrap().iter().any(|v| v["t"] == t && v[field] == value)
    }
}

const SERVER_ID: DeviceId = [9; 16];

#[test]
fn pairs_then_reconnects_and_receives_input() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let dir = std::env::temp_dir().join(format!("sharekvm-android-test-{}", rand_suffix()));
    set_screen(1000.0, 500.0, 2.0);

    // Fake desktop server: remembers the client's key after the first pairing.
    let saved: Arc<Mutex<Option<(DeviceId, [u8; 32])>>> = Arc::default();
    let saved2 = saved.clone();
    let server = std::thread::spawn(move || {
        for round in 0..2 {
            let (s, _) = listener.accept().unwrap();
            let lookup = |id: &DeviceId| saved2.lock().unwrap().filter(|(c, _)| c == id).map(|(_, k)| k);
            let mut sess = server_handshake(s.try_clone().unwrap(), s.try_clone().unwrap(), SERVER_ID, "482913", &lookup).unwrap();
            assert!(matches!(sess.first, Msg::Hello { ref name } if name == "Pixel"));
            let pairing = (round == 0).then(|| {
                let k = trust::new_secret();
                *saved2.lock().unwrap() = Some((sess.peer_id, k));
                k.to_vec()
            });
            assert_eq!(sess.used_saved_key, round == 1, "second connection must use the saved key");
            sess.writer.send(&Msg::Welcome { server_edge: Edge::Left, server_name: "Desk".into(), pairing }).unwrap();
            if round == 0 {
                drop(s); // disconnect; the client should reconnect with its new key
                continue;
            }
            for m in [
                Msg::Enter { pos: 0.5 },
                Msg::MouseDelta { dx: 10.0, dy: 5.0 },
                Msg::Input(EventType::ButtonPress(Button::Left)),
                Msg::Input(EventType::ButtonRelease(Button::Left)),
                Msg::Input(EventType::KeyPress(Key::ShiftLeft)),
                Msg::Input(EventType::KeyPress(Key::KeyH)),
                Msg::Input(EventType::KeyRelease(Key::ShiftLeft)),
                Msg::Input(EventType::KeyPress(Key::DownArrow)),
                Msg::Clipboard("hello phone".into()),
                Msg::MouseDelta { dx: -500.0, dy: 0.0 }, // off the left edge: back to the server
            ] {
                sess.writer.send(&m).unwrap();
            }
            // The client must hand control back.
            loop {
                match sess.reader.recv().unwrap() {
                    Msg::Leave { .. } => break,
                    _ => continue,
                }
            }
        }
    });

    let rec = Arc::new(Recorder::default());
    let cfg = Config {
        data_dir: dir.clone(),
        server: format!("127.0.0.1:{port}"),
        server_id: None,
        code: "482913".into(),
        name: "Pixel".into(),
    };
    start(cfg, rec.clone());

    rec.wait_for(|v| v["t"] == "status" && v["state"] == "connected" && v["newlyPaired"] == true);
    rec.wait_for(|v| v["t"] == "status" && v["state"] == "connected" && v["newlyPaired"] == false);
    server.join().unwrap();
    stop();

    // Entered at the left edge, halfway down; moved by (10,5) points x density 2.
    let cursor = rec.cursor.lock().unwrap().clone();
    assert_eq!(cursor[0], (1.0, 250.0));
    assert_eq!(cursor[1], (21.0, 260.0));
    assert!(rec.has("press", "b", "left"));
    assert!(rec.has("release", "b", "left"));
    assert!(rec.has("text", "s", "H"));
    assert!(rec.has("key", "k", "down"));
    assert!(rec.has("clipboard", "s", "hello phone"));
    // The pairing was saved for next time.
    let paired: Value = serde_json::from_str(&sharekvm_android::paired_json(dir.to_str().unwrap())).unwrap();
    assert_eq!(paired[0]["name"], "Desk");
    let _ = std::fs::remove_dir_all(&dir);
}

fn rand_suffix() -> String {
    trust::to_hex(&trust::new_secret()[..4])
}
