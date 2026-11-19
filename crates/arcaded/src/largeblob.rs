//! Large blobs: one blob of up to 100 MB on the ARCADE chain (Martin 2026-11-18), with no consensus change.
//!
//! - **A large blob is a manifest.** An ordinary blob of type [`crate::arcblob::FILE_MANIFEST`] whose JSON body gives
//!   the total length, the content type, the encoding, the class, an overall **root** (the RFC 6962 Merkle root over
//!   all the bytes in the same 127-byte leaves a small blob's root uses) and the ordered parts `[[blob id, length], ...]`.
//!   Each part is an ordinary blob of at most [`PART_BYTES`] (one block's 4 MB blob budget, cut on a leaf boundary so a
//!   part's leaves are the whole's leaves) of the same class. The id people use is the manifest's blob id: it names the
//!   root and every part, so it is as checkable as a small blob's.
//!   The manifests `scripts/file-publish.py` makes (no root, up to 1 GB) are still read and served as before; only one
//!   with a root is a large blob, and it is at most [`MAX_LARGE_BYTES`].
//! - **Publishing** (`arc.blob_publish_large`, and `arc.blob_publish` above 4 MB): the file is cut into parts, the fee
//!   and the blocks it takes are quoted, then the parts go out one transaction each, at most [`WINDOW`] waiting at once
//!   (a block takes one 4 MB part; the pool's 64 MB of waiting blobs is shared by everyone), each spending the change
//!   of the one before while it waits. The manifest goes last, once every part is in a block or waiting. A journal in
//!   `<home>/arc/<net>/large/` keeps each part's signed transaction before it is sent, so a restart sends the same
//!   transactions again and never pays twice. A part the chain already holds isn't paid again.
//! - **Reading**: `/content/<manifest id>` serves the whole, with byte ranges, part by part (crate::arcblob::file_range):
//!   a part not here is fetched from whoever has it over the mesh and kept like any viewed blob, each checked against
//!   its id; the whole is checked against the root once all its parts are here.
//! - **Fog** (crate::fogstore): a large blob's parts live and fade together (a view of one is a view of all), keepers
//!   keep whole large blobs, and the storage reads list a large blob as one item with its part count.

use crate::node::Node;
use arcade_arc::blob::{self, Class, Encoding, LEAF, MAX_BLOB_BYTES};
use arcade_arc::merkle::{self, Hash};
use serde_json::{json, Value};
use sha2::Digest;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Mutex;

/// Most bytes of one large blob.
pub const MAX_LARGE_BYTES: u64 = 100_000_000;
/// Bytes of each part but the last: the most whole leaves in one block's blob budget (31,496 leaves, 3,999,992 bytes).
pub const PART_BYTES: usize = MAX_BLOB_BYTES / LEAF * LEAF;
/// Most parts of one large blob.
pub const MAX_PARTS: usize = (MAX_LARGE_BYTES as usize).div_ceil(PART_BYTES);
/// The type of each part's record.
pub const PART_TYPE: &str = "application/vnd.arcade.part";
/// Parts of one large blob waiting in the pool at once. A block takes one, so more wouldn't be faster; this leaves the
/// pool's room (crate::arcblob::MAX_WAITING, at every arcade that relays them) to everyone else.
pub const WINDOW: usize = 3;
/// Most bytes of a manifest that fog storage fetches and keeps everywhere (so every arcade knows which parts belong
/// together): a 26-part manifest is about 4 kB.
pub const MANIFEST_KEEP_MAX: u64 = 64 << 10;
/// Most file manifests without a root (scripts/file-publish.py) a manifest may describe.
const MAX_FILE_BYTES: u64 = 1 << 30;

// ---------------- the manifest ----------------

/// A manifest: a large blob (with `root`) or an older file of blobs (without).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub kind: String,
    pub size: u64,
    pub parts: Vec<(Hash, u64)>,
    /// The overall root (a large blob); None for an older file manifest.
    pub root: Option<Hash>,
    pub class: u8,
    pub encoding: u8,
    pub sha256: String,
    pub name: String,
}

impl Manifest {
    /// Whether it is a large blob (it names its overall root).
    pub fn large(&self) -> bool {
        self.root.is_some()
    }

    pub fn gzip(&self) -> bool {
        self.encoding == Encoding::Gzip as u8
    }

    /// Its body, as the manifest blob holds it.
    pub fn to_bytes(&self) -> Vec<u8> {
        let parts: Vec<Value> = self.parts.iter().map(|(id, n)| json!([hex::encode(id), n])).collect();
        let mut v = json!({"arcade": "file", "type": self.kind, "size": self.size, "sha256": self.sha256, "parts": parts});
        if let Some(r) = self.root {
            v["v"] = json!(2);
            v["root"] = json!(hex::encode(r));
            v["leaf"] = json!(LEAF);
            v["class"] = json!(self.class);
            v["encoding"] = json!(self.encoding);
        }
        if !self.name.is_empty() {
            v["name"] = json!(self.name);
        }
        v.to_string().into_bytes()
    }

