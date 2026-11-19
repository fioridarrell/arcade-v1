//! Blobs on the arcade's side (docs/ARCADECHAIN.md §15 step 1; the rules are arcade-arc `blob.rs`).
//!
//! - **The blob store**, `<home>/arc/<net>/blobs/<id>`: the bytes of every blob a block brought, written once when the
//!   block is taken (on any branch). The chain keeps only the ids; a block handed to a peer gets its bytes back from
//!   here ([`attach`]).
//! - **Waiting blobs**: the bytes of blobs that waiting transactions bring, held in memory (at most [`MAX_WAITING`])
//!   until a block brings them or their transactions go. A transaction enters the pool only with them here.
//! - **Over the mesh**, as "ac" frames: `bl {id, seq, end}` + bytes, a waiting blob sent ahead of the transactions that
//!   bring it (the transaction answer `txs` follows).

use arcade_arc::block::Block;
use arcade_arc::merkle::Hash;
use arcade_arc::params::Net;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

/// Most bytes of waiting blobs held in memory.
pub const MAX_WAITING: usize = 64 << 20;

#[derive(Default)]
struct Waiting {
    blobs: HashMap<Hash, Vec<u8>>,
    bytes: usize,
    /// Pieces of blobs on their way from a peer: (peer, id) -> (next piece, bytes so far).
    parts: HashMap<(String, Hash), (u64, Vec<u8>)>,
}

static WAITING: Mutex<Option<Waiting>> = Mutex::new(None);

fn with<R>(f: impl FnOnce(&mut Waiting) -> R) -> R {
    let mut g = WAITING.lock().unwrap();
    f(g.get_or_insert_with(Waiting::default))
}

static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Where the store is: `<home>/arc/<net>/blobs` (set when the chain is defined, installed or borrowed).
pub fn init(dir: PathBuf) {
    let _ = std::fs::create_dir_all(&dir);
    let _ = DIR.set(dir);
}

pub fn dir() -> PathBuf {
    DIR.get().cloned().unwrap_or_default()
}

fn path(id: &Hash) -> PathBuf {
    dir().join(hex::encode(id))
}

/// Hold a waiting blob's bytes (they must be the id's). Whether it is held now.
pub fn hold(net: &Net, bytes: Vec<u8>) -> Option<Hash> {
    if bytes.is_empty() || bytes.len() > arcade_arc::blob::MAX_BLOB_BYTES {
        return None;
    }
    let id = arcade_arc::blob::id_of_bytes(net, &bytes);
    with(|w| {
        if w.blobs.contains_key(&id) {
            return Some(id);
        }
        if w.bytes + bytes.len() > MAX_WAITING {
            return None;
        }
        w.bytes += bytes.len();
        w.blobs.insert(id, bytes);
        Some(id)
    })
}

/// What the store knows of a blob it was brought (kept beside its bytes, and after they go): its class, type, the
/// height and time of the block that brought it, and its release line (its type and its publisher's script).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Meta {
    pub class: u8,
    pub kind: String,
    pub encoding: u8,
    pub height: u64,
    pub time: u64,
    pub line: String,
    /// Kept plain until then, whatever the class: boosts (class 3 records) push it 90 days past their block.
    #[serde(default)]
    pub boosted_until: u64,
}

/// How long a boost keeps a blob plain (the Temporary time: 90 days; local test chains less).
static BOOST_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(90 * 24 * 3600);

pub fn set_boost_secs(s: u64) {
    BOOST_SECS.store(s, std::sync::atomic::Ordering::Relaxed);
}

fn meta_path(id: &Hash) -> PathBuf {
    dir().join(format!("{}.meta", hex::encode(id)))
}

pub fn meta(id: &Hash) -> Option<Meta> {
    serde_json::from_slice(&std::fs::read(meta_path(id)).ok()?).ok()
}

/// Reads a sealed blob-family unit at a height (this arcade's own replica), set when the chain starts.
type SealedUnit = Box<dyn Fn(u8, u64) -> Option<Vec<u8>> + Send + Sync>;
static SEALED: std::sync::OnceLock<SealedUnit> = std::sync::OnceLock::new();

