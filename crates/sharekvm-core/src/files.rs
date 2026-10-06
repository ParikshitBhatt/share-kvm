//! File and folder transfer over the encrypted link.
//!
//! Sender: `FileOffer` (the full list of entries) -> `FileChunk`s in order -> `FileEnd`.
//! Receiver: writes into the download folder and answers `FileReceived`, or
//! `FileCancel` on any problem. Either side may cancel at any time.
//!
//! The receiver never trusts names from the peer: every path component is
//! sanitised and must be a plain name, so nothing can be written outside the
//! download folder, and existing files are never overwritten.

use crate::link::Link;
use crate::protocol::Msg;
use crate::status::{emit, Status};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

/// Small enough that a mouse event never waits long behind one.
pub const CHUNK: usize = 32 * 1024;
const PROGRESS_EVERY: Duration = Duration::from_millis(150);

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FileEntry {
    /// Path components relative to the transfer root, e.g. ["Photos", "a.jpg"].
    pub path: Vec<String>,
    pub size: u64,
    pub dir: bool,
}

struct Outgoing {
    label: String,
    cancel: Arc<AtomicBool>,
}

struct Incoming {
    label: String,
    entries: Vec<FileEntry>,
    /// Final location of each entry.
    targets: Vec<PathBuf>,
    /// Top-level items created, removed again if the transfer fails.
    tops: Vec<PathBuf>,
    written: Vec<u64>,
    open: Option<(u32, BufWriter<File>)>,
    done: u64,
    total: u64,
    last_emit: Instant,
}

#[derive(Default)]
struct Registry {
    link: Option<Link>,
    outgoing: HashMap<u64, Outgoing>,
    incoming: HashMap<u64, Incoming>,
}

static DOWNLOAD_DIR: OnceLock<PathBuf> = OnceLock::new();
static REG: OnceLock<Mutex<Registry>> = OnceLock::new();

fn reg() -> std::sync::MutexGuard<'static, Registry> {
    REG.get_or_init(Default::default).lock().unwrap()
}

/// Sets where received files go. Call once at startup.
pub fn init(download_dir: PathBuf) {
    let _ = DOWNLOAD_DIR.set(download_dir);
}

pub fn download_dir() -> PathBuf {
    DOWNLOAD_DIR.get().cloned().unwrap_or_else(default_download_dir)
}

pub fn default_download_dir() -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
        .map(PathBuf::from)
        .unwrap_or_else(|| ".".into());
    home.join("Downloads").join("ShareKVM")
}

/// Sets (or clears, on disconnect) the connection transfers use.
/// Clearing fails every transfer in progress.
pub fn set_link(link: Option<Link>) {
    let mut r = reg();
    let connected = link.is_some();
    r.link = link;
    if !connected {
        for (id, out) in r.outgoing.drain() {
            out.cancel.store(true, Ordering::SeqCst);
            emit(Status::TransferFailed { id, direction: "send".into(), label: out.label, reason: "disconnected".into() });
        }
        for (id, inc) in r.incoming.drain() {
            discard(inc, id, "disconnected");
        }
    }
}

pub fn is_connected() -> bool {
    reg().link.is_some()
}

