//! Which drive a chain's file is on (docs/CHAINDRIVES.md): the drives to choose from, a drive
//! chosen when a chain is installed, and moving an installed chain to another drive while the
//! arcade runs (Chains & Coins, `arcade chain move`).
//!
//! On a drive, a chain lives in `<drive>/arcade-chains/<key>/chain.mdbx`; the arcade's
//! `chains/<key>.place` says so. Its own drive (the arcade's home) is the default.

use crate::node::Node;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// The folder a chosen drive keeps chains in.
pub const DIR: &str = "arcade-chains";
/// Kept free on the drive a chain is copied to, beyond the copy itself.
const KEEP_FREE: u64 = 5 << 30;

#[cfg(unix)]
fn free_bytes(p: &Path) -> Option<(u64, u64)> {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some((s.f_bavail as u64 * s.f_frsize as u64, s.f_blocks as u64 * s.f_frsize as u64))
}

/// Windows: free and total bytes of the volume that holds `p` (GetDiskFreeSpaceExW).
#[cfg(windows)]
fn free_bytes(p: &Path) -> Option<(u64, u64)> {
    let wide = win::wide(p);
    let (mut free, mut total) = (0u64, 0u64);
    let ok = unsafe { win::GetDiskFreeSpaceExW(wide.as_ptr(), &mut free, &mut total, std::ptr::null_mut()) };
    if ok == 0 { None } else { Some((free, total)) }
}

/// The Windows calls the drive list needs (kernel32), as diskfree.rs does: no extra crate.
#[cfg(windows)]
mod win {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;
    pub const DRIVE_FIXED: u32 = 3;
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetDiskFreeSpaceExW(dir: *const u16, free_to_caller: *mut u64, total: *mut u64, total_free: *mut u64) -> i32;
        pub fn GetLogicalDrives() -> u32;
        pub fn GetDriveTypeW(root: *const u16) -> u32;
        pub fn GetVolumePathNameW(file: *const u16, volume: *mut u16, len: u32) -> i32;
        pub fn GetVolumeInformationW(root: *const u16, name: *mut u16, name_len: u32, serial: *mut u32,
                                     max_component: *mut u32, flags: *mut u32, fs_name: *mut u16, fs_name_len: u32) -> i32;
    }
    pub fn wide(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }
}

/// A path as Windows shows it: canonicalize gives \\?\C:\..., the drive list has C:\.
fn plain(p: PathBuf) -> PathBuf {
    #[cfg(windows)]
    {
        if let Some(rest) = p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
            if rest.as_bytes().get(1) == Some(&b':') {
                return PathBuf::from(rest);
            }
        }
    }
    p
}

/// Mounted drives a chain can go on: real disks (/dev/...), not the system's own boot and
/// snap mounts, one entry per disk. The arcade's own drive comes first, as "home".
/// The install dialog and `arcade chain drives` poll this, so it writes to no drive: "ok" is
/// only a look at the folder's permissions; the write test comes when a drive is picked (folder).
pub fn drives(node: &Node) -> Value {
    let mut out = vec![];
    let (free, size) = free_bytes(&node.home).unwrap_or((0, 0));
    out.push(json!({"id": "home", "path": node.home.join("chains").display().to_string(), "label": "This arcade's drive",
                    "free": free, "size": size, "home": true, "ok": true}));
    for p in mount_points(node) {
        let Some((free, size)) = free_bytes(&p) else { continue };
        let dir = p.join(DIR);
        let ok = may_write(&p);
        out.push(json!({"id": p.display().to_string(), "path": dir.display().to_string(), "label": p.display().to_string(), "free": free, "size": size, "home": false, "ok": ok,
            "fix": if ok { Value::Null } else { json!(fix(&dir)) }}));
    }
    Value::Array(out)
}

/// The mount points `drives` lists (not the arcade's own drive): read from /proc/self/mounts,
/// nothing written or measured.
#[cfg(unix)]
fn mount_points(node: &Node) -> Vec<PathBuf> {
    let home_dev = dev_of(&node.home);
    let mut out = vec![];
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap_or_default();
    let mut seen = std::collections::HashSet::new();
    for line in mounts.lines() {
        let f: Vec<&str> = line.split_whitespace().collect();
        let (Some(dev), Some(mnt), Some(fs)) = (f.first(), f.get(1), f.get(2)) else { continue };
        let mnt = mnt.replace("\\040", " ");
        if !dev.starts_with("/dev/") || dev.starts_with("/dev/loop") {
            continue;
        }
        if !matches!(*fs, "ext4" | "ext3" | "xfs" | "btrfs" | "f2fs" | "zfs") {
            continue;
        }
        if mnt == "/" || ["/boot", "/snap", "/var/snap", "/efi", "/run", "/proc", "/sys"].iter().any(|p| mnt == *p || mnt.starts_with(&format!("{p}/"))) {
            continue;
        }
        let p = PathBuf::from(&mnt);
        if dev_of(&p) == home_dev || !seen.insert(dev.to_string()) {
            continue;
        }
        out.push(p);
    }
    out
}