    /// A manifest from a blob's body, if it is a well-formed one: its parts add up to its size; a large blob's are 1 to
    /// 4 MB each, at most [`MAX_PARTS`], at most [`MAX_LARGE_BYTES`] in all, with a known class and encoding.
    pub fn parse(bytes: &[u8]) -> Option<Manifest> {
        let v: Value = serde_json::from_slice(bytes).ok()?;
        if v["arcade"] != "file" {
            return None;
        }
        let mut parts = Vec::new();
        for p in v["parts"].as_array()? {
            let h: Hash = hex::decode(p[0].as_str()?).ok()?.try_into().ok()?;
            parts.push((h, p[1].as_u64()?));
        }
        let size = v["size"].as_u64()?;
        if parts.iter().map(|p| p.1).sum::<u64>() != size {
            return None;
        }
        let root: Option<Hash> = match v.get("root") {
            None | Some(Value::Null) => None,
            Some(r) => Some(hex::decode(r.as_str()?).ok()?.try_into().ok()?),
        };
        let class = v["class"].as_u64().unwrap_or(0);
        let encoding = v["encoding"].as_u64().unwrap_or(0);
        if root.is_some() {
            let ok = size > 0
                && size <= MAX_LARGE_BYTES
                && !parts.is_empty()
                && parts.len() <= MAX_PARTS
                && parts.iter().all(|p| p.1 > 0 && p.1 <= MAX_BLOB_BYTES as u64)
                && matches!(Class::from_u8(class as u8), Some(Class::Permanent | Class::Release | Class::Temporary))
                && class < 256
                && Encoding::from_u8(encoding as u8).is_some()
                && encoding < 256;
            if !ok {
                return None;
            }
        } else if size > MAX_FILE_BYTES {
            return None;
        }
        Some(Manifest {
            kind: v["type"].as_str().unwrap_or("application/octet-stream").chars().take(blob::MAX_TYPE).collect(),
            size,
            parts,
            root,
            class: class as u8,
            encoding: encoding as u8,
            sha256: v["sha256"].as_str().unwrap_or("").chars().take(64).collect(),
            name: v["name"].as_str().unwrap_or("").chars().take(200).collect(),
        })
    }
}

/// The overall root, fed the bytes in any pieces: the same as `blob::root_of` over all of them at once.
pub struct RootStream {
    b: merkle::RootBuilder,
    carry: Vec<u8>,
    len: u64,
}

impl Default for RootStream {
    fn default() -> Self {
        RootStream { b: merkle::RootBuilder::new(), carry: Vec::with_capacity(LEAF), len: 0 }
    }
}

impl RootStream {
    pub fn push(&mut self, mut bytes: &[u8]) {
        self.len += bytes.len() as u64;
        if !self.carry.is_empty() {
            let take = (LEAF - self.carry.len()).min(bytes.len());
            self.carry.extend_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            if self.carry.len() == LEAF {
                self.b.push_leaf(&self.carry);
                self.carry.clear();
            }
        }
        let whole = bytes.len() / LEAF * LEAF;
        for leaf in bytes[..whole].chunks(LEAF) {
            self.b.push_leaf(leaf);
        }
        self.carry.extend_from_slice(&bytes[whole..]);
    }

    /// The root and the length.
    pub fn finish(mut self) -> (Hash, u64) {
        if !self.carry.is_empty() {
            self.carry.resize(LEAF, 0);
            self.b.push_leaf(&self.carry);
        }
        (self.b.finish().unwrap_or([0; 32]), self.len)
    }
}

/// The parts bytes `from..=to` of the whole lie in: (part index, first byte in it, end in it, exclusive).
pub fn ranges(parts: &[(Hash, u64)], from: u64, to: u64) -> Vec<(usize, u64, u64)> {
    let mut out = Vec::new();
    let mut at = 0u64;
    for (i, (_, len)) in parts.iter().enumerate() {
        let (start, end) = (at, at + len);
        at = end;
        if end <= from || start > to {
            continue;
        }
        out.push((i, from.saturating_sub(start), (to + 1).min(end) - start));
    }
    out
}

/// A file cut into parts: the parts' ids and lengths, the overall root and sha256, and its manifest (not yet knowing
/// its name, type, class and encoding: see [`plan`]).
pub struct Cut {
    pub parts: Vec<(Hash, u64)>,
    pub root: Hash,
    pub size: u64,
    pub sha256: String,
}

/// Cut `src` into parts, handing each to `each` (index, bytes) as it is read: 4 MB in memory at a time.
pub fn cut(net: &arcade_arc::params::Net, src: &mut dyn std::io::Read, mut each: impl FnMut(usize, &[u8]) -> Result<(), String>) -> Result<Cut, String> {
    let (mut parts, mut root, mut sha) = (Vec::new(), RootStream::default(), sha2::Sha256::new());
    let mut buf = vec![0u8; PART_BYTES];
    loop {
        let mut n = 0;
        while n < PART_BYTES {
            let k = src.read(&mut buf[n..]).map_err(|e| e.to_string())?;
            if k == 0 {
                break;
            }
            n += k;
        }
        if n == 0 {
            break;
        }
        let b = &buf[..n];
        if (parts.len() as u64) * PART_BYTES as u64 + n as u64 > MAX_LARGE_BYTES {
            return Err(format!("a large blob is at most {MAX_LARGE_BYTES} bytes"));
        }
        root.push(b);
        sha.update(b);
        each(parts.len(), b)?;
        parts.push((blob::id_of_bytes(net, b), n as u64));
        if n < PART_BYTES {
            break;
        }
    }
    let (root, size) = root.finish();
    if size == 0 {
        return Err("no bytes".into());
    }
    Ok(Cut { parts, root, size, sha256: hex::encode(sha.finalize()) })
}