pub fn set_sealed_reader(f: SealedUnit) {
    let _ = SEALED.set(f);
}

/// A blob's bytes: from the store, else from those waiting, else unsealed from this arcade's replica of the blob
/// family (its plain copy dropped, §15 step 3).
pub fn get(id: &Hash) -> Option<Vec<u8>> {
    if let Some(b) = std::fs::read(path(id)).ok().or_else(|| with(|w| w.blobs.get(id).cloned())) {
        return Some(b);
    }
    let m = meta(id)?;
    let net = crate::arcsvc::svc_net()?;
    // From rules version 5 a Temporary blob is sealed in its own family (crate::fogstore::family_of).
    let family = if arcade_arc::params::rules_v5(&net, m.height) { crate::fogstore::family_of(m.class) } else { arcade_arc::segment::FAMILY_BLOBS };
    let unit = SEALED.get()?(family, m.height)?;
    split_unit(&unit).into_iter().find(|b| arcade_arc::blob::id_of_bytes(&net, b) == *id)
}

/// A blob-family unit's blobs (segment::FAMILY_BLOBS: a varint count, then each varint length + bytes).
pub fn split_unit(unit: &[u8]) -> Vec<Vec<u8>> {
    let mut r = arcade_chain::Reader::new(unit);
    let Ok(n) = r.len() else { return Vec::new() };
    let mut out = Vec::new();
    for _ in 0..n {
        let Ok(k) = r.len() else { break };
        let Ok(b) = r.bytes(k) else { break };
        out.push(b.to_vec());
    }
    out
}

// ---------------- the viewer cache (§15 step 4) ----------------

/// The cache's size limit in bytes (Options; 0: off). On by default at 2 GB (Martin 2026-11-14).
static CACHE_LIMIT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(2 << 30);

pub fn set_cache_limit(bytes: u64) {
    CACHE_LIMIT.store(bytes, std::sync::atomic::Ordering::Relaxed);
}

pub fn cache_limit() -> u64 {
    CACHE_LIMIT.load(std::sync::atomic::Ordering::Relaxed)
}

fn cache_dir() -> PathBuf {
    dir().join("cache")
}

/// A blob from the cache (touched, so the cache drops what isn't viewed first).
fn cached(id: &Hash) -> Option<Vec<u8>> {
    let p = cache_dir().join(hex::encode(id));
    let b = std::fs::read(&p).ok()?;
    if let Ok(f) = std::fs::File::options().append(true).open(&p) {
        let _ = f.set_modified(std::time::SystemTime::now());
    }
    Some(b)
}

/// The cache's (files, bytes).
pub fn cache_use() -> (u64, u64) {
    let Ok(rd) = std::fs::read_dir(cache_dir()) else { return (0, 0) };
    rd.flatten().filter_map(|e| e.metadata().ok()).filter(|m| m.is_file()).fold((0, 0), |(n, b), m| (n + 1, b + m.len()))
}