/// Windows: the fixed drives (C:\, D:\, ...) other than the arcade's own, from GetLogicalDrives;
/// nothing written or measured.
#[cfg(windows)]
fn mount_points(node: &Node) -> Vec<PathBuf> {
    let home_dev = dev_of(&node.home);
    let mask = unsafe { win::GetLogicalDrives() };
    let mut out = vec![];
    for i in 0..26u32 {
        if mask & (1 << i) == 0 {
            continue;
        }
        let p = PathBuf::from(format!("{}:\\", (b'A' + i as u8) as char));
        if unsafe { win::GetDriveTypeW(win::wide(&p).as_ptr()) } != win::DRIVE_FIXED {
            continue;
        }
        if dev_of(&p).is_none() || dev_of(&p) == home_dev {
            continue;
        }
        out.push(p);
    }
    out
}

#[cfg(unix)]
fn fix(dir: &Path) -> String {
    format!("sudo mkdir -p {d} && sudo chown $USER: {d}", d = dir.display())
}

#[cfg(windows)]
fn fix(dir: &Path) -> String {
    format!("mkdir \"{}\"  (in a Command Prompt; or give your user write access to that drive)", dir.display())
}

#[cfg(unix)]
fn dev_of(p: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.dev())
}

/// Windows: the serial number of the volume that holds `p` (std's volume_serial_number is unstable).
#[cfg(windows)]
fn dev_of(p: &Path) -> Option<u64> {
    std::fs::metadata(p).ok()?;
    let wide = win::wide(p);
    let mut root = [0u16; 512];
    if unsafe { win::GetVolumePathNameW(wide.as_ptr(), root.as_mut_ptr(), root.len() as u32) } == 0 {
        return None;
    }
    let mut serial = 0u32;
    let ok = unsafe {
        win::GetVolumeInformationW(root.as_ptr(), std::ptr::null_mut(), 0, &mut serial, std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut(), 0)
    };
    if ok == 0 { None } else { Some(serial as u64) }
}

/// Whether the arcade looks able to keep chains on a drive, without writing to it: its
/// arcade-chains folder is writable, or (none yet) the drive's top folder is, to make it in.
fn may_write(mnt: &Path) -> bool {
    let d = mnt.join(DIR);
    let at = if d.exists() { d } else { mnt.to_path_buf() };
    #[cfg(unix)]
    {
        let Ok(c) = std::ffi::CString::new(at.as_os_str().as_encoded_bytes()) else { return false };
        at.is_dir() && unsafe { libc::access(c.as_ptr(), libc::W_OK | libc::X_OK) } == 0
    }
    // Windows: a folder that isn't marked read-only; the real write test comes when it is picked (writable).
    #[cfg(windows)]
    {
        at.is_dir() && std::fs::metadata(&at).is_ok_and(|m| !m.permissions().readonly())
    }
}

/// Whether the arcade can keep chains on a drive (its arcade-chains folder exists and is
/// writable, or can be made): a real write, made only for a drive the user picked.
fn writable(mnt: &Path) -> bool {
    let d = mnt.join(DIR);
    if !d.is_dir() && std::fs::create_dir(&d).is_err() {
        return false;
    }
    let t = d.join(".arcade-write-test");
    let ok = std::fs::write(&t, b"").is_ok();
    let _ = std::fs::remove_file(&t);
    ok
}

/// The chain folder on drive `id` ("home" or a mount point from `drives`): None for home.
fn folder(node: &Node, id: &str, key: &str) -> Result<Option<PathBuf>, String> {
    if id.is_empty() || id == "home" {
        return Ok(None);
    }
    let list = drives(node);
    let d = list.as_array().unwrap().iter().find(|d| d["id"] == id).ok_or_else(|| format!("{id} is not a drive this arcade can use (see  arcade chain drives)"))?;
    let dir = PathBuf::from(d["path"].as_str().unwrap());
    if d["ok"] != true || !writable(Path::new(id)) {
        return Err(format!("the arcade can't write to {id}; run: {}", fix(&dir)));
    }
    Ok(Some(dir.join(key)))
}

