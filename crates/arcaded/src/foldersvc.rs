//! arcade.folder (2026-11-21, Martin approved): a person gives an app one folder on this computer, and the app may read
//! and write inside it and nowhere else. Any app: the arcade knows nothing of what an app keeps there.
//!
//! - The grant is kept per app id in the app store's META (`appfolder:<id>` = {path, name, granted_at}); off until the
//!   person picks a folder on the app window's card (apps.folder_set). apps.perms shows {granted, name}, never the path,
//!   and `set: {folder: false}` takes it away.
//! - The page API (/r/folder/*, crate::http) and the socket commands folder.* (for programs on this computer) go through
//!   the same functions here. Paths are relative to the folder; anything that would land outside it (absolute paths,
//!   `..`, a symlink that leads out) is refused.
//! - A folder holds at most 200 MB of what an app writes (counted over the whole folder, up to 20,000 entries; more
//!   than that counts as full); one read or write is at most 8 MB.
//! - Every write, mkdir and remove is logged (the last 500 per app, apps.folder_log).
//! - Nothing here runs anything: no commands, no opening files with other programs.

use crate::node::Node;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The most one read or write moves.
pub const MAX_IO: usize = 8 << 20;
/// The most a granted folder may hold once a write is done.
pub const QUOTA: u64 = 200 << 20;
/// Entries counted when a folder's size is measured; more counts as over the quota.
pub const WALK_CAP: usize = 20_000;
/// Changes logged per app.
pub const LOG_KEEP: usize = 500;
/// Entries one list answers (and subfolders one browse answers).
const LIST_MOST: usize = 5_000;

/// Writes, mkdirs, removes and their log, one at a time, so two writes at once can't both fit what only one does.
static CHANGING: Mutex<()> = Mutex::new(());

/// Pages to the app they belong to (page txid -> app id), filled when apps.launch answers (it knows both). An app page
/// asks with its pass, which names only the page; looking the app up through the whole App Store (appsvc::of_page)
/// takes minutes on a busy arcade, so that is only the fallback (after a restart, say), and its answer is kept here.
static PAGES: Mutex<BTreeMap<String, String>> = Mutex::new(BTreeMap::new());

pub fn remember_page(txid: &str, id: &str) {
    if !txid.is_empty() && !id.is_empty() {
        PAGES.lock().unwrap().insert(txid.to_ascii_lowercase(), id.to_string());
    }
}

/// The app id of a page: from what apps.launch told, else from the App Store (kept for next time).
pub fn app_of_page(node: &Node, txid: &str) -> Option<String> {
    let txid = txid.to_ascii_lowercase();
    if let Some(id) = PAGES.lock().unwrap().get(&txid) {
        return Some(id.clone());
    }
    let id = crate::appsvc::of_page(node, &txid)?["id"].as_str()?.to_string();
    remember_page(&txid, &id);
    Some(id)
}

fn meta_get(node: &Node, k: &str) -> Value {
    node.app.store.read(|r| r.get(arcade_app::META, k.as_bytes())).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or(Value::Null)
}

fn meta_put(node: &Node, k: &str, v: &Value) -> Result<(), String> {
    node.app.store.write(|w| w.put(arcade_app::META, k.as_bytes(), v.to_string().as_bytes())).map_err(|e| e.to_string())
}

fn meta_del(node: &Node, k: &str) -> Result<(), String> {
    node.app.store.write(|w| w.del(arcade_app::META, k.as_bytes())).map(|_| ()).map_err(|e| e.to_string())
}

fn grant_key(id: &str) -> String {
    format!("appfolder:{id}")
}

fn log_key(id: &str) -> String {
    format!("appfolderlog:{id}")
}

// ---------------- the grant ----------------

/// The folder an app was given: its canonical path.
fn root_of(node: &Node, id: &str) -> Result<PathBuf, String> {
    let g = meta_get(node, &grant_key(id));
    let p = g["path"].as_str().ok_or("no folder: this app was not given one (arcade.folder.ask)")?;
    // Canonical again each time: the folder may have been moved or removed since.
    std::fs::canonicalize(p).map_err(|_| "the folder this app was given is gone (moved or removed): ask for one again".to_string())
}