// ---------------- which blobs belong together (for fog storage) ----------------

#[derive(Default)]
struct Groups {
    /// A part's (or a manifest's own) manifest id.
    of: HashMap<Hash, Hash>,
    manifests: HashMap<Hash, Manifest>,
}

static GROUPS: Mutex<Option<Groups>> = Mutex::new(None);

/// Remember a manifest (its parts belong to it).
pub fn note(id: Hash, m: Manifest) {
    let mut g = GROUPS.lock().unwrap();
    let g = g.get_or_insert_with(Groups::default);
    g.of.insert(id, id);
    for (p, _) in &m.parts {
        g.of.insert(*p, id);
    }
    g.manifests.insert(id, m);
}

/// Read again every manifest whose bytes are here (fog's round does, every few minutes).
pub fn refresh() {
    let mut fresh = Groups::default();
    for (id, m) in crate::arcblob::known() {
        if m.kind != crate::arcblob::FILE_MANIFEST {
            continue;
        }
        let Some(bytes) = crate::arcblob::local(&id) else { continue };
        let Some(man) = Manifest::parse(&bytes) else { continue };
        fresh.of.insert(id, id);
        for (p, _) in &man.parts {
            fresh.of.insert(*p, id);
        }
        fresh.manifests.insert(id, man);
    }
    *GROUPS.lock().unwrap() = Some(fresh);
}

/// The manifest a blob belongs to (a part's, or a manifest itself), and that manifest.
pub fn group_of(id: &Hash) -> Option<(Hash, Manifest)> {
    let g = GROUPS.lock().unwrap();
    let g = g.as_ref()?;
    let m = g.of.get(id)?;
    Some((*m, g.manifests.get(m)?.clone()))
}

/// The other blobs of the large blob (or file) `id` is part of: its manifest and the other parts. Empty for a lone blob.
pub fn siblings(id: &Hash) -> Vec<Hash> {
    let Some((m, man)) = group_of(id) else { return Vec::new() };
    std::iter::once(m).chain(man.parts.iter().map(|p| p.0)).filter(|x| x != id).collect()
}

/// Every manifest known here: (id, manifest).
pub fn all() -> Vec<(Hash, Manifest)> {
    GROUPS.lock().unwrap().as_ref().map(|g| g.manifests.iter().map(|(k, v)| (*k, v.clone())).collect()).unwrap_or_default()
}

// ---------------- the whole against its root ----------------

static CHECKED: Mutex<Option<HashMap<Hash, bool>>> = Mutex::new(None);

/// Whether a large blob's parts make the root it names: Some once known (all its parts were here), None while some
/// part isn't here yet. An older file manifest (no root) is Some(true).
pub fn whole_ok(id: &Hash, m: &Manifest) -> Option<bool> {
    let Some(root) = m.root else { return Some(true) };
    if let Some(r) = CHECKED.lock().unwrap().as_ref().and_then(|c| c.get(id).copied()) {
        return Some(r);
    }
    if !m.parts.iter().all(|p| crate::arcblob::bytes_here(&p.0)) {
        return None;
    }
    let mut rs = RootStream::default();
    for (p, len) in &m.parts {
        let b = crate::arcblob::local(p)?;
        if b.len() as u64 != *len {
            return None;
        }
        rs.push(&b);
    }
    let ok = rs.finish() == (root, m.size);
    remember(id, ok);
    Some(ok)
}

/// Remember whether a large blob's whole made its root (as a full read found).
pub fn remember(id: &Hash, ok: bool) {
    CHECKED.lock().unwrap().get_or_insert_with(HashMap::new).insert(*id, ok);
    if !ok {
        eprintln!("arc: large blob {}: its parts don't make the root it names", &hex::encode(id)[..12]);
    }
}