/// chains.add with a drive (empty or "home": the arcade's own): the chain's place file is
/// written, or cleared, before `add` runs, and cleared again if `add` fails. A place left behind
/// by a failed add would otherwise win over a retry with drive=home, and the chain would start on
/// a drive that can be unplugged.
pub fn add(node: &Node, key: &str, drive: &str, add: impl FnOnce() -> Result<Value, String>) -> Result<Value, String> {
    let fresh = crate::chains::find(key).filter(|p| p.chain_no != node.params.chain_no && !node.extra.lock().unwrap().contains_key(p.key));
    let Some(p) = fresh else { return add() };
    let dir = folder(node, drive, p.key)?;
    install_at(node, p.chain_no, dir.as_deref(), add)
}

fn install_at(node: &Node, c: u16, dir: Option<&Path>, add: impl FnOnce() -> Result<Value, String>) -> Result<Value, String> {
    // Home clears only a place file there is: a just-removed chain's file still being deleted
    // at home must not stop the add.
    if dir.is_some() || node.chain.placed_elsewhere(c) {
        node.chain.set_place(c, dir).map_err(|e| e.to_string())?;
    }
    let r = add();
    if r.is_err() {
        let _ = node.chain.set_place(c, None);
    }
    r
}

/// Where chain `c`'s file is: its folder, the drive's id, the file's size.
pub fn place(node: &Node, c: u16) -> Value {
    let dir = node.chain.own_dir(c);
    let real = plain(std::fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone()));
    let size = std::fs::metadata(dir.join("chain.mdbx")).map(|m| m.len()).unwrap_or(0);
    let home = dev_of(&real).is_some() && dev_of(&real) == dev_of(&node.home);
    // chains.status polls this: the drive is found in the mount list, no drive is measured or written to.
    let id = if home { "home".to_string() } else {
        mount_points(node).into_iter().filter(|m| real.starts_with(m)).max_by_key(|m| m.as_os_str().len()).map(|m| m.display().to_string()).unwrap_or_default()
    };
    json!({"dir": real.display().to_string(), "drive": id, "bytes": size, "free": free_bytes(&real).map(|f| f.0).unwrap_or(0)})
}

#[derive(Clone)]
struct Move {
    to: String,
    phase: &'static str,
    part: Option<PathBuf>,
    total: u64,
    error: Option<String>,
    started: u64,
    finished: Option<u64>,
}

static MOVES: Mutex<BTreeMap<String, Move>> = Mutex::new(BTreeMap::new());

/// A move's progress (Null: none since the arcade started).
pub fn status(key: &str) -> Value {
    let Some(m) = MOVES.lock().unwrap().get(key).cloned() else { return Value::Null };
    let done = if m.finished.is_some() && m.error.is_none() { m.total } else {
        m.part.as_ref().and_then(|p| std::fs::metadata(p).ok()).map(|x| x.len()).unwrap_or(0)
    };
    json!({"to": m.to, "phase": m.phase, "bytes": m.total, "bytes_done": done.min(m.total),
           "pct": if m.total == 0 { 0.0 } else { (done as f64 * 100.0 / m.total as f64).min(if m.finished.is_some() { 100.0 } else { 99.0 }) },
           "error": m.error, "started": m.started, "finished": m.finished})
}

pub fn moving(key: &str) -> bool {
    MOVES.lock().unwrap().get(key).is_some_and(|m| m.finished.is_none())
}

/// Refuse a change to chain `key` (remove, pause or resume, an index, a copy or refill) while it
/// moves: the move holds its sync and starts it again when the copy is done.
pub fn not_moving(key: &str) -> Result<(), String> {
    if moving(key) {
        return Err(format!("{key} is moving to another drive; try again when that is done (chains.status shows how far it is)"));
    }
    Ok(())
}

/// Record a move of `key` unless one runs: checked and recorded under one lock, so two quick
/// chains.move calls can't both start one.
fn begin(key: &str, name: &str, m: Move) -> Result<(), String> {
    let mut moves = MOVES.lock().unwrap();
    if moves.get(key).is_some_and(|m| m.finished.is_none()) {
        return Err(format!("{name} is already moving"));
    }
    moves.insert(key.into(), m);
    Ok(())
}