/// Keep a fetched blob in the cache, then drop the least recently viewed while over the limit.
fn cache_put(id: &Hash, bytes: &[u8]) {
    let limit = cache_limit();
    if limit == 0 || bytes.len() as u64 > limit {
        return;
    }
    let d = cache_dir();
    let _ = std::fs::create_dir_all(&d);
    let p = d.join(hex::encode(id));
    if std::fs::write(p.with_extension("in"), bytes).and_then(|_| std::fs::rename(p.with_extension("in"), &p)).is_err() {
        return;
    }
    let Ok(rd) = std::fs::read_dir(&d) else { return };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd.flatten()
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            if !m.is_file() {
                return None;
            }
            Some((m.modified().ok()?, m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort_by_key(|f| f.0);
    for (_, len, path) in files {
        if total <= limit {
            break;
        }
        let _ = std::fs::remove_file(path);
        total -= len;
    }
}

/// A blob for a viewer: here (stored, waiting, sealed, cached), else fetched from the arcades this one is linked to and
/// kept in the cache, so a blob many view spreads to many arcades and leaves them when nobody views it any more.
pub fn view(id: &Hash) -> Option<Vec<u8>> {
    if let Some(b) = get(id).or_else(|| cached(id)) {
        touch(id);
        return Some(b);
    }
    // Installed: the arcades its chain is linked to. Only borrowed (no chain running here): the arcades lending it.
    // Then any other arcade linked now (one that viewed the blob keeps it and serves it, borrowing or not).
    let (net, mut peers) = match crate::arcsvc::svc_net() {
        Some(net) => (net, crate::arcsvc::linked_peers()),
        None => {
            let (net, ..) = crate::arcsvc::defined_net()?;
            (net, crate::arcsvc::defined().map(|p| crate::borrow::linked_lenders(p.key)).unwrap_or_default())
        }
    };
    // At most a few of those: each that has no answer costs its wait.
    let extra: Vec<String> = crate::crvia::linked_ids().into_iter().filter(|id| !peers.contains(id)).take(OTHER_PEERS).collect();
    peers.extend(extra);
    // First, the arcades that announce holding it (crate::fogstore), linked or not yet (asked for a link).
    let mut holders: Vec<String> = Vec::new();
    for h in crate::fogstore::holders_now(id) {
        if crate::crvia::linked(&h) {
            holders.push(h);
        } else {
            crate::fogstore::want_link(&h);
        }
    }
    peers.retain(|p| !holders.contains(p));
    holders.extend(peers);
    let peers = holders;
    for peer in peers {
        let got = fetch_from(&peer, id);
        if let Err(e) = &got {
            eprintln!("arc: blob {} not fetched from {}: {e}", &hex::encode(id)[..12], &peer[..peer.len().min(16)]);
        }
        if let Ok((b, kind, gz)) = got {
            if arcade_arc::blob::id_of_bytes(&net, &b) == *id {
                cache_put(id, &b);
                if !kind.is_empty() {
                    let _ = std::fs::write(cache_dir().join(format!("{}.kind", hex::encode(id))), &kind);
                }
                if gz {
                    let _ = std::fs::write(cache_dir().join(format!("{}.gz", hex::encode(id))), b"1");
                }
                return Some(b);
            }
        }
    }
    None
}

/// A blob's content type: from its record (the store's meta), else what the arcade it was fetched from said.
pub fn kind_of(id: &Hash) -> Option<String> {
    meta(id).map(|m| m.kind).or_else(|| std::fs::read_to_string(cache_dir().join(format!("{}.kind", hex::encode(id)))).ok())
}

/// Whether a blob is stored gzipped (its record's encoding; for a fetched one, what the arcade it came from said), so a
/// viewer is told to undo it.
pub fn gzipped(id: &Hash) -> bool {
    match meta(id) {
        Some(m) => m.encoding == arcade_arc::blob::Encoding::Gzip as u8,
        None => cache_dir().join(format!("{}.gz", hex::encode(id))).exists(),
    }
}

/// Other linked arcades a blob is asked of once its chain's own peers (or lenders) don't have it.
const OTHER_PEERS: usize = 6;

/// Bytes of a blob per answer piece (a mesh frame is 64 KiB at most, head and blob).
const PIECE: u64 = 48_000;
/// Pieces asked for at once.
const WINDOW: usize = 16;

fn fetch_from(peer: &str, id: &Hash) -> Result<(Vec<u8>, String, bool), String> {
    use crate::crsource::Pipe;
    use arcade_mesh::link::Frame;
    use std::time::{Duration, Instant};
    let mut out: Vec<u8> = Vec::new();
    let mut asked: std::collections::VecDeque<(u64, crate::crvia::Via)> = std::collections::VecDeque::new();
    let (mut next, mut total) = (0u64, None::<u64>);
    loop {
        while asked.len() < WINDOW && total.is_none_or(|t| next < t) {
            let mut via = crate::crvia::Via::open(peer)?;
            via.send(&crate::arcsvc::keeper_frame("bq", serde_json::json!({"id": via.id, "b": hex::encode(id), "at": next})))?;
            asked.push_back((next, via));
            next += PIECE;
            if total.is_none() {
                break; // the first answer says how big it is
            }
        }
        let Some((at, mut via)) = asked.pop_front() else { break };
        let f: Frame = via.recv(Instant::now() + Duration::from_secs(20))?;
        if let Some(e) = f.head["err"].as_str() {
            return Err(e.to_string());
        }
        let len = f.head["len"].as_u64().ok_or("no length")?;
        let kind = f.head["kind"].as_str().unwrap_or("").chars().take(arcade_arc::blob::MAX_TYPE).collect::<String>();
        if len as usize > arcade_arc::blob::MAX_BLOB_BYTES {
            return Err("too big".into());
        }
        total.get_or_insert(len);
        if at != out.len() as u64 || f.blob.len() as u64 > PIECE {
            return Err("pieces out of order".into());
        }
        out.extend_from_slice(&f.blob);
        if out.len() as u64 >= len {
            out.truncate(len as usize);
            return Ok((out, kind, f.head["gz"] == true));
        }
    }
    Err("no answer".into())
}

/// A `bq` frame: a piece of a blob this arcade has (stored, sealed or cached), answered `a` over the same link.
pub fn serve_piece(link: &mut arcade_mesh::link::Link, head: &serde_json::Value) -> Result<(), arcade_mesh::link::LinkError> {
    let id_n = head["id"].as_u64().unwrap_or(0);
    let at = head["at"].as_u64().unwrap_or(0);
    let bytes = head["b"].as_str().and_then(|s| hex::decode(s).ok()).and_then(|b| <Hash>::try_from(b).ok()).and_then(|id| get(&id).or_else(|| cached(&id)));
    let kind = head["b"].as_str().and_then(|s| hex::decode(s).ok()).and_then(|b| <Hash>::try_from(b).ok()).and_then(|id| kind_of(&id));
    let mut f = crate::arcsvc::keeper_frame("a", serde_json::json!({"id": id_n}));
    match bytes {
        Some(b) => {
            // Asked for: it counts as used here (fog storage keeps what is viewed or served, crate::fogstore).
            if at == 0 {
                if let Some(id) = head["b"].as_str().and_then(|s| hex::decode(s).ok()).and_then(|b| <Hash>::try_from(b).ok()) {
                    touch(&id);
                }
            }
            f.head["len"] = serde_json::json!(b.len());
            f.head["kind"] = serde_json::json!(kind.unwrap_or_default());
            if head["b"].as_str().and_then(|s| hex::decode(s).ok()).and_then(|b| <Hash>::try_from(b).ok()).is_some_and(|id| gzipped(&id)) {
                f.head["gz"] = serde_json::json!(true);
            }
            let from = (at as usize).min(b.len());
            f.blob = b[from..(from + PIECE as usize).min(b.len())].to_vec();
        }
        None => f.head["err"] = serde_json::json!("not here"),
    }
    link.send(&f)
}

/// Whether a blob's plain copy is here (not only sealed).
pub fn plain_here(id: &Hash) -> bool {
    path(id).exists()
}

/// Drop a blob's plain copy (its meta stays, so a sealed copy here is still found).
pub fn drop_plain(id: &Hash) -> u64 {
    let n = std::fs::metadata(path(id)).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_file(path(id));
    n
}

/// Forget a blob altogether (a Temporary one past its time).
pub fn forget(id: &Hash) -> u64 {
    let n = drop_plain(id);
    let _ = std::fs::remove_file(meta_path(id));
    n
}

/// Every blob the store knows (by its meta).
pub fn known() -> Vec<(Hash, Meta)> {
    let Ok(rd) = std::fs::read_dir(dir()) else { return Vec::new() };
    rd.flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            let id: Hash = hex::decode(n.strip_suffix(".meta")?).ok()?.try_into().ok()?;
            Some((id, meta(&id)?))
        })
        .collect()
}

