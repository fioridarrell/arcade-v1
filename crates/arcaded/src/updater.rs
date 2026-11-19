//! Updates (the Options tab): this arcade's programs replaced by the newest release, when the
//! operator turns it on (off by default). **Releases come from the Arcade V1 network itself**
//! (Martin 2026-11-14: "Arcade V1 should be where arcade V1 is pulling all future updates"): every
//! arcade keeps the signed releases of `@arcade/arcade-v1` (crate::arccode: records signed by
//! Strawberry's release key, every file checked against its record), so a new release reaches this
//! arcade from the arcades it is linked to, with no website in between. Every few minutes it takes
//! the newest release (vX.Y.Z) whose program archive for this machine is held here, checks it again,
//! makes sure the new arcaded says it is that version, swaps the programs where they are installed
//! (the old ones kept as `<name>.prev`), and restarts: the same clean stop as `systemctl stop` (coin
//! caches written), then this process starts the new arcaded in its place with the same options.
//!
//! "Check now" and "Update now" do the same by hand. A local build (no release version) never updates itself.
//!
//! **Windows** (the windows branch): the same signed releases, the same checks. Its program archive is
//! `program-windows-x86_64` (the release's `arcade-v1-windows-x86_64.zip`), carried either by the release itself
//! (`vX.Y.Z`) or by a Windows release of its own (`vX.Y.Z-windows`, the windows branch's commit); a Windows build's
//! version `vX.Y.Z-windows` is compared as `vX.Y.Z`. A running program can't be replaced on Windows but can be
//! renamed, so each one is renamed to `<name>.prev` and the new one put in its place (all put back if any fails); the
//! MPIR DLLs beside arcaded are swapped the same way. After the clean stop the new arcaded is started in its own
//! process with the same options (the old one leaves once it has started; the new one waits for the old one's
//! `arcaded.lock`). An open window keeps running the old Arcade V1.0 until it is reopened.

use crate::node::Node;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

const SETTING: &[u8] = b"auto_update";
/// How often an arcade with updates on looks for a new release (the first look ten minutes after
/// it starts, each a little later than the last so arcades don't all ask at once).
/// (Looking is reading this arcade's own code store, so it can be often.)
const EVERY_SECS: u64 = 600;
const FIRST_SECS: u64 = 120;
/// The programs a release carries; each one present beside arcaded is replaced.
const PROGRAMS: &[&str] = &["arcaded", "arcade", "arcade-desktop"];

/// This build's release version (None for a local build).
/// The longest an installed update waits for the next ARCADE block before it restarts.
const RESTART_WAIT: std::time::Duration = std::time::Duration::from_secs(600);
/// The longest an installed update waits for the draw seed's proof (crate::arcsvc::seed_running): seed proofs took
/// 15 to 56 minutes on the testnet's writers (2026-11-15/16).
const SEED_WAIT: std::time::Duration = std::time::Duration::from_secs(70 * 60);

pub fn version() -> Option<&'static str> {
    option_env!("ARCADE_VERSION")
}

fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let mut it = v.strip_prefix('v')?.split('.');
    let t = (it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
    it.next().is_none().then_some(t)
}

/// A Windows build's version is its release's with "-windows" after it (the windows branch's tags).
const WINDOWS_SUFFIX: &str = "-windows";

/// A release version for comparing: `vX.Y.Z`, and on Windows `vX.Y.Z-windows` too (as `vX.Y.Z`).
fn release_version(v: &str, windows: bool) -> Option<(u64, u64, u64)> {
    parse(v).or_else(|| if windows { parse(v.strip_suffix(WINDOWS_SUFFIX)?) } else { None })
}

fn rel(v: &str) -> Option<(u64, u64, u64)> {
    release_version(v, cfg!(windows))
}

/// Whether `arcaded --version`'s output names release `tag` (on Windows, `vX.Y.Z` and `vX.Y.Z-windows` name the same
/// release).
fn says_version(said: &str, tag: &str, windows: bool) -> bool {
    let base = |v: &str| v.strip_suffix(WINDOWS_SUFFIX).unwrap_or(v).to_string();
    said.split_whitespace().any(|w| w == tag || (windows && parse(&base(w)).is_some() && base(w) == base(tag)))
}

/// The release archive built for a system (the folder inside it is named the same, without .tar.gz / .zip).
fn asset_for(os: &str, arch: &str) -> Option<String> {
    match (os, arch) {
        ("linux", a @ ("x86_64" | "aarch64")) => Some(format!("arcade-v1-linux-{a}.tar.gz")),
        ("windows", "x86_64") => Some("arcade-v1-windows-x86_64.zip".into()),
        _ => None,
    }
}