// ---------------- publishing ----------------

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Piece {
    id: String,
    len: u64,
    /// Its signed transaction (kept before it is sent), its id and fee.
    #[serde(default)]
    raw: String,
    #[serde(default)]
    txid: String,
    #[serde(default)]
    fee: u64,
    /// Failed sends of `raw` since it last went in.
    #[serde(default)]
    tries: u32,
    /// "" (not sent), "waiting", "block".
    #[serde(default)]
    state: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct Job {
    id: String,
    kind: String,
    class: u8,
    encoding: u8,
    size: u64,
    root: String,
    parts: Vec<Piece>,
    manifest: Piece,
    /// Transactions of this job that can never go in a block (rebuilt: a new one spends their coins).
    #[serde(default)]
    dead: Vec<String>,
    /// "publishing", "done".
    state: String,
    #[serde(default)]
    note: String,
    started: u64,
    #[serde(default)]
    updated: u64,
    #[serde(default)]
    done_at: u64,
    #[serde(default)]
    quote: Value,
    /// Times a part's transaction was sent again (after a restart, say).
    #[serde(default)]
    resent: u32,
    /// Times one was rebuilt.
    #[serde(default)]
    rebuilt: u32,
}

fn jobs_dir() -> PathBuf {
    crate::arcblob::dir().parent().map(|p| p.join("large")).unwrap_or_default()
}

fn job_path(id: &str) -> PathBuf {
    jobs_dir().join(format!("{id}.json"))
}

fn part_path(id: &str, i: usize) -> PathBuf {
    jobs_dir().join(id).join(format!("{i}"))
}

fn manifest_path(id: &str) -> PathBuf {
    jobs_dir().join(id).join("m")
}

fn save(job: &mut Job) {
    job.updated = arcade_app::now();
    let p = job_path(&job.id);
    let tmp = p.with_extension("in");
    if std::fs::write(&tmp, serde_json::to_vec(job).unwrap_or_default()).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
        eprintln!("arc: large blob {}: couldn't write its journal", &job.id[..12]);
    }
}

fn load(id: &str) -> Option<Job> {
    serde_json::from_slice(&std::fs::read(job_path(id)).ok()?).ok()
}

fn jobs() -> Vec<Job> {
    let Ok(rd) = std::fs::read_dir(jobs_dir()) else { return Vec::new() };
    let mut out: Vec<Job> = rd.flatten().filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").and_then(load)).collect();
    out.sort_by_key(|j| j.started);
    out
}

/// One job at a time is stepped (the worker, and a publish that starts one).
static STEP: Mutex<()> = Mutex::new(());

/// The fee and the time a large blob takes, before it is sent.
fn quote(parts: &[(Hash, u64)], manifest_len: u64, class: Class) -> Result<Value, String> {
    let p = crate::arcsvc::defined().filter(|p| crate::arcsvc::runs(p)).ok_or("the ARCADE chain isn't running here")?;
    let net = crate::arcsvc::svc_net().ok_or("the ARCADE chain isn't running here")?;
    let tip = crate::arcsvc::with_chain(|_, c| c.tip().height).unwrap_or(0);
    let new: Vec<&(Hash, u64)> = parts.iter().filter(|x| !crate::arcsvc::holds_blob(&x.0)).collect();
    let blob_fee: u64 = new.iter().map(|x| blob::fee_at(&net, tip + 1, &[(class, x.1)])).sum::<u64>() + blob::fee_at(&net, tip + 1, &[(class, manifest_len)]);
    let fp = crate::coinsend::fee_policy(p);
    // One input, a record and the change each (a record is about 110 bytes).
    let one = arcade_wallet::tx::fee_for_size(p, arcade_wallet::tx::estimate_size(1, &[3 + 4 + 32 + 8 + 2 + PART_TYPE.len(), 25]) as u64, fp.fallback_per_kb);
    let txs = new.len() as u64 + 1;
    let blocks = txs;
    Ok(json!({"parts": parts.len(), "new_parts": new.len(), "bytes": parts.iter().map(|x| x.1).sum::<u64>(), "manifest_bytes": manifest_len,
              "blob_fee": blob_fee, "tx_fees": one * txs, "fee": blob_fee + one * txs, "transactions": txs,
              "blocks": blocks, "block_seconds": net.block_seconds, "seconds": blocks * net.block_seconds,
              "note": "one part a block (a block carries 4 MB of new blobs), then the manifest; more if others publish blobs meanwhile"}))
}