/// Whether the bytes of `id` are here (stored or waiting).
pub fn here(id: &Hash) -> bool {
    path(id).exists() || with(|w| w.blobs.contains_key(id))
}

/// A block was taken: its blobs into the store (checked by the chain already), out of those waiting.
pub fn keep_block(net: &Net, block: &Block) {
    if block.blob_ids.is_empty() {
        return;
    }
    let _ = std::fs::create_dir_all(dir());
    // Each brought blob's record (the first naming it in the block) for its meta.
    let mut recs: HashMap<Hash, (arcade_arc::blob::Record, Vec<u8>)> = HashMap::new();
    for tx in block.txs.iter().skip(1) {
        let publisher = tx.outputs.iter().find(|o| !arcade_arc::script::unspendable(&o.script_pubkey)).map(|o| o.script_pubkey.clone()).unwrap_or_default();
        for r in arcade_arc::blob::records(tx) {
            recs.entry(r.id(net)).or_insert((r, publisher.clone()));
        }
    }
    // Boosts in this block push the blobs they name past their time (their meta, made below for a blob a boost brings).
    let boost_until = block.header.time + BOOST_SECS.load(std::sync::atomic::Ordering::Relaxed);
    for tx in block.txs.iter().skip(1) {
        for r in arcade_arc::blob::records(tx).into_iter().filter(|r| r.class == arcade_arc::blob::Class::Boost) {
            let id = r.id(net);
            if let Some(mut m) = meta(&id) {
                if m.boosted_until < boost_until {
                    m.boosted_until = boost_until;
                    let _ = std::fs::write(meta_path(&id), serde_json::to_vec(&m).unwrap_or_default());
                }
            }
        }
    }
    for (id, (r, publisher)) in &recs {
        if block.blob_ids.contains(id) && !meta_path(id).exists() {
            use sha2::Digest;
            let line = hex::encode(&sha2::Sha256::digest([r.kind.as_bytes(), b"\0", publisher].concat())[..16]);
            let boosted_until = if r.class == arcade_arc::blob::Class::Boost { boost_until } else { 0 };
            let m = Meta { class: r.class as u8, kind: r.kind.clone(), encoding: r.encoding as u8, height: block.header.height, time: block.header.time, line, boosted_until };
            let _ = std::fs::write(meta_path(id), serde_json::to_vec(&m).unwrap_or_default());
        }
    }
    for (id, bytes) in block.blob_ids.iter().zip(&block.blobs) {
        if bytes.is_empty() {
            continue; // an old block's blob that came without its bytes
        }
        let p = path(id);
        if !p.exists() {
            let tmp = p.with_extension("in");
            if std::fs::write(&tmp, bytes).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
                eprintln!("arc: couldn't keep blob {}", hex::encode(&id[..8]));
            }
        }
        with(|w| {
            if let Some(b) = w.blobs.remove(id) {
                w.bytes -= b.len();
            }
        });
    }
}