/// The release archive built for this machine.
fn asset_name() -> Option<String> {
    asset_for(std::env::consts::OS, std::env::consts::ARCH)
}

/// The folder inside a release archive.
fn archive_folder(asset: &str) -> &str {
    asset.trim_end_matches(".tar.gz").trim_end_matches(".zip")
}

/// The release file that holds an archive (`program-<platform>`, crate::arccode).
fn program_file_for(asset: &str) -> String {
    format!("program-{}", archive_folder(asset).trim_start_matches("arcade-v1-"))
}

/// The release file that holds this machine's archive.
fn program_file() -> Option<String> {
    asset_name().map(|a| program_file_for(&a))
}

#[derive(Default, Clone)]
struct State {
    /// What it is doing: idle, checking, downloading, installing, restarting.
    doing: &'static str,
    last_check: u64,
    newest: Option<String>,
    error: Option<String>,
    installed: Option<String>,
}

fn st() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State { doing: "idle", ..Default::default() }))
}

/// Where arcaded was started from, and how (read at start, before any update replaces it).
static SELF: OnceLock<(PathBuf, Vec<std::ffi::OsString>)> = OnceLock::new();

pub fn on(node: &Node) -> bool {
    node.app.store.read(|r| r.get(arcade_app::META, SETTING)).ok().flatten()
        .and_then(|v| serde_json::from_slice::<Value>(&v).ok()).is_some_and(|v| v["on"] == true)
}

pub fn set(node: &Node, on: bool) -> Result<Value, String> {
    let v = json!({"on": on});
    node.app.store.write(|w| w.put(arcade_app::META, SETTING, v.to_string().as_bytes())).map_err(|e| e.to_string())?;
    let _ = node.app.event("update", if on { "automatic updates turned on" } else { "automatic updates turned off" });
    Ok(status(node))
}

/// Why this arcade can't update itself, if it can't.
fn unsupported() -> Option<String> {
    if version().is_none() {
        return Some("this is a local build (no release version): update it by building again".into());
    }
    if asset_name().is_none() {
        return Some(format!("no release build for this system ({} {})", std::env::consts::OS, std::env::consts::ARCH));
    }
    let (exe, _) = SELF.get()?;
    let dir = exe.parent()?;
    let probe = dir.join(".arcade-update-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            None
        }
        Err(_) => Some(format!("{} can't be written by this arcade: update it the way it was installed", dir.display())),
    }
}

pub fn status(node: &Node) -> Value {
    let s = st().lock().unwrap().clone();
    let newer = match (s.newest.as_deref().and_then(rel), version().and_then(rel)) {
        (Some(n), Some(c)) => n > c,
        _ => false,
    };
    json!({
        "version": version(), "on": on(node), "supported": unsupported().is_none(), "why_not": unsupported(),
        "newest": s.newest, "newer": newer, "last_check": if s.last_check == 0 { Value::Null } else { json!(s.last_check) },
        "doing": s.doing, "error": s.error, "installed": s.installed,
        "every_minutes": EVERY_SECS / 60, "from": "the Arcade V1 network: releases of @arcade/arcade-v1 signed by Strawberry's release key",
    })
}

/// The newest release of the arcade's own code whose archive for this machine is held here: (tag, archive, sha256).
fn newest_release() -> Result<Option<(String, String, String)>, String> {
    let file = program_file().ok_or("no release build for this system")?;
    let best = crate::arccode::held_programs(crate::arccode::ARCADE_V1, &file)
        .into_iter()
        .filter_map(|(v, path, sha)| Some((rel(&v)?, v, path, sha)))
        .max_by(|a, b| a.0.cmp(&b.0));
    Ok(best.map(|(_, v, path, sha)| (v, path.to_string_lossy().into_owned(), sha)))
}

/// Look for a release (status updated); Some(the release) when it is newer than this build.
pub fn check() -> Result<Option<(String, String, String)>, String> {
    st().lock().unwrap().doing = "checking";
    let r = newest_release();
    let mut s = st().lock().unwrap();
    s.doing = "idle";
    s.last_check = arcade_app::now();
    match r {
        Ok(rel) => {
            s.error = None;
            s.newest = rel.as_ref().map(|r| r.0.clone());
            let cur = version().and_then(self::rel);
            Ok(rel.filter(|r| matches!((self::rel(&r.0), cur), (Some(n), Some(c)) if n > c)))
        }
        Err(e) => {
            s.error = Some(format!("couldn't look for a release: {e}"));
            Err(e)
        }
    }
}