/// {granted, name}: what an app page and apps.perms may see (never the path).
pub fn status(node: &Node, id: &str) -> Value {
    let g = meta_get(node, &grant_key(id));
    match g["path"].as_str() {
        Some(_) => json!({"granted": true, "name": g["name"]}),
        None => json!({"granted": false, "name": Value::Null}),
    }
}

/// Where the person may pick a folder from: their home folder, and on Windows the other drives.
pub fn browse_roots() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    if let Some(h) = home.and_then(|h| std::fs::canonicalize(h).ok()) {
        out.push(h);
    }
    #[cfg(windows)]
    {
        // Other drives (D:, E:, ...): the drive the home folder is on is reached through the home folder only.
        let home_drive = out.first().map(|h| h.to_string_lossy().trim_start_matches(r"\\?\").chars().next().unwrap_or('C').to_ascii_uppercase());
        for c in b'A'..=b'Z' {
            let d = c as char;
            if Some(d) == home_drive {
                continue;
            }
            if let Ok(p) = std::fs::canonicalize(format!("{d}:\\")) {
                out.push(p);
            }
        }
    }
    out
}

/// Give app `id` the folder at `path` (apps.folder_set: the window, after the person picked it on its card). Only a
/// folder under one of `roots` (browse_roots), not a root itself, not hidden, and not this arcade's own data folder.
pub fn grant(node: &Node, id: &str, path: &str, roots: &[PathBuf]) -> Result<Value, String> {
    if id.is_empty() || id.len() > 256 {
        return Err("id: an app id".into());
    }
    let p = std::fs::canonicalize(path).map_err(|_| "path: no such folder".to_string())?;
    if !p.is_dir() {
        return Err("path: not a folder".into());
    }
    let Some(root) = roots.iter().find(|r| p.starts_with(r)) else { return Err("path: pick a folder inside your home folder".into()) };
    if p == *root {
        return Err("path: pick a folder inside it, not the whole of it".into());
    }
    if p.strip_prefix(root).unwrap().components().any(|c| c.as_os_str().to_string_lossy().starts_with('.')) {
        return Err("path: a hidden folder can't be given to an app".into());
    }
    grant_unchecked(node, id, &p)
}

/// The grant itself, for a folder already checked (and for the tests, whose folders are in the temp dir).
fn grant_unchecked(node: &Node, id: &str, p: &Path) -> Result<Value, String> {
    let p = std::fs::canonicalize(p).map_err(|_| "path: no such folder".to_string())?;
    // This arcade's own files (its keys among them) are never an app's.
    if let Ok(h) = std::fs::canonicalize(&node.home) {
        if p.starts_with(&h) || h.starts_with(&p) {
            return Err("path: that folder holds this arcade's own files".into());
        }
    }
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    meta_put(node, &grant_key(id), &json!({"path": p.to_string_lossy(), "name": name, "granted_at": arcade_app::now()}))?;
    Ok(json!({"granted": true, "name": name}))
}

/// Take the folder away (apps.perms set {folder: false}). The folder and its files stay as they are.
pub fn revoke(node: &Node, id: &str) -> Result<(), String> {
    meta_del(node, &grant_key(id))
}

/// folder.browse: the subfolders (names only, no files, no hidden ones) of `path`, or of the home folder; for the
/// window's folder picker. Only inside browse_roots.
pub fn browse(path: Option<&str>) -> Result<Value, String> {
    browse_in(path, &browse_roots())
}

fn browse_in(path: Option<&str>, roots: &[PathBuf]) -> Result<Value, String> {
    let first = roots.first().ok_or("no home folder is known here")?;
    let p = match path.filter(|p| !p.is_empty()) {
        Some(p) => std::fs::canonicalize(p).map_err(|_| "path: no such folder".to_string())?,
        None => first.clone(),
    };
    let Some(root) = roots.iter().find(|r| p.starts_with(r)) else { return Err("path: only folders inside your home folder".into()) };
    if !p.is_dir() {
        return Err("path: not a folder".into());
    }
    let mut dirs: Vec<String> = std::fs::read_dir(&p)
        .map_err(|e| format!("can't read that folder: {e}"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !n.starts_with('.'))
        .collect();
    dirs.sort_by_key(|a| a.to_lowercase());
    let more = dirs.len() > LIST_MOST;
    dirs.truncate(LIST_MOST);
    let parent = (p != *root).then(|| p.parent().map(|x| x.to_string_lossy().into_owned())).flatten();
    // The way down from the root, for the card's breadcrumb: [{name, path}].
    let mut crumbs = vec![json!({"name": root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.to_string_lossy().into_owned()), "path": root.to_string_lossy()})];
    let mut at = root.clone();
    for c in p.strip_prefix(root).unwrap().components() {
        at.push(c);
        crumbs.push(json!({"name": c.as_os_str().to_string_lossy(), "path": at.to_string_lossy()}));
    }
    Ok(json!({
        "path": p.to_string_lossy(), "name": p.file_name().map(|n| n.to_string_lossy().into_owned()), "parent": parent,
        "is_root": p == *root, "crumbs": crumbs, "dirs": dirs, "more": more,
        "roots": roots.iter().map(|r| json!({"name": r.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| r.to_string_lossy().into_owned()), "path": r.to_string_lossy()})).collect::<Vec<_>>(),
    }))
}