/// `arc.blob_publish_large {path | hex, type, class?, gzip?, name?, send?}` (and `arc.blob_publish` above 4 MB): a large
/// blob. Without `send`, the quote: its id, parts, fee and the blocks it takes. With it, the job starts (or the one for
/// the same bytes resumes) and `arc.blob_publish_status` follows it.
pub fn api_publish(node: &Node, req: &Value) -> Result<Value, String> {
    if crate::arcsvc::usable_chain().is_some_and(|p| !crate::arcsvc::own(p)) {
        return Err("large blobs need the ARCADE chain installed here (it is only borrowed): install it in Chains & Coins".into());
    }
    let net = crate::arcsvc::svc_net().ok_or("the ARCADE chain isn't running here")?;
    let tip = crate::arcsvc::with_chain(|_, c| c.tip().height).unwrap_or(0);
    if !arcade_arc::params::rules_v5(&net, tip + 1) {
        return Err("large blobs come with rules version 5 (fog storage)".into());
    }
    let class = Class::from_u8(req["class"].as_u64().unwrap_or(0) as u8).ok_or("class is 0, 1 or 2")?;
    if class == Class::Boost {
        return Err("a large blob is Permanent (0), Release (1) or Temporary (2); boost its parts one by one".into());
    }
    let encoding = if req["gzip"] == true { Encoding::Gzip } else { Encoding::Plain };
    let kind: String = req["type"].as_str().unwrap_or("application/octet-stream").to_string();
    if kind.len() > blob::MAX_TYPE {
        return Err(format!("the type is at most {} characters", blob::MAX_TYPE));
    }
    let send = req["send"] == true;
    let hex_bytes = match req["hex"].as_str() {
        Some(h) => Some(hex::decode(h.trim()).map_err(|_| "hex is not hex".to_string())?),
        None => None,
    };
    let path = req["path"].as_str().map(String::from);
    let name = req["name"].as_str().map(String::from).or_else(|| path.as_deref().and_then(|p| std::path::Path::new(p).file_name()?.to_str().map(String::from))).unwrap_or_default();
    let open = || -> Result<Box<dyn std::io::Read>, String> {
        match (&path, &hex_bytes) {
            (Some(p), _) => Ok(Box::new(std::io::BufReader::new(std::fs::File::open(p).map_err(|e| format!("{p}: {e}"))?))),
            (None, Some(b)) => Ok(Box::new(std::io::Cursor::new(b.clone()))),
            _ => Err("path or hex required".into()),
        }
    };
    // First pass: the parts' ids and the root (nothing written), so the job's id is known.
    let c = cut(&net, &mut *open()?, |_, _| Ok(()))?;
    let man = Manifest { kind: kind.clone(), size: c.size, parts: c.parts.clone(), root: Some(c.root), class: class as u8, encoding: encoding as u8, sha256: c.sha256.clone(), name };
    let mbytes = man.to_bytes();
    let mid = blob::id_of_bytes(&net, &mbytes);
    let id = hex::encode(mid);
    let q = quote(&c.parts, mbytes.len() as u64, class)?;
    let mut out = json!({"blob": id, "root": hex::encode(c.root), "bytes": c.size, "type": kind, "class": class as u8, "parts": c.parts.len(),
                         "part_ids": c.parts.iter().map(|p| hex::encode(p.0)).collect::<Vec<_>>(), "quote": q, "large": true, "sent": false});
    if !send {
        return Ok(out);
    }
    if crate::arcsvc::holds_blob(&mid) {
        out["note"] = json!("the chain holds this large blob already");
        return Ok(out);
    }
    let _g = STEP.lock().unwrap();
    if load(&id).is_none() {
        // Second pass: the parts into the journal's folder (checked to be the same bytes).
        std::fs::create_dir_all(jobs_dir().join(&id)).map_err(|e| e.to_string())?;
        let again = cut(&net, &mut *open()?, |i, b| {
            let p = part_path(&id, i);
            let tmp = p.with_extension("in");
            std::fs::write(&tmp, b).and_then(|_| std::fs::rename(&tmp, &p)).map_err(|e| e.to_string())
        })?;
        if again.root != c.root || again.parts != c.parts {
            let _ = std::fs::remove_dir_all(jobs_dir().join(&id));
            return Err("the file changed while it was read".into());
        }
        std::fs::write(manifest_path(&id), &mbytes).map_err(|e| e.to_string())?;
        let mut job = Job {
            id: id.clone(),
            kind,
            class: class as u8,
            encoding: encoding as u8,
            size: c.size,
            root: hex::encode(c.root),
            parts: c.parts.iter().map(|(p, n)| Piece { id: hex::encode(p), len: *n, ..Default::default() }).collect(),
            manifest: Piece { id: id.clone(), len: mbytes.len() as u64, ..Default::default() },
            state: "publishing".into(),
            started: arcade_app::now(),
            quote: q,
            ..Default::default()
        };
        save(&mut job);
        eprintln!("arc: large blob {}: {} bytes in {} parts, publishing", &id[..12], c.size, c.parts.len());
        let _ = step(node, &mut job);
    }
    note(mid, man);
    out["sent"] = json!(true);
    out["status"] = status_of(&load(&id).ok_or("its journal is gone")?);
    Ok(out)
}

/// `arc.blob_publish_status {id?}`: one large blob's publishing (by its id), or every one this arcade has published.
pub fn api_status(req: &Value) -> Result<Value, String> {
    if let Some(id) = req["id"].as_str() {
        return load(&id.to_lowercase()).map(|j| status_of(&j)).ok_or_else(|| "no large blob of that id is published from here".into());
    }
    Ok(json!({"jobs": jobs().iter().map(status_of).collect::<Vec<_>>()}))
}

fn status_of(j: &Job) -> Value {
    let in_blocks = j.parts.iter().filter(|p| p.state == "block").count();
    let waiting = j.parts.iter().filter(|p| p.state == "waiting").count();
    let paid: u64 = j.parts.iter().chain(std::iter::once(&j.manifest)).filter(|p| p.state == "block" || p.state == "waiting").map(|p| p.fee).sum();
    json!({"id": j.id, "state": j.state, "type": j.kind, "class": j.class, "bytes": j.size, "root": j.root,
           "parts": j.parts.len(), "parts_in_blocks": in_blocks, "parts_waiting": waiting, "manifest": if j.manifest.state.is_empty() { "not sent" } else { &j.manifest.state },
           "paid": paid, "quote": j.quote, "resent": j.resent, "rebuilt": j.rebuilt, "dead": j.dead.len(),
           "txids": j.parts.iter().chain(std::iter::once(&j.manifest)).map(|p| p.txid.clone()).collect::<Vec<_>>(),
           "note": j.note, "started": j.started, "updated": j.updated, "done_at": j.done_at})
}

