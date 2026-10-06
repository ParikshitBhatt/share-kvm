//! Test driver: pretends to be a desktop server so the Android app can be
//! exercised without a physical mouse. Uses the real pairing, encryption and
//! protocol. Reads commands from stdin, one per line:
//!
//!   enter <pos 0..1>        cursor arrives at the client's left edge
//!   move <dx> <dy>          relative mouse movement (desktop points)
//!   click | rclick          left / right click
//!   type <text>             types text as key presses (US layout)
//!   key <name>              up down left right enter escape backspace tab meta
//!   scroll <dy>             wheel steps (positive = up)
//!   wait <ms>
//!
//! Usage: cargo run -p sharekvm-android --example drive -- <port> <code>

use sharekvm_core::protocol::{Button, Edge, EventType, Key, Msg};
use sharekvm_core::secure::server_handshake;
use sharekvm_core::trust::{self, DeviceId};
use std::io::BufRead;
use std::net::TcpListener;
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let port: u16 = args.get(1).and_then(|p| p.parse().ok()).unwrap_or(24899);
    let code = args.get(2).cloned().unwrap_or_else(|| "482913".into());
    let listener = TcpListener::bind(("0.0.0.0", port)).expect("bind");
    eprintln!("driver: waiting on port {port}");
    // Keep accepting until a client pairs (earlier attempts may lack the code).
    let (mut sess, peer, sock) = loop {
        let (s, peer) = listener.accept().unwrap();
        match server_handshake(s.try_clone().unwrap(), s.try_clone().unwrap(), [7u8; 16] as DeviceId, &code, &|_| None) {
            Ok(sess) => break (sess, peer, s),
            Err(e) => eprintln!("driver: {peer} failed to pair: {e}"),
        }
    };
    let name = match &sess.first {
        Msg::Hello { name } => name.clone(),
        _ => "?".into(),
    };
    eprintln!("driver: paired with '{name}' from {peer}");
    sess.writer
        .send(&Msg::Welcome { server_edge: Edge::Left, server_name: "Test Desk".into(), pairing: Some(trust::new_secret().to_vec()) })
        .unwrap();
    // Like the real server: one writer thread, plus keepalive pings.
    let w = sharekvm_core::link::spawn_writer(sess.writer, sock.try_clone().unwrap());
    let ping = w.ctrl.clone();
    std::thread::spawn(move || while ping.send(Msg::Ping).is_ok() {
        std::thread::sleep(Duration::from_secs(2));
    });
    let mut r = sess.reader;
    std::thread::spawn(move || loop {
        match r.recv() {
            Ok(Msg::Ping) => {}
            Ok(m) => eprintln!("driver: {:?} got {m:?}", now()),
            Err(e) => {
                eprintln!("driver: {:?} connection ended: {e}", now());
                break;
            }
        }
    });
    let mut w = w;

    let send = |w: &mut sharekvm_core::link::Link, m: Msg| w.send(m);
    let tap = |w: &mut sharekvm_core::link::Link, k: Key| {
        w.send(Msg::Input(EventType::KeyPress(k)));
        w.send(Msg::Input(EventType::KeyRelease(k)));
    };
    for line in std::io::stdin().lock().lines().map_while(Result::ok) {
        let mut parts = line.splitn(2, ' ');
        let cmd = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("").to_string();
        match cmd {
            "enter" => send(&mut w, Msg::Enter { pos: arg.parse().unwrap_or(0.5) }),
            "move" => {
                let v: Vec<f64> = arg.split_whitespace().filter_map(|n| n.parse().ok()).collect();
                // Arrive in small steps like a real mouse.
                let steps = 10.0;
                for _ in 0..10 {
                    send(&mut w, Msg::MouseDelta { dx: v[0] / steps, dy: v[1] / steps });
                    std::thread::sleep(Duration::from_millis(8));
                }
            }
            "click" | "rclick" => {
                let b = if cmd == "click" { Button::Left } else { Button::Right };
                send(&mut w, Msg::Input(EventType::ButtonPress(b)));
                std::thread::sleep(Duration::from_millis(60));
                send(&mut w, Msg::Input(EventType::ButtonRelease(b)));
            }
            "type" => {
                for c in arg.chars() {
                    let (k, shift) = key_for(c);
                    if shift {
                        send(&mut w, Msg::Input(EventType::KeyPress(Key::ShiftLeft)));
                    }
                    tap(&mut w, k);
                    if shift {
                        send(&mut w, Msg::Input(EventType::KeyRelease(Key::ShiftLeft)));
                    }
                    std::thread::sleep(Duration::from_millis(40));
                }
            }
            "key" => tap(
                &mut w,
                match arg.as_str() {
                    "up" => Key::UpArrow,
                    "down" => Key::DownArrow,
                    "left" => Key::LeftArrow,
                    "right" => Key::RightArrow,
                    "enter" => Key::Return,
                    "escape" => Key::Escape,
                    "backspace" => Key::Backspace,
                    "tab" => Key::Tab,
                    "meta" => Key::MetaLeft,
                    other => panic!("unknown key {other}"),
                },
            ),
            "scroll" => send(&mut w, Msg::Input(EventType::Wheel { delta_x: 0, delta_y: arg.parse().unwrap_or(-3) })),
            "wait" => std::thread::sleep(Duration::from_millis(arg.parse().unwrap_or(500))),
            "" => {}
            other => eprintln!("driver: unknown command {other}"),
        }
        eprintln!("driver: {:?} did {line}", now());
    }
}

fn key_for(c: char) -> (Key, bool) {
    let lower = c.to_ascii_lowercase();
    let k = match lower {
        'a' => Key::KeyA, 'b' => Key::KeyB, 'c' => Key::KeyC, 'd' => Key::KeyD, 'e' => Key::KeyE,
        'f' => Key::KeyF, 'g' => Key::KeyG, 'h' => Key::KeyH, 'i' => Key::KeyI, 'j' => Key::KeyJ,
        'k' => Key::KeyK, 'l' => Key::KeyL, 'm' => Key::KeyM, 'n' => Key::KeyN, 'o' => Key::KeyO,
        'p' => Key::KeyP, 'q' => Key::KeyQ, 'r' => Key::KeyR, 's' => Key::KeyS, 't' => Key::KeyT,
        'u' => Key::KeyU, 'v' => Key::KeyV, 'w' => Key::KeyW, 'x' => Key::KeyX, 'y' => Key::KeyY,
        'z' => Key::KeyZ, '0' => Key::Num0, '1' => Key::Num1, '2' => Key::Num2, '3' => Key::Num3,
        '4' => Key::Num4, '5' => Key::Num5, '6' => Key::Num6, '7' => Key::Num7, '8' => Key::Num8,
        '9' => Key::Num9, ' ' => Key::Space, '.' => Key::Dot, ',' => Key::Comma, '-' => Key::Minus,
        _ => Key::Space,
    };
    (k, c.is_ascii_uppercase())
}

fn now() -> String {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    let s = t.as_secs() % 60;
    format!("{s:02}.{:03}", t.subsec_millis())
}