// ---------------- paths inside the folder ----------------

/// A path an app names, as its parts: relative to the folder, never absolute, no `..`, no NUL, no `:` (a Windows
/// drive or stream). "" (or ".") is the folder itself.
pub fn parts(rel: &str) -> Result<Vec<String>, String> {
    if rel.len() > 1024 {
        return Err("path: at most 1024 characters".into());
    }
    if rel.contains('\0') {
        return Err("path: no NUL characters".into());
    }
    if rel.starts_with('/') || rel.starts_with('\\') || Path::new(rel).is_absolute() || Path::new(rel).has_root() {
        return Err("path: relative to the folder, not absolute".into());
    }
    let mut out = Vec::new();
    for c in rel.split(['/', '\\']) {
        match c {
            "" | "." => continue,
            ".." => return Err("path: no .. in it".into()),
            c if c.contains(':') => return Err("path: no : in it".into()),
            c => out.push(c.to_string()),
        }
    }
    Ok(out)
}

/// The place a path names inside `root` (canonical): the parent folder, canonical and inside the root, joined with the
/// last part (not followed). For the root itself: the root. A last part that exists and is a symlink must lead inside
/// the root too.
pub fn resolve(root: &Path, rel: &str) -> Result<PathBuf, String> {
    let ps = parts(rel)?;
    let Some((last, dirs)) = ps.split_last() else { return Ok(root.to_path_buf()) };
    let mut parent = root.to_path_buf();
    for d in dirs {
        parent.push(d);
    }
    let parent = std::fs::canonicalize(&parent).map_err(|_| "no such folder in the folder".to_string())?;
    if !parent.starts_with(root) {
        return Err("path: outside the folder".into());
    }
    if !parent.is_dir() {
        return Err("path: a part of it is a file, not a folder".into());
    }
    let full = parent.join(last);
    if let Ok(m) = std::fs::symlink_metadata(&full) {
        if m.file_type().is_symlink() {
            let to = std::fs::canonicalize(&full).map_err(|_| "path: a link that leads nowhere".to_string())?;
            if !to.starts_with(root) {
                return Err("path: outside the folder".into());
            }
        }
    }
    Ok(full)
}

/// What the folder holds now: bytes, or None when it has more than WALK_CAP entries (counted as full). Symlinks are
/// counted as entries and never followed.
pub fn folder_size(root: &Path) -> Option<u64> {
    let (mut total, mut n, mut stack) = (0u64, 0usize, vec![root.to_path_buf()]);
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            n += 1;
            if n > WALK_CAP {
                return None;
            }
            let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else if m.is_file() {
                total += m.len();
            }
        }
    }
    Some(total)
}

fn rel_name(rel: &str) -> String {
    parts(rel).map(|p| p.join("/")).unwrap_or_default()
}