/// Every few seconds, for as long as the arcade runs: each job not done goes as far as it can.
pub fn run(node: std::sync::Arc<Node>) {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(2));
        if !crate::arcsvc::ready() {
            continue;
        }
        let _g = STEP.lock().unwrap();
        for mut job in jobs().into_iter().filter(|j| j.state != "done") {
            let before = serde_json::to_string(&job).unwrap_or_default();
            if let Err(e) = step(&node, &mut job) {
                job.note = e;
            }
            if serde_json::to_string(&job).unwrap_or_default() != before {
                save(&mut job);
            }
        }
    }
}

/// Where one of a waiting transaction's coins is.
#[derive(Debug, PartialEq)]
enum Coin {
    /// Spendable now (in a block and unspent, or a waiting output nothing spends).
    Here,
    /// Spent by another transaction (in a block or waiting).
    Spent,
    /// Its transaction is neither in a block nor waiting.
    Missing,
}

fn coin(node: &Node, op: &arcade_chain::OutPoint, by: &arcade_chain::Hash256) -> Coin {
    match crate::arcsvc::spender(op) {
        Some(s) if s == *by => return Coin::Here,
        Some(_) => return Coin::Spent,
        None => {}
    }
    if crate::arcsvc::unspent(op) || crate::arcsvc::pooled(&op.txid) {
        return Coin::Here;
    }
    let mined = crate::arcsvc::defined().is_some_and(|p| node.chain.tx_raw(p, &op.txid).ok().flatten().is_some());
    if mined {
        Coin::Spent
    } else {
        Coin::Missing
    }
}

/// Failed sends of a kept transaction before it is rebuilt when nothing says it is dead.
const TRIES: u32 = 15;

/// Bring one piece (a part, or the manifest) as far as it goes: in a block, waiting, sent (again), or built and sent
/// when `may_build`. Whether it is in a block or waiting afterwards.
fn advance(node: &Node, job: &mut Job, which: Option<usize>, may_build: bool) -> Result<bool, String> {
    let net = crate::arcsvc::svc_net().ok_or("the ARCADE chain isn't running here")?;
    let p = crate::arcsvc::defined().ok_or("the ARCADE chain isn't running here")?;
    let jid = job.id.clone();
    let (bytes_path, kind) = match which {
        Some(i) => (part_path(&jid, i), PART_TYPE.to_string()),
        None => (manifest_path(&jid), crate::arcblob::FILE_MANIFEST.to_string()),
    };
    let class = Class::from_u8(job.class).unwrap_or(Class::Permanent);
    let dead: HashSet<String> = job.dead.iter().cloned().collect();
    let piece = match which {
        Some(i) => &mut job.parts[i],
        None => &mut job.manifest,
    };
    let id: Hash = hex::decode(&piece.id).ok().and_then(|b| b.try_into().ok()).ok_or("a bad id in the journal")?;
    if crate::arcsvc::holds_blob(&id) {
        piece.state = "block".into();
        crate::arcblob::mark_mine(&id);
        return Ok(true);
    }
    let mut must: Vec<arcade_chain::OutPoint> = Vec::new();
    let mut newly_dead: Option<String> = None;
    if !piece.raw.is_empty() {
        let tx = hex::decode(&piece.raw).ok().and_then(|r| arcade_chain::Tx::parse_with(&r, &p.format).ok()).ok_or("a bad transaction in the journal")?;
        if crate::arcsvc::pooled(&tx.txid) {
            piece.state = "waiting".into();
            piece.tries = 0;
            return Ok(true);
        }
        // Neither in a block nor waiting (a restart empties the pool, and the blob's bytes with it): the same
        // transaction again, so nothing is paid twice.
        let bytes = std::fs::read(&bytes_path).map_err(|e| format!("part {}: {e}", piece.id))?;
        let held = crate::arcblob::hold(&net, bytes);
        let sent = match held {
            Some(_) => crate::ownwait::send_arc(node, &tx).map(|_| ()),
            None => Err("no room for more waiting blobs".to_string()),
        };
        // Put back meanwhile by someone else (the own-waiting sweep, or a peer relaying it): the same one, waiting.
        let sent = if sent.is_err() && crate::arcsvc::pooled(&tx.txid) { Ok(()) } else { sent };
        match sent {
            Ok(()) => {
                crate::arcblob::mark_mine(&id);
                piece.state = "waiting".into();
                piece.tries = 0;
                eprintln!("arc: large blob {}: sent {} again ({})", &jid[..12], &piece.id[..12], &piece.txid[..12.min(piece.txid.len())]);
                job.resent += 1;
                return Ok(true);
            }
            Err(e) => {
                let states: Vec<Coin> = tx.inputs.iter().map(|i| coin(node, &i.prevout, &tx.txid)).collect();
                let gone = tx.inputs.iter().zip(&states).any(|(i, s)| *s == Coin::Spent || (*s == Coin::Missing && dead.contains(&i.prevout.txid.to_hex())));
                piece.tries += 1;
                if !gone && piece.tries < TRIES {
                    piece.state = String::new();
                    return Err(format!("part {} not sent again yet: {e}", &piece.id[..12]));
                }
                eprintln!("arc: large blob {}: {} rebuilt ({e}; its coins {states:?}, {} tries)", &jid[..12], &piece.id[..12], piece.tries);
                // It can't go in (or hasn't for long): a new one, spending every coin of the old one still here, so the
                // old one can never also go in a block.
                must = tx.inputs.iter().zip(&states).filter(|(_, s)| **s == Coin::Here).map(|(i, _)| i.prevout).collect();
                newly_dead = Some(piece.txid.clone());
                piece.raw.clear();
                piece.txid.clear();
                piece.fee = 0;
                piece.tries = 0;
                piece.state = String::new();
            }
        }
    }
    if let Some(d) = newly_dead {
        job.dead.push(d);
        job.rebuilt += 1;
    }
    if !may_build {
        return Ok(false);
    }
    let bytes = std::fs::read(&bytes_path).map_err(|e| format!("{}: {e}", bytes_path.display()))?;
    // Built and kept in the journal before it is sent: a restart in between sends this same one.
    let built = crate::coinsend::publish_blobs_spending(node, &[(bytes.clone(), kind, class, Encoding::Plain)], false, &must)?;
    let raw = built["hex"].as_str().unwrap_or("").to_string();
    let tx = hex::decode(&raw).ok().and_then(|r| arcade_chain::Tx::parse_with(&r, &p.format).ok()).ok_or("the built transaction didn't parse")?;
    let piece = match which {
        Some(i) => &mut job.parts[i],
        None => &mut job.manifest,
    };
    piece.raw = raw;
    piece.txid = tx.txid.to_hex();
    piece.fee = built["fee"].as_u64().unwrap_or(0);
    piece.state = String::new();
    save(job);
    let piece = match which {
        Some(i) => &mut job.parts[i],
        None => &mut job.manifest,
    };
    crate::arcblob::hold(&net, bytes).ok_or("no room for more waiting blobs")?;
    crate::arcblob::mark_mine(&id);
    crate::ownwait::send_arc(node, &tx)?;
    piece.state = "waiting".into();
    Ok(true)
}