fn set_phase(key: &str, phase: &'static str) {
    if let Some(m) = MOVES.lock().unwrap().get_mut(key) {
        m.phase = phase;
    }
}

/// Move installed chain `key` to drive `id` ("home" for the arcade's own drive), in the
/// background: its sync stops, the file is copied, the chain switches to the copy and syncs on.
pub fn start(node: &Arc<Node>, key: &str, id: &str) -> Result<Value, String> {
    let p = crate::chains::find(key).ok_or_else(|| format!("unknown chain {key}"))?;
    let key = p.key;
    let c = p.chain_no;
    if c == node.params.chain_no {
        return Err(format!("{} is this arcade's home chain; it stays on the arcade's drive for now", p.name));
    }
    let run = node.extra.lock().unwrap().get(key).cloned().ok_or_else(|| format!("{} is not installed", p.name))?;
    if moving(key) {
        return Err(format!("{} is already moving", p.name));
    }
    if crate::coreboot::held(key) {
        return Err(format!("{} is taking in a copy; move it when that is done", p.name));
    }
    if crate::refill::status(Some(key)).get("finished").is_some_and(|f| f.is_null()) {
        return Err(format!("{} is refilling blocks; move it when that is done", p.name));
    }
    let to = folder(node, id, key)?;
    let src = node.chain.own_path(c);
    let total = std::fs::metadata(&src).map(|m| m.len()).map_err(|_| format!("{} has no file of its own to move", p.name))?;
    let dest_drive = to.clone().unwrap_or_else(|| node.home.clone());
    let probe = dest_drive.ancestors().find(|a| a.exists()).unwrap_or(Path::new("/")).to_path_buf();
    let free = free_bytes(&probe).map(|f| f.0).unwrap_or(0);
    if free < total + KEEP_FREE {
        return Err(format!("not enough room: {} needs {:.1} GB and {} has {:.1} GB free", p.name, (total + KEEP_FREE) as f64 / 1e9, if id.is_empty() { "home" } else { id }, free as f64 / 1e9));
    }
    let label = if to.is_none() { "this arcade's drive".to_string() } else { id.to_string() };
    begin(key, p.name, Move { to: label.clone(), phase: "stopping", part: None, total, error: None, started: arcade_app::now(), finished: None })?;
    let n = node.clone();
    let spawned = std::thread::Builder::new().name(format!("move {key}")).spawn(move || {
        let was_paused = run.paused.load(Ordering::Relaxed);
        // Its sync and address build stop; it stays in the list (and in the saved settings).
        run.paused.store(true, Ordering::Relaxed);
        run.stop.store(true, Ordering::Relaxed);
        crate::addrsvc::stop(c);
        let took = crate::chains::wait_stopped(c);
        while crate::addrsvc::building(c) {
            std::thread::sleep(Duration::from_millis(200));
        }
        set_phase(key, "copying");
        let r = match took {
            None => Err("the chain's sync didn't stop".to_string()),
            Some(_one) => n.chain.relocate(c, to.as_deref(), |part| {
                if let Some(m) = MOVES.lock().unwrap().get_mut(key) {
                    m.part = Some(part.to_path_buf());
                }
            }).map_err(|e| e.to_string()),
        };
        let restarted = crate::chains::relaunch(&n, key, was_paused);
        let mut moves = MOVES.lock().unwrap();
        let m = moves.get_mut(key).unwrap();
        m.finished = Some(arcade_app::now());
        match r {
            Ok(dir) => {
                m.phase = "done";
                drop(moves);
                let _ = n.app.event("chains", &format!("moved {} to {} ({:.1} GB)", run.params.name, dir.display(), total as f64 / 1e9));
            }
            Err(e) => {
                m.phase = "failed";
                m.error = Some(e.clone());
                drop(moves);
                let _ = n.app.event("chains", &format!("could not move {}: {e} (it stays where it was)", run.params.name));
            }
        }
        if let Err(e) = restarted {
            let _ = n.app.event("chains", &format!("could not restart {} after its move: {e}", run.params.name));
        }
    });
    if let Err(e) = spawned {
        // No move runs after all: don't leave one recorded as running (it would refuse changes).
        if let Some(m) = MOVES.lock().unwrap().get_mut(key) {
            (m.phase, m.error, m.finished) = ("failed", Some(e.to_string()), Some(arcade_app::now()));
        }
        return Err(e.to_string());
    }
    Ok(json!({"moving": key, "to": label, "bytes": total}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sitesvc::tests::shared;

    fn running(to: &str) -> Move {
        Move { to: to.into(), phase: "copying", part: None, total: 1, error: None, started: 0, finished: None }
    }

    fn finish(key: &str) {
        if let Some(m) = MOVES.lock().unwrap().get_mut(key) {
            m.finished = Some(1);
        }
    }

    /// Martin, PR #153: an add to /mnt/usb that failed left jkc.place behind, and a retry with
    /// drive=home still created the chain on the USB drive.
    #[test]
    fn a_failed_add_leaves_no_place_and_a_retry_at_home_starts_at_home() {
        let node = &shared().node;
        let c = crate::chains::find("jkc").unwrap().chain_no;
        let usb = tempfile::tempdir().unwrap();
        let on_usb = usb.path().join(DIR).join("jkc");
        let at_home = node.chain.own_dir(c);
        assert!(install_at(node, c, Some(&on_usb), || Err("no peers".into())).is_err());
        assert!(!node.chain.placed_elsewhere(c), "a failed add left its place behind");
        assert_eq!(node.chain.own_dir(c), at_home);
        // A place file left by an older arcade: drive=home clears it.
        node.chain.set_place(c, Some(&on_usb)).unwrap();
        let mut ran = false;
        add(node, "jkc", "home", || {
            ran = true;
            Ok(json!({}))
        })
        .unwrap();
        assert!(ran);
        assert!(!node.chain.placed_elsewhere(c));
        assert_eq!(node.chain.own_dir(c), at_home);
        // An add that goes ahead keeps the drive chosen for it.
        install_at(node, c, Some(&on_usb), || Ok(json!({}))).unwrap();
        assert_eq!(node.chain.own_dir(c), on_usb);
        node.chain.set_place(c, None).unwrap();
    }

    /// Martin, PR #153: listing the drives made arcade-chains on every mounted drive.
    #[test]
    fn looking_at_a_drive_writes_nothing() {
        let d = tempfile::tempdir().unwrap();
        assert!(may_write(d.path()));
        assert!(!d.path().join(DIR).exists(), "a look made the folder");
        // Picking the drive makes it, and tests it.
        assert!(writable(d.path()));
        assert!(d.path().join(DIR).is_dir());
        assert!(may_write(d.path()));
        let f = tempfile::tempdir().unwrap();
        std::fs::write(f.path().join(DIR), b"").unwrap();
        assert!(!may_write(f.path()), "arcade-chains is a file, not a folder");
        let _ = drives(&shared().node);
        let _ = place(&shared().node, crate::chains::find("jkc").unwrap().chain_no);
    }

    /// Martin, PR #153: two quick chains.move calls both started a move.
    #[test]
    fn only_one_of_two_quick_moves_starts() {
        let gate = Arc::new(std::sync::Barrier::new(8));
        let started: Vec<bool> = (0..8)
            .map(|_| {
                let g = gate.clone();
                std::thread::spawn(move || {
                    g.wait();
                    begin("flop", "Flopcoin", running("/mnt/usb")).is_ok()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| t.join().unwrap())
            .collect();
        assert_eq!(started.iter().filter(|s| **s).count(), 1);
        assert!(begin("flop", "Flopcoin", running("home")).unwrap_err().contains("already moving"));
        finish("flop");
        assert!(begin("flop", "Flopcoin", running("home")).is_ok(), "a finished move doesn't block the next");
        finish("flop");
    }

    /// Martin, PR #153: nothing stopped a remove, pause or index change during a copy.
    #[test]
    fn changes_wait_while_a_chain_moves() {
        let node = &shared().node;
        begin("kot", "Kotia", running("/mnt/usb")).unwrap();
        let refused = |r: Result<Value, String>| r.unwrap_err().contains("moving to another drive");
        assert!(refused(crate::chains::set_paused(node, "kot", true)));
        assert!(refused(crate::chains::set_paused(node, "KOT", false)));
        assert!(refused(crate::chains::remove(node, "kot")));
        assert!(refused(crate::chains::add_index(node, "kot", "ord")));
        assert!(refused(crate::chains::remove_index(node, "kot", "ord")));
        assert!(refused(crate::coreboot::start(node, "kot", "coins")));
        finish("kot");
        // Done moving: the usual answer (it isn't installed on this test arcade).
        assert!(crate::chains::set_paused(node, "kot", true).unwrap_err().contains("not installed"));
    }
}