fn entry(name: &str, m: &std::fs::Metadata) -> Value {
    let modified = m.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs());
    let kind = if m.file_type().is_symlink() { "link" } else if m.is_dir() { "dir" } else { "file" };
    json!({"name": name, "kind": kind, "dir": m.is_dir(), "size": if m.is_file() { m.len() } else { 0 }, "modified": modified})
}

fn log(node: &Node, id: &str, op: &str, rel: &str, bytes: u64) -> Result<(), String> {
    let k = log_key(id);
    let mut l: Vec<Value> = meta_get(node, &k).as_array().cloned().unwrap_or_default();
    l.insert(0, json!({"op": op, "path": rel_name(rel), "bytes": bytes, "at": arcade_app::now()}));
    l.truncate(LOG_KEEP);
    meta_put(node, &k, &json!(l))
}

/// apps.folder_log: an app's changes in its folder, newest first (at most 500), and {granted, name}.
pub fn folder_log(node: &Node, id: &str, limit: Option<usize>) -> Value {
    let mut l: Vec<Value> = meta_get(node, &log_key(id)).as_array().cloned().unwrap_or_default();
    l.truncate(limit.unwrap_or(LOG_KEEP).min(LOG_KEEP));
    let mut s = status(node, id);
    s["changes"] = json!(l);
    s
}

// ---------------- what an app does in its folder ----------------