/// Starts sending files/folders to the connected peer. Returns the transfer id.
pub fn send_paths(paths: Vec<PathBuf>) -> Option<u64> {
    let label = label_for(&paths);
    let id = rand::random::<u64>() >> 11; // stays exact as a JS number
    let link = {
        let mut r = reg();
        let Some(link) = r.link.clone() else {
            emit(Status::TransferFailed { id, direction: "send".into(), label, reason: "not connected".into() });
            return None;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        r.outgoing.insert(id, Outgoing { label: label.clone(), cancel: cancel.clone() });
        (link, cancel)
    };
    thread::Builder::new()
        .name("send-files".into())
        .spawn(move || {
            let (link, cancel) = link;
            if let Err(e) = send_files(id, &paths, &label, &link, &cancel) {
                let was_ours = reg().outgoing.remove(&id).is_some();
                if was_ours {
                    link.send(Msg::FileCancel { id, reason: e.to_string() });
                    emit(Status::TransferFailed { id, direction: "send".into(), label, reason: e.to_string() });
                }
            }
        })
        .ok()?;
    Some(id)
}

/// Cancels a transfer in either direction (from the local user).
pub fn cancel(id: u64) {
    let mut r = reg();
    let link = r.link.clone();
    if let Some(out) = r.outgoing.remove(&id) {
        out.cancel.store(true, Ordering::SeqCst);
        emit(Status::TransferFailed { id, direction: "send".into(), label: out.label, reason: "cancelled".into() });
    } else if let Some(inc) = r.incoming.remove(&id) {
        discard(inc, id, "cancelled");
    } else {
        return;
    }
    if let Some(l) = link {
        l.send(Msg::FileCancel { id, reason: "cancelled by the other computer".into() });
    }
}

/// Handles transfer messages from the peer. Returns false for any other message.
pub fn handle(msg: &Msg) -> bool {
    match msg {
        Msg::FileOffer { id, entries } => on_offer(*id, entries),
        Msg::FileChunk { id, index, data } => on_chunk(*id, *index, data),
        Msg::FileEnd { id } => on_end(*id),
        Msg::FileReceived { id } => {
            if let Some(out) = reg().outgoing.remove(id) {
                emit(Status::TransferDone { id: *id, direction: "send".into(), label: out.label, path: None });
            }
        }
        Msg::FileCancel { id, reason } => {
            let mut r = reg();
            if let Some(out) = r.outgoing.remove(id) {
                out.cancel.store(true, Ordering::SeqCst);
                emit(Status::TransferFailed { id: *id, direction: "send".into(), label: out.label, reason: reason.clone() });
            } else if let Some(inc) = r.incoming.remove(id) {
                drop(r);
                discard(inc, *id, reason);
            }
        }
        _ => return false,
    }
    true
}

// ---------- sending ----------

fn label_for(paths: &[PathBuf]) -> String {
    match paths {
        [one] => one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "1 item".into()),
        many => format!("{} items", many.len()),
    }
}

/// Lists every entry under `paths`, with the local file for each regular file.
fn collect(paths: &[PathBuf]) -> io::Result<Vec<(FileEntry, Option<PathBuf>)>> {
    fn walk(dir: &Path, prefix: &[String], out: &mut Vec<(FileEntry, Option<PathBuf>)>) -> io::Result<()> {
        let mut children: Vec<_> = fs::read_dir(dir)?.filter_map(Result::ok).collect();
        children.sort_by_key(|e| e.file_name());
        for child in children {
            let name = child.file_name().to_string_lossy().into_owned();
            let path = [prefix, &[name]].concat();
            let ft = child.file_type()?;
            if ft.is_dir() {
                out.push((FileEntry { path: path.clone(), size: 0, dir: true }, None));
                walk(&child.path(), &path, out)?;
            } else if ft.is_file() {
                let size = child.metadata()?.len();
                out.push((FileEntry { path, size, dir: false }, Some(child.path())));
            } // symlinks and special files are skipped
        }
        Ok(())
    }

    let mut out = Vec::new();
    for p in paths {
        let meta = fs::metadata(p)?;
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("can't send {}", p.display())))?;
        if meta.is_dir() {
            out.push((FileEntry { path: vec![name.clone()], size: 0, dir: true }, None));
            walk(p, &[name], &mut out)?;
        } else {
            out.push((FileEntry { path: vec![name], size: meta.len(), dir: false }, Some(p.clone())));
        }
    }
    Ok(out)
}

fn send_files(id: u64, paths: &[PathBuf], label: &str, link: &Link, cancel: &AtomicBool) -> io::Result<()> {
    let list = collect(paths)?;
    let total: u64 = list.iter().map(|(e, _)| e.size).sum();
    let entries: Vec<FileEntry> = list.iter().map(|(e, _)| e.clone()).collect();
    log::info!("sending '{label}': {} entries, {total} bytes", entries.len());

    let closed = || io::Error::new(io::ErrorKind::BrokenPipe, "disconnected");
    link.bulk.send(Msg::FileOffer { id, entries }).map_err(|_| closed())?;
    emit(Status::TransferProgress { id, direction: "send".into(), label: label.into(), done: 0, total });

    let (mut done, mut last_emit) = (0u64, Instant::now());
    let mut buf = vec![0u8; CHUNK];
    for (index, (_, local)) in list.iter().enumerate() {
        let Some(local) = local else { continue };
        let mut f = File::open(local)?;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Ok(()); // whoever cancelled already reported it
            }
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            link.bulk.send(Msg::FileChunk { id, index: index as u32, data: buf[..n].to_vec() }).map_err(|_| closed())?;
            done += n as u64;
            if last_emit.elapsed() >= PROGRESS_EVERY {
                last_emit = Instant::now();
                emit(Status::TransferProgress { id, direction: "send".into(), label: label.into(), done, total });
            }
        }
    }
    link.bulk.send(Msg::FileEnd { id }).map_err(|_| closed())?;
    emit(Status::TransferProgress { id, direction: "send".into(), label: label.into(), done: total, total });
    Ok(())
}