/// One pass over a job: the parts in order (each sent again or built while fewer than WINDOW wait), then the manifest
/// once every part is in a block or waiting; done when the manifest is in a block.
fn step(node: &Node, job: &mut Job) -> Result<(), String> {
    let mut waiting = 0;
    let mut all_in = true;
    for i in 0..job.parts.len() {
        let r = advance(node, job, Some(i), waiting < WINDOW);
        match r {
            Ok(true) => {
                if job.parts[i].state == "waiting" {
                    waiting += 1;
                }
            }
            Ok(false) => all_in = false,
            Err(e) => {
                all_in = false;
                job.note = e;
                if job.note.starts_with("not enough ARCADE") {
                    return Ok(());
                }
            }
        }
    }
    if all_in {
        match advance(node, job, None, true) {
            Ok(_) => {}
            Err(e) => job.note = e,
        }
    }
    if job.manifest.state == "block" && job.parts.iter().all(|p| p.state == "block") {
        job.state = "done".into();
        job.done_at = arcade_app::now();
        job.note.clear();
        let _ = std::fs::remove_dir_all(jobs_dir().join(&job.id));
        eprintln!("arc: large blob {}: every part and the manifest are in blocks", &job.id[..12]);
        if let (Ok(Ok(mid)), Some(m)) = (hex::decode(&job.id).map(<Hash>::try_from), std::fs::read(manifest_path(&job.id)).ok().and_then(|b| Manifest::parse(&b))) {
            note(mid, m);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcade_arc::params::TESTNET;

    fn bytes(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect()
    }

    #[test]
    fn parts_are_whole_leaves_of_one_block() {
        assert_eq!(PART_BYTES % LEAF, 0);
        assert!(PART_BYTES <= MAX_BLOB_BYTES && PART_BYTES + LEAF > MAX_BLOB_BYTES);
        assert_eq!(PART_BYTES, 3_999_992);
        assert_eq!(MAX_PARTS, 26);
    }

    #[test]
    fn the_root_streams_as_it_is_whole() {
        for n in [1usize, 126, 127, 128, 1_000, 10_000, 12_345] {
            let b = bytes(n);
            for piece in [1usize, 7, 127, 300, 5_000] {
                let mut rs = RootStream::default();
                for c in b.chunks(piece) {
                    rs.push(c);
                }
                assert_eq!(rs.finish(), (blob::root_of(&b), n as u64), "{n} bytes in pieces of {piece}");
            }
        }
    }

    #[test]
    fn a_cut_names_its_parts_and_root() {
        // A small stand-in: the cut is by PART_BYTES, so 9 MB makes 3 parts (two whole, one short).
        let b = bytes(9_000_000);
        let mut seen = Vec::new();
        let c = cut(&TESTNET, &mut std::io::Cursor::new(b.clone()), |i, part| {
            seen.push((i, part.len()));
            Ok(())
        })
        .unwrap();
        assert_eq!(seen, vec![(0, PART_BYTES), (1, PART_BYTES), (2, 9_000_000 - 2 * PART_BYTES)]);
        assert_eq!(c.root, blob::root_of(&b));
        assert_eq!(c.size, 9_000_000);
        assert_eq!(c.parts[1], (blob::id_of_bytes(&TESTNET, &b[PART_BYTES..2 * PART_BYTES]), PART_BYTES as u64));
        assert_eq!(c.sha256, hex::encode(sha2::Sha256::digest(&b)));
        // Too big is refused, at the part that passes the limit.
        struct Zeros(u64);
        impl std::io::Read for Zeros {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = (buf.len() as u64).min(self.0) as usize;
                buf[..n].fill(0);
                self.0 -= n as u64;
                Ok(n)
            }
        }
        assert!(cut(&TESTNET, &mut Zeros(MAX_LARGE_BYTES), |_, _| Ok(())).is_ok());
        assert!(cut(&TESTNET, &mut Zeros(MAX_LARGE_BYTES + 1), |_, _| Ok(())).is_err());
        assert!(cut(&TESTNET, &mut Zeros(0), |_, _| Ok(())).is_err());
    }

    fn sample() -> Manifest {
        let parts: Vec<(Hash, u64)> = (0..3u8).map(|i| ([i; 32], if i < 2 { PART_BYTES as u64 } else { 1_000 })).collect();
        Manifest { kind: "video/mp4".into(), size: 2 * PART_BYTES as u64 + 1_000, parts, root: Some([9; 32]), class: 2, encoding: 0, sha256: "ab".repeat(32), name: "film.mp4".into() }
    }

    #[test]
    fn a_manifest_reads_back_and_is_checked() {
        let m = sample();
        let b = m.to_bytes();
        assert!(b.len() < 1_000, "{} bytes", b.len());
        assert_eq!(Manifest::parse(&b), Some(m.clone()));
        assert!(m.large());
        // An older file manifest (scripts/file-publish.py): no root, read as before.
        let old = serde_json::json!({"arcade": "file", "name": "x", "type": "video/mp4", "size": 5, "sha256": "", "parts": [[hex::encode([1u8; 32]), 5]]});
        let o = Manifest::parse(old.to_string().as_bytes()).unwrap();
        assert!(!o.large());
        // Refused: parts that don't add up, a part over 4 MB, too many parts, too big, a boost, an unknown encoding.
        let bad = |f: &dyn Fn(&mut Manifest)| {
            let mut x = sample();
            f(&mut x);
            Manifest::parse(&x.to_bytes()).is_none()
        };
        assert!(bad(&|x| x.size += 1));
        assert!(bad(&|x| {
            x.parts[2].1 = MAX_BLOB_BYTES as u64 + 1;
            x.size = x.parts.iter().map(|p| p.1).sum();
        }));
        assert!(bad(&|x| {
            x.parts = vec![([1; 32], 100); MAX_PARTS + 1];
            x.size = 100 * (MAX_PARTS as u64 + 1);
        }));
        assert!(bad(&|x| {
            x.parts = vec![([1; 32], 4_000_000); 25];
            x.parts.push(([2; 32], 1));
            x.size = 100_000_001;
        }));
        assert!(bad(&|x| x.class = 3));
        assert!(bad(&|x| x.encoding = 2));
        assert!(Manifest::parse(b"{\"arcade\":\"file\"}").is_none());
        assert!(Manifest::parse(b"not json").is_none());
    }

    #[test]
    fn a_range_maps_to_its_parts() {
        let m = sample();
        let p = PART_BYTES as u64;
        // The whole.
        assert_eq!(ranges(&m.parts, 0, m.size - 1), vec![(0, 0, p), (1, 0, p), (2, 0, 1_000)]);
        // Inside one part.
        assert_eq!(ranges(&m.parts, 10, 19), vec![(0, 10, 20)]);
        // Across a boundary, from the middle (a video seeking).
        assert_eq!(ranges(&m.parts, p - 5, p + 4), vec![(0, p - 5, p), (1, 0, 5)]);
        // The last byte; the first byte of the last part.
        assert_eq!(ranges(&m.parts, m.size - 1, m.size - 1), vec![(2, 999, 1_000)]);
        assert_eq!(ranges(&m.parts, 2 * p, 2 * p), vec![(2, 0, 1)]);
        // Past the end: nothing.
        assert!(ranges(&m.parts, m.size, m.size + 10).is_empty());
        // Every byte lands once: the pieces add up to the range.
        for (a, z) in [(0u64, 0u64), (3, p * 2 + 7), (p, p), (p + 1, m.size - 1)] {
            let n: u64 = ranges(&m.parts, a, z).iter().map(|r| r.2 - r.1).sum();
            assert_eq!(n, z - a + 1, "{a}..={z}");
        }
    }

    #[test]
    fn groups_name_siblings() {
        let m = sample();
        let mid = [7u8; 32];
        note(mid, m.clone());
        let s = siblings(&[1; 32]);
        assert_eq!(s.len(), 3);
        assert!(s.contains(&mid) && s.contains(&[0; 32]) && s.contains(&[2; 32]) && !s.contains(&[1; 32]));
        assert_eq!(siblings(&mid).len(), 3);
        assert!(siblings(&[42; 32]).is_empty());
        assert_eq!(group_of(&[2; 32]).map(|g| g.0), Some(mid));
    }
}