/// One folder op for app `id` (the page API and the socket commands): list, stat, read, write, mkdir, remove. `args`:
/// {path, data?, as?: text|base64} (encoding is taken for as).
pub fn op(node: &Node, id: &str, op: &str, args: &Value) -> Result<Value, String> {
    let rel = match &args["path"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        _ => return Err("path: text".into()),
    };
    let enc = args["as"].as_str().or(args["encoding"].as_str()).unwrap_or("text");
    if enc != "text" && enc != "base64" {
        return Err("as: text or base64".into());
    }
    let root = root_of(node, id)?;
    match op {
        "list" => {
            let p = resolve(&root, &rel)?;
            let mut out: Vec<Value> = std::fs::read_dir(&p)
                .map_err(|_| "not a folder in the folder".to_string())?
                .flatten()
                .filter_map(|e| std::fs::symlink_metadata(e.path()).ok().map(|m| entry(&e.file_name().to_string_lossy(), &m)))
                .collect();
            out.sort_by(|a, b| (b["dir"] == true).cmp(&(a["dir"] == true)).then(a["name"].as_str().cmp(&b["name"].as_str())));
            let more = out.len() > LIST_MOST;
            out.truncate(LIST_MOST);
            Ok(json!({"path": rel_name(&rel), "entries": out, "more": more}))
        }
        "stat" => {
            let p = resolve(&root, &rel)?;
            match std::fs::metadata(&p) {
                Ok(m) => {
                    let mut e = entry(p.file_name().map(|n| n.to_string_lossy().into_owned()).as_deref().unwrap_or(""), &m);
                    e["exists"] = json!(true);
                    e["path"] = json!(rel_name(&rel));
                    Ok(e)
                }
                Err(_) => Ok(json!({"exists": false, "path": rel_name(&rel)})),
            }
        }
        "read" => {
            let p = resolve(&root, &rel)?;
            let m = std::fs::metadata(&p).map_err(|_| "no such file in the folder".to_string())?;
            if !m.is_file() {
                return Err("not a file".into());
            }
            if m.len() > MAX_IO as u64 {
                return Err("too big: a read is at most 8 MB".into());
            }
            let bytes = std::fs::read(&p).map_err(|e| format!("can't read it: {e}"))?;
            let data = if enc == "base64" {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode(&bytes)
            } else {
                String::from_utf8(bytes).map_err(|_| "not text: read it with as: base64".to_string())?
            };
            Ok(json!({"path": rel_name(&rel), "size": m.len(), "as": enc, "data": data}))
        }
        "write" => {
            let data = args["data"].as_str().ok_or("data: text, or base64 with as: base64")?;
            let bytes = if enc == "base64" {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.decode(data).map_err(|_| "data: not base64".to_string())?
            } else {
                data.as_bytes().to_vec()
            };
            if bytes.len() > MAX_IO {
                return Err("too big: a write is at most 8 MB".into());
            }
            if parts(&rel)?.is_empty() {
                return Err("path: name a file".into());
            }
            let _one = CHANGING.lock().unwrap();
            let p = resolve(&root, &rel)?;
            let old = match std::fs::metadata(&p) {
                Ok(m) if m.is_dir() => return Err("that is a folder".into()),
                Ok(m) => m.len(),
                Err(_) => 0,
            };
            let now = folder_size(&root).ok_or("the folder is full: it holds more than 20,000 entries")?;
            if now.saturating_sub(old) + bytes.len() as u64 > QUOTA {
                return Err("the folder is full: an app may keep at most 200 MB in it".into());
            }
            write_file(&p, &bytes).map_err(|e| format!("can't write it: {e}"))?;
            log(node, id, "write", &rel, bytes.len() as u64)?;
            Ok(json!({"path": rel_name(&rel), "size": bytes.len()}))
        }
        "mkdir" => {
            let ps = parts(&rel)?;
            if ps.is_empty() {
                return Err("path: name a folder".into());
            }
            let _one = CHANGING.lock().unwrap();
            // One level at a time, each checked as it is made.
            let mut made = false;
            for i in 1..=ps.len() {
                let p = resolve(&root, &ps[..i].join("/"))?;
                match std::fs::symlink_metadata(&p) {
                    Ok(m) if m.is_dir() || (m.file_type().is_symlink() && p.is_dir()) => continue,
                    Ok(_) => return Err("a file is in the way".into()),
                    Err(_) => {
                        std::fs::create_dir(&p).map_err(|e| format!("can't make it: {e}"))?;
                        made = true;
                    }
                }
            }
            if made {
                log(node, id, "mkdir", &rel, 0)?;
            }
            Ok(json!({"path": rel_name(&rel), "made": made}))
        }
        "remove" => {
            if parts(&rel)?.is_empty() {
                return Err("path: the folder itself can't be removed".into());
            }
            let _one = CHANGING.lock().unwrap();
            let p = resolve(&root, &rel)?;
            // The entry itself, never what a link leads to.
            let m = std::fs::symlink_metadata(&p).map_err(|_| "no such file in the folder".to_string())?;
            let bytes = if m.is_file() { m.len() } else { 0 };
            if m.is_dir() {
                std::fs::remove_dir(&p).map_err(|_| "a folder is removed only when it is empty".to_string())?;
            } else {
                std::fs::remove_file(&p).map_err(|e| format!("can't remove it: {e}"))?;
            }
            log(node, id, "remove", &rel, bytes)?;
            Ok(json!({"path": rel_name(&rel), "removed": true}))
        }
        _ => Err("folder ops are list, stat, read, write, mkdir and remove".into()),
    }
}