/// A block as the chain keeps it, with its blobs' bytes again, to hand on. A blob gone from here goes without its bytes
/// when the block is older than the window (the chain allows that); in a recent block it can't: None.
pub fn attach(block: &Block) -> Option<Block> {
    let mut b = block.clone();
    if b.blobs.is_empty() && !b.blob_ids.is_empty() {
        // From rules version 5 any block may go without bytes this arcade doesn't keep (crate::fogstore).
        let old = crate::arcsvc::svc_net().is_some_and(|n| b.header.time.saturating_add(n.blob_window) <= arcade_app::now() || arcade_arc::params::rules_v5(&n, b.header.height));
        let mut blobs = Vec::with_capacity(b.blob_ids.len());
        for id in &b.blob_ids {
            match get(id) {
                Some(x) => blobs.push(x),
                None if old => blobs.push(Vec::new()),
                None => return None,
            }
        }
        b.blobs = blobs;
    }
    Some(b)
}

/// Waiting blobs no waiting transaction names any more (and none the chain lacks) go.
pub fn forget_except(keep: &std::collections::HashSet<Hash>) {
    with(|w| {
        let gone: Vec<Hash> = w.blobs.keys().filter(|k| !keep.contains(*k)).copied().collect();
        for k in gone {
            if let Some(b) = w.blobs.remove(&k) {
                w.bytes -= b.len();
            }
        }
    });
}