// ---------- receiving ----------

/// Makes one untrusted path component safe on every OS, or None if unusable.
fn sanitize(name: &str) -> Option<String> {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim_end_matches(['.', ' ']).trim_start().to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    // Windows reserved device names.
    let stem = cleaned.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = ["CON", "PRN", "AUX", "NUL"].contains(&stem.as_str())
        || ((stem.starts_with("COM") || stem.starts_with("LPT")) && stem.len() == 4 && stem.as_bytes()[3].is_ascii_digit());
    Some(if reserved { format!("_{cleaned}") } else { cleaned })
}

/// "report.pdf" -> "report (2).pdf" until the name is free.
fn unique(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("some free name")
}

fn plan_targets(dir: &Path, entries: &[FileEntry]) -> Result<(Vec<PathBuf>, Vec<PathBuf>), String> {
    let mut tops: HashMap<String, PathBuf> = HashMap::new();
    let mut top_order = Vec::new();
    let mut targets = Vec::with_capacity(entries.len());
    for e in entries {
        let parts: Option<Vec<String>> = e.path.iter().map(|p| sanitize(p)).collect();
        let parts = parts.filter(|p| !p.is_empty()).ok_or("invalid file name in transfer")?;
        let top = tops.entry(parts[0].clone()).or_insert_with(|| {
            let t = unique(dir, &parts[0]);
            top_order.push(t.clone());
            t
        });
        let mut target = top.clone();
        target.extend(&parts[1..]);
        // Defense in depth: everything must stay inside the download folder.
        if !target.starts_with(dir) || target.components().any(|c| matches!(c, Component::ParentDir)) {
            return Err("unsafe path in transfer".into());
        }
        targets.push(target);
    }
    Ok((targets, top_order))
}