/// The folder a release is unpacked in: inside the arcade's own home (which only its user can open), made new
/// and private each time. Not in the shared temporary folder, where another user of the computer could put
/// their own program in its place between the check and the install.
fn work_dir(home: &std::path::Path) -> Result<std::path::PathBuf, String> {
    let work = home.join(".update-work");
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(home).map_err(|e| e.to_string())?;
    std::fs::create_dir(&work).map_err(|e| format!("the update folder: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&work, std::fs::Permissions::from_mode(0o700)).map_err(|e| e.to_string())?;
    }
    Ok(work)
}

/// Check and put in place release `tag` (its archive at `archive`, which must hash to `want`); then restart into it.
fn install(node: &Node, tag: &str, archive: &str, want: &str) -> Result<(), String> {
    if let Some(why) = unsupported() {
        return Err(why);
    }
    let (exe, _) = SELF.get().ok_or("arcaded's own path unknown")?;
    let dir = exe.parent().ok_or("arcaded's folder unknown")?.to_path_buf();
    let work = work_dir(&node.home)?;
    let result = (|| -> Result<(), String> {
        st().lock().unwrap().doing = "checking the release";
        // (named as its kind: Expand-Archive, the Windows fallback, takes only a .zip)
        let tarball = work.join(if cfg!(windows) { "release.zip" } else { "release.tar.gz" });
        std::fs::copy(archive, &tarball).map_err(|e| format!("the release archive: {e}"))?;
        // Checked when it was kept; once more, as it is what runs next.
        let got = hex::encode(Sha256::digest(std::fs::read(&tarball).map_err(|e| e.to_string())?));
        if got != want {
            return Err(format!("the release archive doesn't match its record ({got} is not {want})"));
        }
        st().lock().unwrap().doing = "installing";
        unpack(&tarball, &work)?;
        let bin = work.join(archive_folder(&asset_name().unwrap())).join("bin");
        // The new arcaded must run here and be the release it says it is.
        let v = quiet(std::process::Command::new(bin.join(format!("arcaded{}", std::env::consts::EXE_SUFFIX))).arg("--version")).output().map_err(|e| format!("the new arcaded doesn't run here: {e}"))?;
        let said = String::from_utf8_lossy(&v.stdout);
        if !says_version(&said, tag, cfg!(windows)) {
            return Err(format!("the new arcaded says it is {:?}, not {tag}", said.trim()));
        }
        #[cfg(windows)]
        return swap_windows(&bin, &dir);
        // Each program beside arcaded: in place under a temporary name, then renamed over it
        // (a running program keeps the file it started from); the old one kept as .prev.
        #[allow(unreachable_code)]
        for name in PROGRAMS {
            let (new, at) = (bin.join(name), dir.join(name));
            if !new.exists() || !(at.exists() || *name == "arcaded") {
                continue;
            }
            let tmp = dir.join(format!(".{name}.update"));
            std::fs::copy(&new, &tmp).map_err(|e| format!("{name}: {e}"))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
            }
            if at.exists() {
                let _ = std::fs::remove_file(dir.join(format!("{name}.prev")));
                std::fs::hard_link(&at, dir.join(format!("{name}.prev"))).or_else(|_| std::fs::copy(&at, dir.join(format!("{name}.prev"))).map(|_| ())).map_err(|e| format!("{name}: {e}"))?;
            }
            std::fs::rename(&tmp, &at).map_err(|e| format!("{name}: {e}"))?;
        }
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&work);
    match result {
        Ok(()) => {
            // Not in the middle of this arcade's own proof of time for the next ARCADE block: wait for that block
            // (ours or anyone's), at most RESTART_WAIT, so an update doesn't throw the work away. The new program is
            // in place already, so a crash meanwhile starts it.
            let since = std::time::Instant::now();
            // Until the tip moves on (not until no proof runs: a writer alone starts its next proof the moment a block
            // lands, so it is nearly always proving; v0.1.95 waited the whole RESTART_WAIT so on the GX10).
            if crate::arcsvc::proving_for_tip() {
                let at = crate::arcsvc::tip_id();
                st().lock().unwrap().doing = "waiting for the next block";
                let _ = node.app.event("update", &format!("{tag} is in place: restarting after the next ARCADE block (this arcade's proof is running)"));
                while crate::arcsvc::proving_for_tip() && crate::arcsvc::tip_id() == at && since.elapsed() < RESTART_WAIT {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
            // Nor in the middle of the draw seed's proof the next epoch's first block needs (one VDF run that a restart
            // starts over, often longer than the epoch has left): wait for it, at most SEED_WAIT.
            if crate::arcsvc::seed_running() {
                let since = std::time::Instant::now();
                st().lock().unwrap().doing = "waiting for the draw seed's proof";
                let _ = node.app.event("update", &format!("{tag} is in place: restarting once the ARCADE draw seed's proof is made"));
                while crate::arcsvc::seed_running() && since.elapsed() < SEED_WAIT {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
            }
            let mut s = st().lock().unwrap();
            s.installed = Some(tag.to_string());
            s.doing = "restarting";
            s.error = None;
            drop(s);
            let _ = node.app.event("update", &format!("updated to {tag}: restarting"));
            crate::request_restart();
            Ok(())
        }
        Err(e) => {
            let mut s = st().lock().unwrap();
            s.doing = "idle";
            s.error = Some(format!("update to {tag} failed: {e}"));
            drop(s);
            let _ = node.app.event("update", &format!("update to {tag} failed: {e}"));
            Err(e)
        }
    }
}

/// Unpack a release archive into `work`: tar.gz with tar (Linux, as before); on Windows the zip with Windows' own
/// tar.exe (bsdtar reads zip), else PowerShell's Expand-Archive (as install.ps1 does).
fn unpack(archive: &std::path::Path, work: &std::path::Path) -> Result<(), String> {
    #[cfg(not(windows))]
    {
        let out = std::process::Command::new("tar").arg("--no-same-owner").arg("-xzf").arg(archive).arg("-C").arg(work).output().map_err(|e| format!("tar: {e}"))?;
        if !out.status.success() {
            return Err(format!("tar: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let root = std::env::var_os("SystemRoot").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
        let tar = root.join("System32").join("tar.exe");
        let first = quiet(std::process::Command::new(&tar).arg("-xf").arg(archive).arg("-C").arg(work)).output();
        if matches!(&first, Ok(o) if o.status.success()) {
            return Ok(());
        }
        let ps = root.join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe");
        let q = |p: &std::path::Path| p.display().to_string().replace('\'', "''");
        let cmd = format!("$ErrorActionPreference='Stop'; Expand-Archive -LiteralPath '{}' -DestinationPath '{}' -Force", q(archive), q(work));
        let out = quiet(std::process::Command::new(ps).args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", &cmd])).output().map_err(|e| format!("Expand-Archive: {e}"))?;
        if !out.status.success() {
            let tar_err = match first {
                Ok(o) => String::from_utf8_lossy(&o.stderr).trim().to_string(),
                Err(e) => e.to_string(),
            };
            return Err(format!("unzip: tar: {tar_err}; Expand-Archive: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
        Ok(())
    }
}

/// A helper program started with no console window (Windows); as it is elsewhere.
fn quiet(c: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// The files of a Windows release put beside arcaded: the programs (when present there, arcaded always) and the
/// MPIR DLLs chiavdf links. Each running one is renamed to `<name>.prev` (Windows lets a running program or loaded
/// DLL be renamed, not replaced) and the new one renamed into its place; if any step fails, every file done so far
/// is put back as it was.
#[cfg(windows)]
fn swap_windows(bin: &std::path::Path, dir: &std::path::Path) -> Result<(), String> {
    let mut names: Vec<String> = PROGRAMS
        .iter()
        .map(|n| format!("{n}{}", std::env::consts::EXE_SUFFIX))
        .filter(|n| bin.join(n).exists() && (dir.join(n).exists() || n.starts_with("arcaded.")))
        .collect();
    if let Ok(rd) = std::fs::read_dir(bin) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.to_ascii_lowercase().ends_with(".dll") {
                names.push(n);
            }
        }
    }
    // (name, whether an old one was moved to .prev)
    let mut done: Vec<(String, bool)> = Vec::new();
    let undo = |done: &[(String, bool)]| {
        for (n, had) in done.iter().rev() {
            let (at, prev) = (dir.join(n), dir.join(format!("{n}.prev")));
            let _ = std::fs::rename(&at, dir.join(format!(".{n}.failed")));
            let _ = std::fs::remove_file(dir.join(format!(".{n}.failed")));
            if *had {
                let _ = std::fs::rename(&prev, &at);
            }
        }
    };
    for n in &names {
        let (new, at, prev, tmp) = (bin.join(n), dir.join(n), dir.join(format!("{n}.prev")), dir.join(format!(".{n}.update")));
        let step = (|| -> Result<bool, String> {
            std::fs::copy(&new, &tmp).map_err(|e| format!("{n}: {e}"))?;
            let had = at.exists();
            if had {
                clear_prev(dir, n);
                std::fs::rename(&at, &prev).map_err(|e| format!("{n}: {e}"))?;
            }
            if let Err(e) = std::fs::rename(&tmp, &at) {
                if had {
                    let _ = std::fs::rename(&prev, &at);
                }
                return Err(format!("{n}: {e}"));
            }
            Ok(had)
        })();
        match step {
            Ok(had) => done.push((n.clone(), had)),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                undo(&done);
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Make room for `<name>.prev`: removed, or (a program from the update before still running, e.g. a window not
/// reopened since) renamed aside to be removed at a later start.
#[cfg(windows)]
fn clear_prev(dir: &std::path::Path, n: &str) {
    let prev = dir.join(format!("{n}.prev"));
    if !prev.exists() || std::fs::remove_file(&prev).is_ok() {
        return;
    }
    let aside = dir.join(format!(".{n}.old-{}-{}", std::process::id(), arcade_app::now()));
    let _ = std::fs::rename(&prev, &aside);
}

/// At start (Windows): the old programs set aside by an earlier update, removed now that nothing runs them.
#[cfg(windows)]
fn clean_aside(dir: &std::path::Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with('.') && (n.contains(".old-") || n.ends_with(".failed")) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// "Update now": install the newest release if it is newer (in the background).
pub fn update_now(node: Arc<Node>) -> Result<Value, String> {
    if let Some(why) = unsupported() {
        return Err(why);
    }
    if st().lock().unwrap().doing != "idle" {
        return Err("an update is already under way".into());
    }
    let Some((tag, tar, sum)) = check()? else {
        return Err(format!("{} is the newest release", version().unwrap_or("this")));
    };
    let n = node.clone();
    std::thread::Builder::new().name("update".into()).spawn(move || {
        let _ = install(&n, &tag, &tar, &sum);
    }).map_err(|e| e.to_string())?;
    Ok(status(&node))
}

/// At start: remember how arcaded was started, and look for releases in the background (an
/// update is installed only while the operator has them on).
pub fn start(node: Arc<Node>) {
    if let Ok(exe) = std::env::current_exe() {
        #[cfg(windows)]
        if let Some(d) = exe.parent() {
            clean_aside(d);
        }
        let _ = SELF.set((exe, std::env::args_os().skip(1).collect()));
    }
    let _ = std::thread::Builder::new().name("updates".into()).spawn(move || {
        // ARCADE_UPDATE_FIRST_SECS: the first look sooner (tests).
        let mut wait = std::env::var("ARCADE_UPDATE_FIRST_SECS").ok().and_then(|v| v.parse().ok())
            .unwrap_or_else(|| FIRST_SECS + (rand_core::RngCore::next_u64(&mut rand_core::OsRng) % 300));
        loop {
            std::thread::sleep(std::time::Duration::from_secs(wait));
            wait = EVERY_SECS + (rand_core::RngCore::next_u64(&mut rand_core::OsRng) % 1800);
            if !on(&node) || unsupported().is_some() || st().lock().unwrap().doing != "idle" {
                continue;
            }
            if let Ok(Some((tag, tar, sum))) = check() {
                if install(&node, &tag, &tar, &sum).is_ok() {
                    return;
                }
            }
        }
    });
}

/// After the clean stop: start the arcaded now at the same path, with the same options, in this
/// process's place (the service manager sees the same process carry on).
#[cfg(unix)]
pub fn exec_self() -> ! {
    use std::os::unix::process::CommandExt;
    match SELF.get() {
        Some((exe, args)) => {
            let e = std::process::Command::new(exe).args(args).exec();
            eprintln!("arcaded: couldn't start the new version ({e}); exiting so the service starts it");
            std::process::exit(75)
        }
        None => std::process::exit(75),
    }
}

/// Windows has no exec: the new arcaded is started in a process of its own, with the same options, folder and
/// environment, and [`RESTARTED`] set so it waits for this one's `arcaded.lock`; then this one leaves. (wscript and
/// the logon task don't start arcaded again by themselves.)
#[cfg(windows)]
pub fn exec_self() -> ! {
    use std::os::windows::process::CommandExt;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    match SELF.get() {
        Some((exe, args)) => {
            let start = |flags: u32| std::process::Command::new(exe).args(args).env(RESTARTED, "1").creation_flags(flags).spawn();
            // Out of the logon task's job if it allows that (so the task ending can't take it along), else in it.
            match start(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB).or_else(|_| start(CREATE_NO_WINDOW)) {
                Ok(_) => std::process::exit(0),
                Err(e) => {
                    eprintln!("arcaded: couldn't start the new version ({e}); exiting so the service starts it");
                    std::process::exit(75)
                }
            }
        }
        None => std::process::exit(75),
    }
}

#[cfg(not(any(unix, windows)))]
pub fn exec_self() -> ! {
    std::process::exit(75)
}

/// Set in the environment of an arcaded started by an update (Windows): it waits for the old one to let go of
/// `arcaded.lock` before taking it.
pub const RESTARTED: &str = "ARCADE_UPDATE_RESTARTED";

#[cfg(test)]
mod tests {
    #[test]
    fn the_update_folder_is_private_and_in_the_home() {
        let home = std::env::temp_dir().join(format!("arcade-uptest-{}", std::process::id()));
        let w = super::work_dir(&home).unwrap();
        assert!(w.starts_with(&home));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&w).unwrap().permissions().mode() & 0o777, 0o700);
        }
        std::fs::write(w.join("left"), b"x").unwrap();
        let w2 = super::work_dir(&home).unwrap();
        assert!(!w2.join("left").exists(), "made new each time");
        let _ = std::fs::remove_dir_all(&home);
    }

    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse("v0.1.63"), Some((0, 1, 63)));
        assert!(parse("v0.1.63-windows").is_none());
        assert!(parse("0.1.63").is_none());
        assert!(parse("v0.1.100") > parse("v0.1.99"));
        assert!(parse("v0.2.0") > parse("v0.1.99"));
    }

    #[test]
    fn windows_versions() {
        // Linux: as before, only vX.Y.Z.
        assert_eq!(release_version("v0.1.71", false), Some((0, 1, 71)));
        assert!(release_version("v0.1.71-windows", false).is_none());
        // Windows: its own builds' versions compare as the release's.
        assert_eq!(release_version("v0.1.71-windows", true), Some((0, 1, 71)));
        assert_eq!(release_version("v0.1.72", true), Some((0, 1, 72)));
        assert!(release_version("v0.1.72-windows", true) > release_version("v0.1.71-windows", true));
        assert!(release_version("v0.1.71-linux", true).is_none());
        assert!(release_version("v0.1.71-windows-windows", true).is_none());
    }

    #[test]
    fn says_its_version() {
        assert!(says_version("arcaded v0.1.72\n", "v0.1.72", false));
        assert!(!says_version("arcaded v0.1.72-windows\n", "v0.1.72", false));
        assert!(says_version("arcaded v0.1.72-windows\r\n", "v0.1.72", true));
        assert!(says_version("arcaded v0.1.72-windows\r\n", "v0.1.72-windows", true));
        assert!(says_version("arcaded v0.1.72\r\n", "v0.1.72-windows", true));
        assert!(!says_version("arcaded v0.1.71-windows\r\n", "v0.1.72", true));
        assert!(!says_version("arcaded 0.1.0 (local build)", "v0.1.72", true));
    }

    #[test]
    fn archives() {
        assert_eq!(asset_for("linux", "x86_64").as_deref(), Some("arcade-v1-linux-x86_64.tar.gz"));
        assert_eq!(asset_for("linux", "aarch64").as_deref(), Some("arcade-v1-linux-aarch64.tar.gz"));
        assert_eq!(asset_for("windows", "x86_64").as_deref(), Some("arcade-v1-windows-x86_64.zip"));
        assert!(asset_for("windows", "aarch64").is_none());
        assert!(asset_for("macos", "aarch64").is_none());
        assert_eq!(program_file_for("arcade-v1-linux-x86_64.tar.gz"), "program-linux-x86_64");
        assert_eq!(program_file_for("arcade-v1-linux-aarch64.tar.gz"), "program-linux-aarch64");
        assert_eq!(program_file_for("arcade-v1-windows-x86_64.zip"), "program-windows-x86_64");
        assert_eq!(archive_folder("arcade-v1-windows-x86_64.zip"), "arcade-v1-windows-x86_64");
        assert_eq!(archive_folder("arcade-v1-linux-x86_64.tar.gz"), "arcade-v1-linux-x86_64");
    }
}