/// A `bl` frame: a piece of a waiting blob from `peer`. When the last piece is in, the blob is held.
pub fn on_piece(net: &Net, peer: &str, head: &serde_json::Value, blob: &[u8]) {
    let Some(id) = head["id"].as_str().and_then(|s| hex::decode(s).ok()).and_then(|b| <Hash>::try_from(b).ok()) else { return };
    let seq = head["seq"].as_u64().unwrap_or(u64::MAX);
    let done = with(|w| {
        let key = (peer.to_string(), id);
        let e = w.parts.entry(key.clone()).or_insert((0, Vec::new()));
        if e.0 != seq || e.1.len() + blob.len() > arcade_arc::blob::MAX_BLOB_BYTES {
            w.parts.remove(&key);
            return None;
        }
        e.0 += 1;
        e.1.extend_from_slice(blob);
        if head["end"] == true {
            w.parts.remove(&key).map(|(_, b)| b)
        } else {
            None
        }
    });
    if let Some(bytes) = done {
        if hold(net, bytes) != Some(id) {
            eprintln!("arc: a blob from {} wasn't its id's, or there is no room", &peer[..peer.len().min(12)]);
        }
    }
}

// ---------------- files bigger than a block (Martin 2026-11-14: a full-length movie) ----------------

/// The type of a file manifest: a blob naming a file's parts in order (`scripts/file-publish.py` makes it).
pub const FILE_MANIFEST: &str = "application/vnd.arcade.file+json";
/// Most bytes of one file (Martin 2026-11-14: up to 1 GB, for whoever pays its blobs).
pub const MAX_FILE_BYTES: u64 = 1 << 30;

/// A file made of blobs: `{"arcade": "file", "type", "size", "sha256", "parts": [[blob id, length], ...]}`.
pub struct FileManifest {
    pub kind: String,
    pub size: u64,
    pub parts: Vec<(Hash, u64)>,
}

/// The file manifest blob `id` is, if it is one (its parts add up to its size, each a blob the chain holds).
pub fn file_manifest(id: &Hash) -> Option<FileManifest> {
    if kind_of(id).as_deref() != Some(FILE_MANIFEST) {
        return None;
    }
    let v: serde_json::Value = serde_json::from_slice(&view(id)?).ok()?;
    if v["arcade"] != "file" {
        return None;
    }
    let mut parts = Vec::new();
    for p in v["parts"].as_array()? {
        let h: Hash = hex::decode(p[0].as_str()?).ok()?.try_into().ok()?;
        parts.push((h, p[1].as_u64()?));
    }
    let size = v["size"].as_u64()?;
    if size > MAX_FILE_BYTES || parts.iter().map(|p| p.1).sum::<u64>() != size || !parts.iter().all(|p| crate::arcsvc::holds_blob(&p.0)) {
        return None;
    }
    Some(FileManifest { kind: v["type"].as_str().unwrap_or("application/octet-stream").chars().take(arcade_arc::blob::MAX_TYPE).collect(), size, parts })
}

/// Bytes `from..=to` of a file, part by part, each handed to `out` as it is read (fetched from linked arcades when it
/// isn't here, and kept in the viewer cache, so a film many watch spreads as any blob does). False when a part can't
/// be had.
pub fn file_range(f: &FileManifest, from: u64, to: u64, mut out: impl FnMut(&[u8]) -> bool) -> bool {
    let mut at = 0u64;
    for (id, len) in &f.parts {
        let (start, end) = (at, at + len);
        at = end;
        if end <= from || start > to {
            continue;
        }
        let Some(b) = view(id) else { return false };
        if b.len() as u64 != *len {
            return false;
        }
        let a = from.saturating_sub(start) as usize;
        let z = ((to + 1).min(end) - start) as usize;
        if !out(&b[a..z]) {
            return false;
        }
    }
    true
}

// ---------------- fog storage's view of the store (crate::fogstore) ----------------

fn mine_path(id: &Hash) -> PathBuf {
    dir().join(format!("{}.mine", hex::encode(id)))
}

/// Mark a blob as published by this arcade: it keeps it for good (crate::fogstore).
pub fn mark_mine(id: &Hash) {
    let _ = std::fs::create_dir_all(dir());
    let _ = std::fs::write(mine_path(id), b"");
}