fn on_offer(id: u64, entries: &[FileEntry]) {
    let dir = download_dir();
    let label = match entries.iter().filter(|e| e.path.len() == 1).count() {
        1 => entries[0].path[0].clone(),
        n => format!("{n} items"),
    };
    let fail = |reason: String| {
        if let Some(l) = reg().link.clone() {
            l.send(Msg::FileCancel { id, reason: reason.clone() });
        }
        emit(Status::TransferFailed { id, direction: "receive".into(), label: label.clone(), reason });
    };
    let (targets, tops) = match fs::create_dir_all(&dir).map_err(|e| e.to_string()).and_then(|_| plan_targets(&dir, entries)) {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    // Create folders and empty files up front so empty files and folders arrive too.
    for (e, t) in entries.iter().zip(&targets) {
        let made = if e.dir {
            fs::create_dir_all(t)
        } else {
            t.parent().map_or(Ok(()), fs::create_dir_all).and_then(|_| File::create(t).map(|_| ()))
        };
        if let Err(err) = made {
            for top in &tops {
                remove(top);
            }
            return fail(format!("can't write {}: {err}", t.display()));
        }
    }
    let total = entries.iter().map(|e| e.size).sum();
    log::info!("receiving '{label}' ({total} bytes) into {}", dir.display());
    emit(Status::TransferProgress { id, direction: "receive".into(), label: label.clone(), done: 0, total });
    reg().incoming.insert(
        id,
        Incoming {
            label,
            written: vec![0; entries.len()],
            entries: entries.to_vec(),
            targets,
            tops,
            open: None,
            done: 0,
            total,
            last_emit: Instant::now(),
        },
    );
}

fn on_chunk(id: u64, index: u32, data: &[u8]) {
    let mut r = reg();
    let Some(inc) = r.incoming.get_mut(&id) else { return };
    let result = (|| -> Result<(), String> {
        let i = index as usize;
        let entry = inc.entries.get(i).ok_or("chunk for unknown file")?;
        if entry.dir || inc.written[i] + data.len() as u64 > entry.size {
            return Err("received more data than announced".into());
        }
        if inc.open.as_ref().map(|(j, _)| *j) != Some(index) {
            if let Some((_, mut w)) = inc.open.take() {
                w.flush().map_err(|e| e.to_string())?;
            }
            let f = fs::OpenOptions::new().append(true).open(&inc.targets[i]).map_err(|e| e.to_string())?;
            inc.open = Some((index, BufWriter::with_capacity(256 * 1024, f)));
        }
        inc.open.as_mut().unwrap().1.write_all(data).map_err(|e| e.to_string())?;
        inc.written[i] += data.len() as u64;
        inc.done += data.len() as u64;
        Ok(())
    })();
    match result {
        Ok(()) if inc.last_emit.elapsed() >= PROGRESS_EVERY => {
            inc.last_emit = Instant::now();
            emit(Status::TransferProgress { id, direction: "receive".into(), label: inc.label.clone(), done: inc.done, total: inc.total });
        }
        Ok(()) => {}
        Err(reason) => {
            let inc = r.incoming.remove(&id).unwrap();
            let link = r.link.clone();
            drop(r);
            if let Some(l) = link {
                l.send(Msg::FileCancel { id, reason: reason.clone() });
            }
            discard(inc, id, &reason);
        }
    }
}

fn on_end(id: u64) {
    let mut r = reg();
    let Some(mut inc) = r.incoming.remove(&id) else { return };
    let link = r.link.clone();
    drop(r);
    let flushed = inc.open.take().map_or(Ok(()), |(_, mut w)| w.flush());
    let complete = inc.entries.iter().zip(&inc.written).all(|(e, w)| e.dir || *w == e.size);
    if flushed.is_err() || !complete {
        if let Some(l) = link {
            l.send(Msg::FileCancel { id, reason: "transfer incomplete".into() });
        }
        return discard(inc, id, "transfer incomplete");
    }
    if let Some(l) = link {
        l.send(Msg::FileReceived { id });
    }
    let path = inc.tops.first().map(|p| p.to_string_lossy().into_owned());
    log::info!("received '{}' -> {}", inc.label, path.as_deref().unwrap_or("?"));
    emit(Status::TransferProgress { id, direction: "receive".into(), label: inc.label.clone(), done: inc.total, total: inc.total });
    emit(Status::TransferDone { id, direction: "receive".into(), label: inc.label, path });
}

/// Removes a failed incoming transfer's partial files and reports it.
fn discard(mut inc: Incoming, id: u64, reason: &str) {
    drop(inc.open.take());
    for top in &inc.tops {
        remove(top);
    }
    emit(Status::TransferFailed { id, direction: "receive".into(), label: inc.label, reason: reason.to_string() });
}

fn remove(p: &Path) {
    let _ = if p.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_blocks_traversal_and_bad_names() {
        assert_eq!(sanitize(".."), None);
        assert_eq!(sanitize("."), None);
        assert_eq!(sanitize(""), None);
        assert_eq!(sanitize("a/b").as_deref(), Some("a_b"));
        assert_eq!(sanitize("..\\..\\x").as_deref(), Some(".._.._x"));
        assert_eq!(sanitize("CON.txt").as_deref(), Some("_CON.txt"));
        assert_eq!(sanitize("what?.txt").as_deref(), Some("what_.txt"));
        assert_eq!(sanitize("photo.jpg").as_deref(), Some("photo.jpg"));
    }

    #[test]
    fn targets_stay_inside_and_dont_overwrite() {
        let dir = std::env::temp_dir().join(format!("sharekvm-files-{}", rand::random::<u32>()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("report.pdf"), b"existing").unwrap();
        let entries = vec![
            FileEntry { path: vec!["report.pdf".into()], size: 1, dir: false },
            FileEntry { path: vec!["..".into(), "evil".into()], size: 1, dir: false },
        ];
        assert!(plan_targets(&dir, &entries).is_err(), "'..' must be rejected");

        let entries = vec![
            FileEntry { path: vec!["report.pdf".into()], size: 1, dir: false },
            FileEntry { path: vec!["Pics".into()], size: 0, dir: true },
            FileEntry { path: vec!["Pics".into(), "a/../../b.jpg".into()], size: 1, dir: false },
        ];
        let (targets, tops) = plan_targets(&dir, &entries).unwrap();
        assert_eq!(targets[0], dir.join("report (2).pdf"));
        assert_eq!(targets[2], dir.join("Pics").join("a_.._.._b.jpg"));
        assert_eq!(tops.len(), 2);
        assert!(targets.iter().all(|t| t.starts_with(&dir)));
        let _ = fs::remove_dir_all(&dir);
    }
}