/// Write a file without following a symlink at its last part (one swapped in after resolve() looked).
fn write_file(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    let mut f = o.open(p)?;
    f.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(d.path()).unwrap().join("work");
        std::fs::create_dir(&root).unwrap();
        (d, std::fs::canonicalize(root).unwrap())
    }

    #[test]
    fn paths_stay_inside_the_folder() {
        let (_d, root) = tmp();
        // Absolute, .., NUL and drive-like paths are refused before anything is looked at.
        for bad in ["/etc/passwd", "\\x", "../x", "a/../../x", "a/..", "x\0y", "C:/x", "a/b:c"] {
            assert!(resolve(&root, bad).is_err(), "{bad:?}");
        }
        assert_eq!(resolve(&root, "").unwrap(), root);
        assert_eq!(resolve(&root, "./").unwrap(), root);
        // A new file in a subfolder: its parent is checked, the file needn't exist.
        std::fs::create_dir(root.join("sub")).unwrap();
        assert_eq!(resolve(&root, "sub/new.txt").unwrap(), root.join("sub/new.txt"));
        assert!(resolve(&root, "nope/new.txt").is_err(), "a parent that isn't there");
        std::fs::write(root.join("f"), b"x").unwrap();
        assert!(resolve(&root, "f/inside").is_err(), "a file as a folder");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_folder_is_refused() {
        let (d, root) = tmp();
        let outside = std::fs::canonicalize(d.path()).unwrap().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"s").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("out")).unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), root.join("s")).unwrap();
        assert!(resolve(&root, "out/secret").is_err(), "through a linked folder");
        assert!(resolve(&root, "out/new").is_err(), "a new file through a linked folder");
        assert!(resolve(&root, "s").is_err(), "a linked file");
        // A link that stays inside is fine.
        std::fs::create_dir(root.join("in")).unwrap();
        std::os::unix::fs::symlink(root.join("in"), root.join("in2")).unwrap();
        assert!(resolve(&root, "in2/x").is_ok());
    }

    #[test]
    fn writes_reads_quota_log_and_revoke() {
        let (_one, t) = crate::http::tests::node();
        let n = &t.node;
        let (_d, root) = tmp();
        let id = "test-folder-app";
        assert_eq!(status(n, id)["granted"], false);
        assert!(op(n, id, "list", &json!({})).is_err(), "nothing given yet");
        assert_eq!(grant_unchecked(n, id, &root).unwrap()["name"], "work");
        // apps.perms shows the grant by name, never the path.
        let p = crate::appsvc::perms(n, id);
        assert_eq!(p["folder"], json!({"granted": true, "name": "work"}));
        assert!(!p.to_string().contains(&*root.to_string_lossy()));

        assert_eq!(op(n, id, "mkdir", &json!({"path": "notes/2026"})).unwrap()["made"], true);
        op(n, id, "write", &json!({"path": "notes/2026/a.txt", "data": "hello"})).unwrap();
        assert_eq!(op(n, id, "read", &json!({"path": "notes/2026/a.txt"})).unwrap()["data"], "hello");
        op(n, id, "write", &json!({"path": "b.bin", "data": "AAEC", "as": "base64"})).unwrap();
        assert_eq!(std::fs::read(root.join("b.bin")).unwrap(), vec![0, 1, 2]);
        assert!(op(n, id, "read", &json!({"path": "b.bin"})).is_ok(), "bytes 0,1,2 are valid UTF-8");
        assert_eq!(op(n, id, "read", &json!({"path": "b.bin", "as": "base64"})).unwrap()["data"], "AAEC");
        let l = op(n, id, "list", &json!({"path": ""})).unwrap();
        assert_eq!(l["entries"][0]["name"], "notes", "folders first: {l}");
        assert_eq!(op(n, id, "stat", &json!({"path": "nope"})).unwrap()["exists"], false);
        assert_eq!(op(n, id, "stat", &json!({"path": "b.bin"})).unwrap()["size"], 3);
        assert!(op(n, id, "remove", &json!({"path": "notes"})).is_err(), "not empty");
        assert!(op(n, id, "remove", &json!({"path": ""})).is_err(), "the folder itself");
        op(n, id, "remove", &json!({"path": "notes/2026/a.txt"})).unwrap();
        op(n, id, "remove", &json!({"path": "notes/2026"})).unwrap();
        assert!(op(n, id, "write", &json!({"path": "../x", "data": "x"})).is_err());

        // One write at most 8 MB; the folder at most 200 MB in all.
        let big = "x".repeat(MAX_IO + 1);
        assert!(op(n, id, "write", &json!({"path": "big", "data": big})).unwrap_err().contains("8 MB"));
        let f = std::fs::File::create(root.join("filler")).unwrap();
        f.set_len(QUOTA - 10).unwrap();
        assert!(op(n, id, "write", &json!({"path": "c", "data": "12345"})).is_ok());
        assert!(op(n, id, "write", &json!({"path": "d", "data": "12345"})).unwrap_err().contains("200 MB"));
        // Rewriting a file counts its new size, not both.
        assert!(op(n, id, "write", &json!({"path": "c", "data": "1234567"})).is_ok());
        std::fs::remove_file(root.join("filler")).unwrap();

        // The log: newest first, each write, mkdir and remove with its path and bytes.
        let lg = folder_log(n, id, None);
        let ch = lg["changes"].as_array().unwrap();
        assert_eq!(ch[0]["op"], "write");
        assert_eq!(ch[0]["path"], "c");
        assert_eq!(ch[0]["bytes"], 7);
        let ops: Vec<&str> = ch.iter().rev().map(|c| c["op"].as_str().unwrap()).collect();
        assert_eq!(ops, ["mkdir", "write", "write", "remove", "remove", "write", "write"]);
        assert!(folder_log(n, id, Some(2))["changes"].as_array().unwrap().len() == 2);
        for i in 0..LOG_KEEP + 5 {
            log(n, id, "write", &format!("x{i}"), 1).unwrap();
        }
        assert_eq!(folder_log(n, id, None)["changes"].as_array().unwrap().len(), LOG_KEEP);

        // Revoked through apps.perms: nothing more, the files stay.
        crate::appsvc::set_perms(n, id, &json!({"folder": false})).unwrap();
        assert_eq!(crate::appsvc::perms(n, id)["folder"]["granted"], false);
        assert!(op(n, id, "read", &json!({"path": "c"})).is_err());
        assert!(root.join("c").exists());
        // The arcade's own folder is never given.
        assert!(grant_unchecked(n, id, &n.home).is_err());
    }

    #[test]
    fn the_quota_counts_entries_too() {
        let (_d, root) = tmp();
        assert_eq!(folder_size(&root), Some(0));
        std::fs::write(root.join("a"), b"abc").unwrap();
        std::fs::create_dir(root.join("d")).unwrap();
        std::fs::write(root.join("d/b"), b"de").unwrap();
        assert_eq!(folder_size(&root), Some(5));
        for i in 0..WALK_CAP {
            std::fs::write(root.join(format!("d/{i}")), b"").unwrap();
        }
        assert_eq!(folder_size(&root), None, "over 20,000 entries counts as full");
    }

    #[test]
    fn the_picker_stays_in_its_roots() {
        let (d, root) = tmp();
        std::fs::create_dir(root.join("Pictures")).unwrap();
        std::fs::create_dir(root.join(".hidden")).unwrap();
        std::fs::write(root.join("file.txt"), b"x").unwrap();
        let roots = vec![root.clone()];
        let b = browse_in(None, &roots).unwrap();
        assert_eq!(b["dirs"], json!(["Pictures"]), "subfolders only, no files, no hidden ones");
        assert_eq!(b["is_root"], true);
        let b = browse_in(Some(&root.join("Pictures").to_string_lossy()), &roots).unwrap();
        assert_eq!(b["crumbs"].as_array().unwrap().len(), 2);
        assert!(browse_in(Some(&d.path().to_string_lossy()), &roots).is_err(), "outside the home folder");
        assert!(browse_in(Some(&root.join("Pictures/../..").to_string_lossy()), &roots).is_err());
    }

    #[test]
    fn a_grant_is_a_folder_inside_the_roots() {
        let (_one, t) = crate::http::tests::node();
        let n = &t.node;
        let (d, root) = tmp();
        std::fs::create_dir(root.join("Games")).unwrap();
        std::fs::create_dir(root.join(".secret")).unwrap();
        std::fs::write(root.join("f"), b"x").unwrap();
        let roots = vec![root.clone()];
        let id = "test-grant-app";
        assert!(grant(n, id, &root.to_string_lossy(), &roots).is_err(), "the whole home folder");
        assert!(grant(n, id, &root.join(".secret").to_string_lossy(), &roots).is_err(), "hidden");
        assert!(grant(n, id, &root.join("f").to_string_lossy(), &roots).is_err(), "a file");
        assert!(grant(n, id, &root.join("missing").to_string_lossy(), &roots).is_err());
        assert!(grant(n, id, &d.path().to_string_lossy(), &roots).is_err(), "outside");
        assert_eq!(grant(n, id, &root.join("Games").to_string_lossy(), &roots).unwrap()["name"], "Games");
        revoke(n, id).unwrap();
    }
}