/// Whether this arcade published the blob.
pub fn is_mine(id: &Hash) -> bool {
    mine_path(id).exists()
}

/// Mark a blob as just used here (viewed or served): its plain copy's or cached copy's time.
pub fn touch(id: &Hash) {
    for p in [path(id), cache_dir().join(hex::encode(id))] {
        if let Ok(f) = std::fs::File::options().append(true).open(&p) {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
    }
}

fn mtime(p: &std::path::Path) -> Option<u64> {
    std::fs::metadata(p).ok()?.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs())
}

/// When the blob was last viewed or served here (its plain copy's time; one fetched to keep a segment says 0).
pub fn last_use(id: &Hash) -> Option<u64> {
    mtime(&path(id))
}

/// The ids whose bytes are here: plain copies and the cache.
pub fn held_ids() -> Vec<Hash> {
    let mut out = Vec::new();
    for d in [dir(), cache_dir()] {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.len() == 64 && e.metadata().is_ok_and(|m| m.is_file()) {
                if let Ok(Ok(id)) = hex::decode(&n).map(<Hash>::try_from) {
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
            }
        }
    }
    out
}

/// Bytes of the blob here (plain or cached), 0 when it isn't.
pub fn size_here(id: &Hash) -> u64 {
    std::fs::metadata(path(id)).or_else(|_| std::fs::metadata(cache_dir().join(hex::encode(id)))).map(|m| m.len()).unwrap_or(0)
}

/// A blob's length as its block's record says (from its meta; else the bytes here).
pub fn len_of(id: &Hash) -> Option<u64> {
    Some(size_here(id)).filter(|n| *n > 0)
}

/// Drop cached copies not viewed or served since `before` (seconds since 1970). Bytes freed.
pub fn cache_drop_idle(before: u64) -> u64 {
    let Ok(rd) = std::fs::read_dir(cache_dir()) else { return 0 };
    let mut freed = 0;
    for e in rd.flatten() {
        let n = e.file_name().to_string_lossy().to_string();
        if n.len() != 64 {
            continue;
        }
        if mtime(&e.path()).is_some_and(|t| t < before) {
            freed += e.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = std::fs::remove_file(e.path());
            let _ = std::fs::remove_file(cache_dir().join(format!("{n}.kind")));
            let _ = std::fs::remove_file(cache_dir().join(format!("{n}.gz")));
        }
    }
    freed
}

/// Fetch a blob from whoever has it: the arcades announcing they hold it (crate::fogstore::holders), then the arcades
/// this one is linked to. An announcing arcade without a link yet is asked for one (through a relay, or UDP), for a
/// later round. The bytes, checked against the id.
pub fn fetch_any(node: &crate::node::Node, id: &Hash) -> Option<(Vec<u8>, String, bool)> {
    let net = crate::arcsvc::svc_net().or_else(|| crate::arcsvc::defined_net().map(|d| d.0))?;
    let mut peers = crate::fogstore::holders(node, id);
    for p in crate::arcsvc::linked_peers() {
        if !peers.contains(&p) {
            peers.push(p);
        }
    }
    for peer in &peers {
        if !crate::crvia::linked(peer) {
            crate::udpsvc::want(peer);
            let _ = crate::relaysvc::want(node, peer);
            continue;
        }
        if let Ok((b, kind, gz)) = fetch_from(peer, id) {
            if arcade_arc::blob::id_of_bytes(&net, &b) == *id {
                return Some((b, kind, gz));
            }
        }
    }
    None
}

/// Fetch a blob a kept segment lacks and keep it in the store (not counted as viewed). Whether it's here now.
pub fn fetch_keep(node: &crate::node::Node, id: &Hash) -> bool {
    let Some((b, ..)) = fetch_any(node, id) else { return false };
    let p = path(id);
    let tmp = p.with_extension("in");
    if std::fs::write(&tmp, &b).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
        return false;
    }
    if let Ok(f) = std::fs::File::options().append(true).open(&p) {
        let _ = f.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1));
    }
    true
}
