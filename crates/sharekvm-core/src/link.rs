//! The outgoing side of a connection: one writer thread fed by two queues.
//!
//! Input, clipboard and control messages go on `ctrl` and always go first.
//! File chunks go on `bulk`, a tiny bounded queue, so a large transfer never
//! makes the mouse lag by more than about one chunk.

use crate::protocol::Msg;
use crate::secure::SecureWriter;
use crossbeam_channel::{bounded, select, unbounded, Sender, TryRecvError};
use std::io::Write;
use std::net::{Shutdown, TcpStream};
use std::thread;

/// Small kernel send buffer: bytes queued there sit in front of mouse events.
const SEND_BUFFER: usize = 64 * 1024;

#[derive(Clone)]
pub struct Link {
    pub ctrl: Sender<Msg>,
    pub bulk: Sender<Msg>,
}

impl Link {
    pub fn send(&self, msg: Msg) {
        let _ = self.ctrl.send(msg);
    }
}

pub fn tune_socket(stream: &TcpStream) {
    let _ = socket2::SockRef::from(stream).set_send_buffer_size(SEND_BUFFER);
}

/// Spawns the writer thread. It exits (and shuts the socket) when every `ctrl`
/// sender is gone or a write fails.
pub fn spawn_writer<W: Write + Send + 'static>(mut writer: SecureWriter<W>, socket: TcpStream) -> Link {
    let (ctrl, ctrl_rx) = unbounded::<Msg>();
    let (bulk, bulk_rx) = bounded::<Msg>(2);
    // Keep one bulk sender alive inside the thread so `bulk_rx` never reads as closed.
    let bulk_keepalive = bulk.clone();
    thread::Builder::new()
        .name("writer".into())
        .spawn(move || {
            let _keep = bulk_keepalive;
            loop {
                let msg = match ctrl_rx.try_recv() {
                    Ok(m) => m,
                    Err(TryRecvError::Disconnected) => break,
                    Err(TryRecvError::Empty) => select! {
                        recv(ctrl_rx) -> m => match m { Ok(m) => m, Err(_) => break },
                        recv(bulk_rx) -> m => match m { Ok(m) => m, Err(_) => continue },
                    },
                };
                if writer.send(&msg).is_err() {
                    break;
                }
            }
            let _ = socket.shutdown(Shutdown::Both);
        })
        .expect("spawn writer");
    Link { ctrl, bulk }
}
