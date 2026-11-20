//! Node storage management (docs/STORAGEAPP.md, Martin 2026-11-19): what **this** arcade keeps of the ARCADE chain's
//! blobs by its operator's choice, and what it forgets.
//!
//! - `fog.keep {segment | blob | publisher, mode: "segment" | "pin"}`:
//!   - **segment** (the default): this arcade becomes a chosen keeper of the blob's whole segment (every segment that
//!     holds the publisher's blobs). Counted once registered and audited, sealed and paid, inside the keep share; the
//!     fog round never lets it go above the floor as it does the segments the share took by itself.
//!   - **pin** (a blob): one uncounted extra copy, never faded, served to anyone. It uses the keep share and earns
//!     nothing (decided 2026-11-19).
//! - `fog.forget {segment | blob | publisher}`: drops this arcade's copies (viewer cache, grace copy, pins) and puts
//!   it on the **forget list**: never fetched again here, every mesh ask answered "not here", left out of the bloom
//!   this arcade announces. A blob it published is un-pinned (the `.mine` mark goes). Forgetting a segment this arcade
//!   keeps is a **hand-over**: it announces it is leaving (`leave` in its hold announcement), other arcades count it
//!   out, so the next ranked arcade with room steps in (the lost-keeper refill path), and this arcade drops its copy
//!   only once that new keeper is registered and passed an audit (the segment's audited copies without this arcade are
//!   back to what they were, or at the floor). While a segment is open (not listed: nothing can be registered yet) the
//!   new keeper must announce it and hold every one of its blobs. With nobody having room it stays a keeper and says so.
//!   Forget never deletes anything anywhere else, and writes nothing on the chain: the dropped replica just stops
//!   answering its audits, the registry's own way of letting a keeper go.
//! - `fog.undo {id}`: takes a choice back (a keep, a pin, a forget; a hand-over not finished is cancelled).
//! - `fog.mine`: the list for the Storage app's Mine tab. Operator only: the forget list is nobody else's business.
//!
//! An app page asks through `POST /r/fog/<keep|forget|undo>` (crate::http): the request waits in the wallet's approval
//! queue like a repository write (crate::reposvc), and Arcade V1.0 shows its card over the app (the size, copies before
//! and after, the pay estimate and the seal time on this machine). Nothing changes before the operator approves.
//!
//! The choices are kept per network in `<home>/arc/<net>/fog-choices.json`.

use crate::fogstore::{self, SegHolders};
use crate::node::Node;
use arcade_arc::merkle::Hash;
use arcade_arc::params::{self, RANGE};
use arcade_arc::segment::{FAMILY_BLOBS, FAMILY_BLOBS_TEMP};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// One choice the operator made.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    /// "k…" a keep, "p…" a pin, "f…" a forget: what `fog.undo` takes.
    pub id: String,
    /// "keep", "pin" or "forget".
    pub kind: String,
    /// "segment", "blob" or "publisher".
    pub what: String,
    /// The segment ("P#3"), the blob (64 hex) or the publisher's address.
    pub key: String,
    /// For people: "P#3", "@tag", a blob's type and size.
    pub label: String,
    pub at: u64,
    /// The segments a keep holds (family, range index).
    #[serde(default)]
    pub segs: Vec<(u8, u64)>,
    /// The blobs a pin holds (a large blob: its manifest and every part), hex.
    #[serde(default)]
    pub blobs: Vec<String>,
    /// A forgotten blob this arcade had published (its `.mine` mark comes back on undo).
    #[serde(default)]
    pub was_mine: Vec<String>,
    /// Segments this forget hands over (it was keeping them).
    #[serde(default)]
    pub handover: Vec<Handover>,
}

/// A kept segment being handed to another keeper before this arcade drops it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Handover {
    pub family: u8,
    pub index: u64,
    /// The copies counted when it was forgotten (this arcade's own among them when it was counted), and the floor.
    pub before: usize,
    pub floor: usize,
    /// Whether the segment was listed (copies are registered, audited replicas) or open (announced keepers).
    pub listed: bool,
    pub since: u64,
    /// When this arcade dropped its copy (the hand-over done).
    #[serde(default)]
    pub done: Option<u64>,
    /// Where it stands, as the last fog round saw it: the copies counted without this arcade, and why it waits.
    #[serde(default)]
    pub now: usize,
    #[serde(default)]
    pub note: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Choices {
    pub entries: Vec<Entry>,
    /// Segments whose sealed copy here was let go (chain key, family, first, last): never sealed again by the
    /// sealing run's "next height" (crate::arcseg).
    #[serde(default)]
    pub released: Vec<(String, u8, u64, u64)>,
}

/// A blob as the forget list matches it: its id, publisher and segment.
#[derive(Clone, Debug)]
pub struct BlobKey {
    pub id: String,
    pub publisher: String,
    pub seg: Option<(u8, u64)>,
}

impl Choices {
    fn of(&self, kind: &str) -> impl Iterator<Item = &Entry> {
        let kind = kind.to_string();
        self.entries.iter().filter(move |e| e.kind == kind)
    }

    /// The segments kept by the operator's choice.
    pub fn chosen(&self) -> BTreeSet<(u8, u64)> {
        self.of("keep").flat_map(|e| e.segs.iter().copied()).collect()
    }

    /// Whether a blob (hex) is pinned.
    pub fn pinned(&self, id: &str) -> bool {
        self.of("pin").any(|e| e.blobs.iter().any(|b| b == id))
    }

    pub fn pins(&self) -> BTreeSet<String> {
        self.of("pin").flat_map(|e| e.blobs.iter().cloned()).collect()
    }

    /// Whether a segment is forgotten here (so this arcade never takes it).
    pub fn seg_forgotten(&self, family: u8, index: u64) -> bool {
        self.of("forget").any(|e| e.what == "segment" && e.segs.contains(&(family, index)))
    }

    /// Segments being handed over (not done yet).
    pub fn handing_over(&self) -> BTreeSet<(u8, u64)> {
        self.of("forget").flat_map(|e| e.handover.iter().filter(|h| h.done.is_none()).map(|h| (h.family, h.index))).collect()
    }

    /// The forget entry that covers a blob, if any.
    pub fn forgotten(&self, b: &BlobKey) -> Option<&Entry> {
        self.of("forget").find(|e| match e.what.as_str() {
            "blob" => e.key == b.id || e.blobs.iter().any(|x| *x == b.id),
            "publisher" => !b.publisher.is_empty() && e.key == b.publisher,
            "segment" => b.seg.is_some_and(|s| e.segs.contains(&s)),
            _ => false,
        })
    }

    /// Whether this arcade refuses a blob: forgotten, and not one it still owes the network. A forgotten segment it
    /// still keeps (`kept`: being handed over) is served until it is dropped, so its new keeper can fetch it.
    pub fn refused(&self, b: &BlobKey, kept: &BTreeSet<(u8, u64)>) -> bool {
        match self.forgotten(b) {
            None => false,
            Some(e) if e.what == "segment" => !b.seg.is_some_and(|s| kept.contains(&s)),
            Some(_) => true,
        }
    }

    /// The first segment past the released sealed copies of `family` on `chain` (0: none).
    pub fn released_end(&self, chain: &str, family: u8) -> u64 {
        self.released.iter().filter(|r| r.0 == chain && r.1 == family).map(|r| r.3 + 1).max().unwrap_or(0)
    }
}

// ---------------- where the choices are kept ----------------

static CH: Mutex<Option<Choices>> = Mutex::new(None);

fn choices_path() -> Option<std::path::PathBuf> {
    let d = crate::arcblob::dir();
    if d.as_os_str().is_empty() {
        return None;
    }
    Some(d.parent()?.join("fog-choices.json"))
}

fn load() -> Choices {
    choices_path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// The choices now (read once, then kept in memory).
pub fn snapshot() -> Choices {
    let mut g = CH.lock().unwrap();
    g.get_or_insert_with(load).clone()
}

/// Change the choices and save them.
fn update<R>(f: impl FnOnce(&mut Choices) -> R) -> R {
    let mut g = CH.lock().unwrap();
    let c = g.get_or_insert_with(load);
    let r = f(c);
    if let Some(p) = choices_path() {
        let tmp = p.with_extension("in");
        if std::fs::write(&tmp, serde_json::to_vec_pretty(c).unwrap_or_default()).and_then(|_| std::fs::rename(&tmp, &p)).is_err() {
            eprintln!("arc: fog: the storage choices couldn't be saved in {}", p.display());
        }
    }
    r
}

#[cfg(test)]
fn set_for_test(c: Choices) {
    *CH.lock().unwrap() = Some(c);
}

/// A blob's key for the forget list, from its meta here (no meta: its id alone).
pub fn key_of(id: &Hash) -> BlobKey {
    let m = crate::arcblob::meta(id);
    let net = crate::arcsvc::svc_net().or_else(|| crate::arcsvc::defined_net().map(|d| d.0));
    BlobKey {
        id: hex::encode(id),
        publisher: m.as_ref().map(|m| m.publisher.clone()).unwrap_or_default(),
        seg: m.as_ref().map(|m| (net.filter(|n| fogstore::on(n, m.height)).map_or(FAMILY_BLOBS, |_| fogstore::family_of(m.class)), m.height / RANGE)),
    }
}

/// Whether this arcade refuses blob `id` (crate::arcblob: view, serve, fetch; crate::fogstore: the bloom, drops).
pub fn refused(id: &Hash) -> bool {
    let c = snapshot();
    if !c.entries.iter().any(|e| e.kind == "forget") {
        return false;
    }
    let k = key_of(id);
    let kept = if k.seg.is_some() && c.forgotten(&k).is_some_and(|e| e.what == "segment") { kept_segs_now() } else { BTreeSet::new() };
    c.refused(&k, &kept)
}

fn kept_segs_now() -> BTreeSet<(u8, u64)> {
    fogstore::node_now().map(|n| fogstore::kept_segs(&n)).unwrap_or_default()
}

/// The first height past the sealed copies let go here of `family` on `chain` (crate::arcseg: sealing never goes back
/// over a segment handed over).
pub fn released_end(chain: &str, family: u8) -> u64 {
    snapshot().released_end(chain, family)
}

// ---------------- the hand-over ----------------

/// What a fog round does with a segment being handed over.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// Its new keeper is counted: drop this arcade's copy.
    Drop,
    /// Not yet: the copies counted without this arcade, how many it waits for, and why.
    Wait { now: usize, need: usize, why: String },
}

/// The copies counted without this arcade (nor any other arcade that is leaving): listed, the registered and audited
/// replicas; open, the announced keepers holding every blob of it (`ready`).
pub fn counted_without(me: &str, held: &SegHolders, leaving: &BTreeSet<String>, ready: &dyn Fn(&str) -> bool) -> usize {
    match &held.registered {
        Some(r) => r.iter().filter(|x| x.1 && x.0 != me && !leaving.contains(&x.0)).count(),
        None => held.keepers.iter().filter(|k| *k != me && !leaving.contains(*k) && ready(k)).count(),
    }
}

/// One hand-over step (pure, for the tests): drop once the copies without this arcade are back to what they were
/// when it was forgotten, or at the floor. `room`: whether any other arcade announces room enough to take it.
pub fn handover_step(h: &Handover, now: usize, room: bool) -> Step {
    let need = h.floor.min(h.before);
    if now >= need {
        return Step::Drop;
    }
    let why = if h.listed {
        format!("waiting for a new keeper to register and pass an audit ({now}/{need} copies without this arcade)")
    } else {
        format!("waiting for a new keeper to take every blob of it ({now}/{need} keepers without this arcade; open segment, not listed yet)")
    };
    if room {
        Step::Wait { now, need, why }
    } else {
        Step::Wait { now, need, why: format!("no arcade has room to take it: this arcade stays a keeper ({now}/{need})") }
    }
}

/// The fog round's hand-over pass (crate::fogstore::round_inner): for each segment being handed over that this arcade
/// still keeps, whether to drop it now. Notes where each stands. `info`: (family, index) → (holders, leaving, room
/// anywhere, ready-check results per keeper).
pub fn round_handover(me: &str, kept: &BTreeSet<(u8, u64)>, look: &dyn Fn(u8, u64) -> Option<(SegHolders, BTreeSet<String>, bool, BTreeSet<String>)>) -> Vec<(u8, u64)> {
    let mut drop = Vec::new();
    let now_t = arcade_app::now();
    update(|c| {
        for e in c.entries.iter_mut().filter(|e| e.kind == "forget") {
            for h in e.handover.iter_mut().filter(|h| h.done.is_none()) {
                if !kept.contains(&(h.family, h.index)) {
                    // Not kept any more (let go by the share, or never registered): nothing left to hand over.
                    h.done = Some(now_t);
                    h.note = "dropped (this arcade no longer kept it)".into();
                    continue;
                }
                let Some((held, leaving, room, ready)) = look(h.family, h.index) else { continue };
                let now = counted_without(me, &held, &leaving, &|k| ready.contains(k));
                h.now = now;
                match handover_step(h, now, room) {
                    Step::Drop => {
                        h.done = Some(now_t);
                        h.note = format!("handed over: {now} copies counted without this arcade (floor {})", h.floor);
                        drop.push((h.family, h.index));
                    }
                    Step::Wait { why, .. } => h.note = why,
                }
            }
        }
    });
    drop
}

/// A hand-over finished: the sealed copy of the segment here is let go (it stops answering audits, so the registry
/// removes it the usual way) and never sealed again.
pub fn release_sealed(node: &Node, family: u8, index: u64) {
    let Some(net) = crate::arcsvc::svc_net() else { return };
    let Some(chain) = crate::arcsvc::defined().map(|p| p.key) else { return };
    let (first, last) = (index * RANGE, index * RANGE + RANGE - 1);
    update(|c| {
        if !c.released.iter().any(|r| r.0 == chain && r.1 == family && r.2 == first) {
            c.released.push((chain.to_string(), family, first, last));
        }
    });
    for r in crate::arcseal::replicas(node, &net) {
        if r.chain == chain && r.family == family && r.first == first {
            let _ = std::fs::remove_dir_all(&r.dir);
            crate::arcseal::forget_cache();
            eprintln!("arc: fog: the sealed copy of {} let go (handed over)", crate::fogmap::seg_name(family, index));
        }
    }
}

// ---------------- what a choice does (the card) ----------------

/// A segment as the card weighs it.
#[derive(Clone, Debug)]
struct SegNow {
    family: u8,
    index: u64,
    bytes: u64,
    blobs: usize,
    floor: usize,
    copies: usize,
    listed: bool,
    mine: bool,
    /// Other arcades announcing room enough for it.
    room_elsewhere: usize,
}

fn net_now() -> Option<params::Net> {
    crate::arcsvc::svc_net().or_else(|| crate::arcsvc::defined_net().map(|d| d.0))
}

/// Every blob known here, by segment.
fn by_segment() -> BTreeMap<(u8, u64), Vec<(Hash, crate::arcblob::Meta)>> {
    let mut out: BTreeMap<(u8, u64), Vec<(Hash, crate::arcblob::Meta)>> = BTreeMap::new();
    let Some(net) = net_now() else { return out };
    for (id, m) in crate::arcblob::known() {
        let fam = if fogstore::on(&net, m.height) { fogstore::family_of(m.class) } else { FAMILY_BLOBS };
        out.entry((fam, m.height / RANGE)).or_default().push((id, m));
    }
    out
}

fn seg_now(node: &Node, segs: &BTreeMap<(u8, u64), Vec<(Hash, crate::arcblob::Meta)>>, family: u8, index: u64) -> SegNow {
    let metas = segs.get(&(family, index)).cloned().unwrap_or_default();
    let bytes = metas.iter().map(|(id, _)| crate::arcblob::len_of(id).unwrap_or(0)).sum();
    let held = fogstore::seg_holders(node, family, index);
    let floor = fogstore::seg_floor(family, index).unwrap_or_else(|| {
        let refs: Vec<&crate::arcblob::Meta> = metas.iter().map(|x| &x.1).collect();
        fogstore::floor_of(family, &refs, crate::arcblob::next_height(), arcade_app::now(), u64::MAX / 4, fogstore::class_floor)
    });
    let room_elsewhere = fogstore::announcements_now(node)
        .iter()
        .filter(|(n, f)| !held.keepers.contains(n) && f["room"].as_u64().is_some_and(|r| r > 0 && r.saturating_add(1) << 20 > bytes))
        .count();
    SegNow { family, index, bytes, blobs: metas.len(), floor, copies: held.for_decision(), listed: held.registered.is_some(), mine: fogstore::kept_segs(node).contains(&(family, index)), room_elsewhere }
}

/// The pot the storage registry shares each block (all but the writer's part), in base units.
const POT_PER_BLOCK: u64 = params::BLOCK_REWARD - params::WRITER_REWARD;
const BLOCKS_A_MONTH: u64 = 30 * 86_400 / params::BLOCK_SECONDS;

/// What keeping `bytes` more as one of `copies_after` keepers would earn a month, about: its pay weight (bytes split
/// among the copies, B/n) as a share of all pay weight now (every owner's replicas and the genesis bytes), of the pot
/// each block shares (its fees left out). ARCADE base units.
fn pay_estimate(bytes: u64, copies_after: usize) -> (u64, u64) {
    let w = bytes / copies_after.max(1) as u64;
    let total: u128 = crate::arcsvc::with_chain(|_, chain| {
        let reg = chain.storage();
        // Every owner's replicas and the genesis bytes: the pot is shared by both.
        reg.owners().iter().map(|o| reg.owner_pay_weight(o) as u128).sum::<u128>() + reg.genesis_earners().iter().map(|g| g.1 as u128).sum::<u128>()
    })
    .unwrap_or(0);
    let month = (POT_PER_BLOCK as u128 * BLOCKS_A_MONTH as u128 * w as u128 / (total + w as u128).max(1)) as u64;
    (w, month)
}

/// How long sealing `bytes` takes on this machine, in seconds: its measured seal speed (crate::sysinfo, core-seconds
/// per GiB) over its cores. None until it has sealed enough to measure.
fn seal_secs(bytes: u64) -> Option<u64> {
    let speed = crate::sysinfo::seal_speed()?;
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
    Some((speed * bytes as f64 / (1u64 << 30) as f64 / cores).ceil() as u64)
}

fn arc(units: u64) -> String {
    let whole = units / params::COIN;
    let frac = (units % params::COIN) / (params::COIN / 100);
    format!("{whole}.{frac:02} ARCADE")
}

fn mb(bytes: u64) -> String {
    if bytes >= 10 << 20 {
        format!("{} MB", bytes >> 20)
    } else if bytes >= 1 << 20 {
        format!("{:.1} MB", bytes as f64 / (1u64 << 20) as f64)
    } else {
        format!("{} kB", bytes.div_ceil(1000))
    }
}

fn hex_id(s: &str) -> Result<Hash, String> {
    hex::decode(s.trim()).ok().and_then(|b| <Hash>::try_from(b).ok()).ok_or_else(|| "blob is a blob id (64 hex)".to_string())
}

/// "P#3" / "T#3" (or "p3") → (family, index).
pub fn parse_seg(s: &str) -> Result<(u8, u64), String> {
    let t = s.trim().to_ascii_uppercase().replace('#', "");
    let (fam, n) = if let Some(n) = t.strip_prefix('P') {
        (FAMILY_BLOBS, n)
    } else if let Some(n) = t.strip_prefix('T') {
        (FAMILY_BLOBS_TEMP, n)
    } else {
        return Err(format!("segment is a blob segment's name, P#n or T#n (as explorer.arc_blobs gives it), not {s}"));
    };
    n.parse().map(|i| (fam, i)).map_err(|_| format!("segment is P#n or T#n, not {s}"))
}

/// A publisher (@tag or address) → its address, as the blobs here name it.
fn publisher_address(node: &Node, s: &str) -> Result<(String, String), String> {
    let s = s.trim();
    if let Some(tag) = s.strip_prefix('@') {
        let p = crate::arcsvc::defined().ok_or("the ARCADE chain isn't here")?;
        // A tag table here, else any blob's publisher whose tag it is.
        if let Ok(Some(row)) = arcade_ledger::index::tag_row(&node.chain, p.chain_no, &tag.to_ascii_lowercase()) {
            return Ok((row.address, format!("@{tag}")));
        }
        let pubs: BTreeSet<String> = crate::arcblob::known().into_iter().map(|(_, m)| m.publisher).collect();
        for a in pubs {
            if arcade_ledger::index::tag_of(&node.chain, p.chain_no, &a).ok().flatten().is_some_and(|t| t.eq_ignore_ascii_case(tag)) {
                return Ok((a, format!("@{tag}")));
            }
        }
        return Err(format!("no blobs here from @{tag}"));
    }
    if s.is_empty() {
        return Err("publisher is an address or @tag".into());
    }
    let tag = crate::arcsvc::defined().and_then(|p| arcade_ledger::index::tag_of(&node.chain, p.chain_no, s).ok().flatten());
    Ok((s.to_string(), tag.map(|t| format!("@{t}")).unwrap_or_else(|| format!("{}…", &s[..s.len().min(10)]))))
}

/// What a request names: ("segment" | "blob" | "publisher", key).
fn target(req: &Value) -> Result<(&'static str, String), String> {
    let mut found = Vec::new();
    for k in ["segment", "blob", "publisher"] {
        if let Some(v) = req[k].as_str().filter(|v| !v.trim().is_empty()) {
            found.push((k, v.trim().to_string()));
        }
    }
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err("name one of segment, blob or publisher".into()),
        _ => Err("name only one of segment, blob or publisher".into()),
    }
}

/// A blob and, for a large blob, its manifest's parts.
fn blob_and_parts(id: &Hash) -> Vec<Hash> {
    let mut v = vec![*id];
    if let Some(m) = crate::largeblob::all().into_iter().find(|g| g.0 == *id) {
        v.extend(m.1.parts.iter().map(|p| p.0));
    }
    v
}

/// Why this arcade can't keep anything by choice now (borrowed, or no keep share), if so.
fn cant_keep(node: &Node) -> Option<String> {
    if crate::arcsvc::svc_net().is_none() {
        return Some("no keep share: this arcade borrows the ARCADE chain (it borrows everything), so it can't keep or pin data. Forget still works on its viewer cache.".into());
    }
    if fogstore::keep_mb(node) == 0 {
        return Some("no keep share: set one in Options (Keep ARCADE data) first. Forget still works.".into());
    }
    None
}

/// The plan for a keep or pin: its card, and what applying it adds.
struct KeepPlan {
    card: Value,
    entry: Entry,
}

fn keep_plan(node: &Node, req: &Value) -> Result<KeepPlan, String> {
    let (what, key) = target(req)?;
    let mode = req["mode"].as_str().unwrap_or("segment");
    if !["segment", "pin"].contains(&mode) {
        return Err("mode is segment or pin".into());
    }
    if mode == "pin" && what != "blob" {
        return Err("pin is for one blob: name blob (or keep the segment)".into());
    }
    if let Some(why) = cant_keep(node) {
        return Err(why);
    }
    let ch = snapshot();
    let segs_all = by_segment();
    let share = fogstore::keep_mb(node) << 20;
    let used = fogstore::kept_mb_used(node) << 20;
    let free = share.saturating_sub(used);
    let kept = fogstore::kept_segs(node);
    let mut notes: Vec<String> = Vec::new();
    let now = arcade_app::now();
    if mode == "pin" {
        let id = hex_id(&key)?;
        let m = crate::arcblob::meta(&id).ok_or("no such blob known here")?;
        let k = key_of(&id);
        if let Some(e) = ch.forgotten(&k) {
            return Err(format!("it's forgotten here ({}): undo that first ({})", e.label, e.id));
        }
        let all = blob_and_parts(&id);
        if all.iter().all(|b| ch.pinned(&hex::encode(b))) {
            return Err("it's pinned here already".into());
        }
        let bytes: u64 = all.iter().map(|b| crate::arcblob::len_of(b).unwrap_or(0)).sum();
        let in_kept = all.iter().all(|b| key_of(b).seg.is_some_and(|s| kept.contains(&s)));
        let adds = if in_kept { 0 } else { bytes };
        if adds > free {
            return Err(format!("the keep share is full: {} free of {}, this pin needs {}. Raise the share in Options.", mb(free), mb(share), mb(adds)));
        }
        if in_kept {
            notes.push("its segment is kept here already: the pin keeps it after this arcade lets the segment go".into());
        }
        let seg = k.seg.map(|(f, i)| seg_now(node, &segs_all, f, i));
        let label = format!("{} {}", if all.len() > 1 { "large blob" } else { "blob" }, &key[..12.min(key.len())]);
        let card = json!({
            "op": "keep", "mode": "pin", "what": "blob", "key": key, "title": format!("Pin this blob ({})", mb(bytes)),
            "size_bytes": bytes, "type": m.kind, "parts": all.len(),
            "segment": seg.as_ref().map(|s| crate::fogmap::seg_name(s.family, s.index)),
            "copies_before": seg.as_ref().map(|s| s.copies), "copies_after": seg.as_ref().map(|s| s.copies), "floor": seg.as_ref().map(|s| s.floor),
            "counted": false, "pay_month": 0, "pay_text": "earns nothing: a pinned extra copy isn't counted against the floor",
            "seal_secs": Value::Null, "seal_text": "not sealed: a pinned copy is kept plain and served to anyone",
            "share": {"mb": share >> 20, "used_bytes": used, "free_bytes": free, "after_free_bytes": free.saturating_sub(adds)},
            "lines": [format!("Keeps {} ({}) here for good, never faded, and serves it to anyone who asks.", label, mb(bytes)),
                      format!("Uses {} of the keep share ({} free of {}).", mb(adds), mb(free), mb(share)),
                      "Uncounted: copies against the floor don't change, and it earns nothing.".to_string()],
            "notes": notes,
        });
        let entry = Entry { id: format!("p{}", arcade_mesh::random::hex(5)), kind: "pin".into(), what: "blob".into(), key: key.clone(), label, at: now,
                            blobs: all.iter().map(hex::encode).collect(), ..Default::default() };
        return Ok(KeepPlan { card, entry });
    }
    // Keep segment(s).
    let (segs, label): (Vec<(u8, u64)>, String) = match what {
        "segment" => {
            let s = parse_seg(&key)?;
            if !segs_all.contains_key(&s) {
                return Err(format!("no blobs known here in segment {key}"));
            }
            (vec![s], crate::fogmap::seg_name(s.0, s.1))
        }
        "blob" => {
            let id = hex_id(&key)?;
            crate::arcblob::meta(&id).ok_or("no such blob known here")?;
            let s: BTreeSet<(u8, u64)> = blob_and_parts(&id).iter().filter_map(|b| key_of(b).seg).collect();
            let names: Vec<String> = s.iter().map(|(f, i)| crate::fogmap::seg_name(*f, *i)).collect();
            (s.into_iter().collect(), names.join(", "))
        }
        _ => {
            let (addr, who) = publisher_address(node, &key)?;
            let s: BTreeSet<(u8, u64)> = segs_all.iter().filter(|(_, v)| v.iter().any(|(_, m)| m.publisher == addr)).map(|(k, _)| *k).collect();
            if s.is_empty() {
                return Err(format!("no blobs known here from {who}"));
            }
            (s.into_iter().collect(), format!("every segment holding {who}'s blobs"))
        }
    };
    let chosen = ch.chosen();
    let new: Vec<(u8, u64)> = segs.iter().copied().filter(|s| !chosen.contains(s)).collect();
    if new.is_empty() {
        return Err(format!("{label}: kept by choice here already"));
    }
    if let Some(s) = new.iter().find(|s| ch.seg_forgotten(s.0, s.1)) {
        return Err(format!("{} is forgotten here: undo that first", crate::fogmap::seg_name(s.0, s.1)));
    }
    let info: Vec<SegNow> = new.iter().map(|(f, i)| seg_now(node, &segs_all, *f, *i)).collect();
    let adds: u64 = info.iter().filter(|s| !s.mine).map(|s| s.bytes).sum();
    let size: u64 = info.iter().map(|s| s.bytes).sum();
    // Room: the share's free room, then segments the share took by itself whose copies hold without this arcade.
    let mut replaces: Vec<Value> = Vec::new();
    if adds > free {
        let mut freed = free;
        let mut cands: Vec<SegNow> = kept.iter().filter(|k| !chosen.contains(k) && !new.contains(k)).map(|(f, i)| seg_now(node, &segs_all, *f, *i)).filter(|s| s.copies > s.floor).collect();
        cands.sort_by_key(|s| std::cmp::Reverse(s.copies.saturating_sub(s.floor)));
        for s in cands {
            if freed >= adds {
                break;
            }
            freed += s.bytes;
            replaces.push(json!({"segment": crate::fogmap::seg_name(s.family, s.index), "bytes": s.bytes, "copies": s.copies, "floor": s.floor}));
        }
        if freed < adds {
            return Err(format!("the keep share is full: {} free of {}, this needs {}. Raise the share in Options.", mb(free), mb(share), mb(adds)));
        }
    }
    let per: Vec<Value> = info
        .iter()
        .map(|s| {
            let after = if s.mine { s.copies } else { s.copies + 1 };
            json!({"segment": crate::fogmap::seg_name(s.family, s.index), "bytes": s.bytes, "blobs": s.blobs, "copies_before": s.copies, "copies_after": after,
                   "floor": s.floor, "listed": s.listed, "kept_here_already": s.mine, "label": format!("{} · {} · {}/{} → {}/{} copies", crate::fogmap::seg_name(s.family, s.index), mb(s.bytes), s.copies, s.floor, after, s.floor)})
        })
        .collect();
    let (copies_before, copies_after, floor) = info.first().map(|s| (s.copies, if s.mine { s.copies } else { s.copies + 1 }, s.floor)).unwrap_or((0, 0, 0));
    let mut month = 0;
    let mut weight = 0;
    for s in &info {
        let (w, m) = pay_estimate(s.bytes, if s.mine { s.copies.max(1) } else { s.copies + 1 });
        weight += w;
        month += m;
    }
    let seal = seal_secs(size);
    let seal_text = match seal {
        Some(t) if t < 120 => format!("about {t} s to seal on this machine"),
        Some(t) if t < 7200 => format!("about {} min to seal on this machine", t.div_ceil(60)),
        Some(t) => format!("about {:.1} h to seal on this machine", t as f64 / 3600.0),
        None => "seal time not measured here yet (this machine shows it after sealing 64 MiB)".into(),
    };
    if info.iter().any(|s| !s.listed) {
        notes.push("an open segment (its 10,000-block range isn't complete): it registers and starts counting once it is listed and passes its first audit".into());
    } else {
        notes.push("counted once its replica is registered and passes its first audit".into());
    }
    if !replaces.is_empty() {
        notes.push(format!("to make room it lets go of {} segment(s) the keep share took by itself that have copies to spare", replaces.len()));
    }
    let already = info.iter().all(|s| s.mine);
    let mut lines = vec![
        format!("This arcade becomes a chosen keeper of {label}: {} in {} blob(s).", mb(size), info.iter().map(|s| s.blobs).sum::<usize>()),
        if already {
            format!("It keeps it already (the keep share took it, copies {copies_before}/{floor}): by choice, the fog round never lets it go above the floor.")
        } else {
            format!("Copies {copies_before}/{floor} → {copies_after}/{floor} once it is counted.")
        },
        format!("Pay: about {} a month (estimate; weight B/n = {}).", arc(month), mb(weight)),
        seal_text.clone(),
        format!("Uses {} of the keep share ({} free of {}).", mb(adds), mb(free), mb(share)),
    ];
    if !replaces.is_empty() {
        lines.push(format!("Replaces: {}.", replaces.iter().filter_map(|r| r["segment"].as_str()).collect::<Vec<_>>().join(", ")));
    }
    let card = json!({
        "op": "keep", "mode": "segment", "what": what, "key": key, "title": format!("Keep {label}"),
        "segments": per, "size_bytes": size, "adds_bytes": adds, "copies_before": copies_before, "copies_after": copies_after, "floor": floor,
        "counted": true, "weight": weight, "pay_month": month, "pay_text": format!("about {} a month (estimate)", arc(month)),
        "seal_secs": seal, "seal_text": seal_text, "replaces": replaces,
        "share": {"mb": share >> 20, "used_bytes": used, "free_bytes": free, "after_free_bytes": free.saturating_sub(adds)},
        "lines": lines, "notes": notes,
    });
    let entry = Entry { id: format!("k{}", arcade_mesh::random::hex(5)), kind: "keep".into(), what: what.into(), key: key.clone(), label, at: now, segs: new, ..Default::default() };
    Ok(KeepPlan { card, entry })
}

struct ForgetPlan {
    card: Value,
    entry: Entry,
}

fn forget_plan(node: &Node, req: &Value) -> Result<ForgetPlan, String> {
    let (what, key) = target(req)?;
    let ch = snapshot();
    let segs_all = by_segment();
    let kept = fogstore::kept_segs(node);
    let now = arcade_app::now();
    let mut notes: Vec<String> = Vec::new();
    let mut entry = Entry { id: format!("f{}", arcade_mesh::random::hex(5)), kind: "forget".into(), what: what.into(), key: key.clone(), at: now, ..Default::default() };
    // The blobs it covers.
    let blobs: Vec<Hash> = match what {
        "segment" => {
            let s = parse_seg(&key)?;
            if ch.seg_forgotten(s.0, s.1) {
                return Err(format!("{key} is forgotten here already"));
            }
            entry.segs = vec![s];
            entry.label = crate::fogmap::seg_name(s.0, s.1);
            entry.key = entry.label.clone();
            segs_all.get(&s).map(|v| v.iter().map(|x| x.0).collect()).unwrap_or_default()
        }
        "blob" => {
            let id = hex_id(&key)?;
            if ch.of("forget").any(|e| e.what == "blob" && e.key == key) {
                return Err("it's forgotten here already".into());
            }
            let all = blob_and_parts(&id);
            entry.label = format!("{} {}", if all.len() > 1 { "large blob" } else { "blob" }, &key[..12.min(key.len())]);
            entry.blobs = all.iter().map(hex::encode).collect();
            all
        }
        _ => {
            let (addr, who) = publisher_address(node, &key)?;
            if ch.of("forget").any(|e| e.what == "publisher" && e.key == addr) {
                return Err(format!("{who} is forgotten here already"));
            }
            entry.key = addr.clone();
            entry.label = who;
            crate::arcblob::known().into_iter().filter(|(_, m)| m.publisher == addr).map(|(id, _)| id).collect()
        }
    };
    let here: Vec<&Hash> = blobs.iter().filter(|b| crate::arcblob::bytes_here(b)).collect();
    let here_bytes: u64 = here.iter().map(|b| crate::arcblob::size_here(b)).sum();
    entry.was_mine = blobs.iter().filter(|b| crate::arcblob::is_mine(b)).map(hex::encode).collect();
    let pinned: Vec<&Hash> = blobs.iter().filter(|b| ch.pinned(&hex::encode(b))).collect();
    let mut lines = vec![format!(
        "Forget {}: {} blob(s) known here, {} held here ({}).",
        entry.label, blobs.len(), here.len(), mb(here_bytes)
    )];
    lines.push("This arcade drops its copies and never fetches or serves them again (mesh asks get \"not here\"). Other arcades still keep and serve them: nothing is deleted anywhere else.".into());
    if !entry.was_mine.is_empty() {
        lines.push(format!("Un-pins {} blob(s) this arcade published: they stay on the chain, kept by their keepers while paid for their class.", entry.was_mine.len()));
    }
    if !pinned.is_empty() {
        lines.push(format!("Your pin of {} blob(s) goes too.", pinned.len()));
    }
    let mut copies_before = Value::Null;
    let mut copies_after = Value::Null;
    let mut floor = Value::Null;
    let mut stop_earning = 0u64;
    let mut handing = Vec::new();
    // Segments this arcade keeps that the forget touches.
    let touched: BTreeSet<(u8, u64)> = blobs.iter().filter_map(|b| key_of(b).seg).filter(|s| kept.contains(s)).collect();
    if what == "segment" {
        let s = entry.segs[0];
        let n = seg_now(node, &segs_all, s.0, s.1);
        floor = json!(n.floor);
        copies_before = json!(n.copies);
        if kept.contains(&s) {
            let after = n.copies.saturating_sub(1);
            copies_after = json!(after);
            let (_, m) = pay_estimate(n.bytes, n.copies.max(1));
            stop_earning = m;
            lines.push(format!("Copies {}/{} → {after}/{} until a new keeper joins: this arcade keeps it until the new keeper is registered and passes an audit, then drops it.", n.copies, n.floor, n.floor));
            lines.push(format!("You stop earning about {} a month (estimate).", arc(m)));
            if n.room_elsewhere == 0 {
                lines.push("No other arcade announces room for it now: this arcade stays a keeper until one does.".into());
            }
            handing.push(Handover { family: s.0, index: s.1, before: n.copies, floor: n.floor, listed: n.listed, since: now, ..Default::default() });
        } else {
            copies_after = json!(n.copies);
            lines.push(format!("This arcade isn't a keeper of it: copies counted stay {}/{}.", n.copies, n.floor));
        }
    } else {
        if !touched.is_empty() {
            let names: Vec<String> = touched.iter().map(|(f, i)| crate::fogmap::seg_name(*f, *i)).collect();
            notes.push(format!("this arcade keeps {} as a keeper: its sealed copy stays (the network is owed it). Forget the segment to hand it over.", names.join(", ")));
        }
        if let Some((f, i)) = blobs.first().and_then(|b| key_of(b).seg) {
            let n = seg_now(node, &segs_all, f, i);
            copies_before = json!(n.copies);
            copies_after = json!(n.copies);
            floor = json!(n.floor);
            lines.push(format!("Copies counted don't change ({}/{}): viewer and pinned copies aren't counted.", n.copies, n.floor));
        }
    }
    entry.handover = handing.clone();
    let card = json!({
        "op": "forget", "what": what, "key": entry.key, "title": format!("Forget {}", entry.label),
        "blobs": blobs.len(), "held_here": here.len(), "frees_bytes": here_bytes, "size_bytes": here_bytes,
        "copies_before": copies_before, "copies_after": copies_after, "floor": floor,
        "stop_earning_month": stop_earning, "unpins_own": entry.was_mine.len(), "drops_pins": pinned.len(),
        "handover": handing.iter().map(|h| json!({"segment": crate::fogmap::seg_name(h.family, h.index), "before": h.before, "floor": h.floor, "listed": h.listed})).collect::<Vec<_>>(),
        "pay_text": if stop_earning > 0 { format!("you stop earning about {} a month", arc(stop_earning)) } else { "no pay changes".to_string() },
        "seal_secs": Value::Null, "seal_text": "nothing to seal",
        "lines": lines, "notes": notes,
    });
    Ok(ForgetPlan { card, entry })
}

fn undo_plan(req: &Value) -> Result<(Value, Entry), String> {
    let id = req["id"].as_str().map(String::from).or_else(|| req["id"].as_u64().map(|n| n.to_string())).ok_or("id required (from fog.mine)")?;
    let e = snapshot().entries.into_iter().find(|e| e.id == id).ok_or_else(|| format!("no choice {id} here (fog.mine lists them)"))?;
    let what = match e.kind.as_str() {
        "keep" => format!("Stop keeping {} by choice: the fog round decides it again with the rest of the keep share (above the floor it may let it go).", e.label),
        "pin" => format!("Un-pin {}: its copy fades like any viewed blob.", e.label),
        _ if e.handover.iter().any(|h| h.done.is_none()) => format!("Take back forgetting {}: the hand-over stops and this arcade stays a keeper.", e.label),
        _ => format!("Take back forgetting {}: it is fetched and served again on its next view.", e.label),
    };
    let card = json!({"op": "undo", "id": id, "title": format!("Undo: {}", e.label), "kind": e.kind, "lines": [what], "notes": [], "size_bytes": 0, "seal_text": "nothing to seal", "pay_text": "no pay changes"});
    Ok((card, e))
}

/// The card for a request (`op`: keep, forget, undo) without changing anything.
pub fn card(node: &Node, op: &str, req: &Value) -> Result<Value, String> {
    match op {
        "keep" => keep_plan(node, req).map(|p| p.card),
        "forget" => forget_plan(node, req).map(|p| p.card),
        "undo" => undo_plan(req).map(|p| p.0),
        _ => Err(format!("no storage op called {op}: keep, forget or undo")),
    }
}

/// Drop this arcade's copies of what it refuses now (forgotten, not owed to the network). Bytes freed.
fn drop_refused(node: &Node) -> u64 {
    let c = snapshot();
    let kept = fogstore::kept_segs(node);
    let mut freed = 0;
    for id in crate::arcblob::held_ids() {
        let k = key_of(&id);
        if c.refused(&k, &kept) && !k.seg.is_some_and(|s| kept.contains(&s)) && !c.pinned(&k.id) {
            freed += crate::arcblob::drop_plain(&id) + crate::arcblob::drop_cached(&id);
        }
    }
    freed
}

/// Apply a request (`op`: keep, forget, undo): (what it did, in words; the entry id).
pub fn apply(node: &Node, op: &str, req: &Value) -> Result<Value, String> {
    match op {
        "keep" => {
            let p = keep_plan(node, req)?;
            let e = p.entry.clone();
            // Room made by letting go of the share's own segments with copies to spare.
            let replaced: Vec<(u8, u64)> = p.card["replaces"].as_array().into_iter().flatten().filter_map(|r| parse_seg(r["segment"].as_str()?).ok()).collect();
            if !e.segs.is_empty() {
                fogstore::keep_now(node, &e.segs, &replaced);
            }
            update(|c| c.entries.push(e.clone()));
            let _ = node.app.event("chains", &format!("Storage: {}", p.card["title"].as_str().unwrap_or("kept")));
            Ok(json!({"done": true, "id": e.id, "kind": e.kind, "card": p.card}))
        }
        "forget" => {
            let p = forget_plan(node, req)?;
            let e = p.entry.clone();
            for b in &e.was_mine {
                if let Ok(id) = hex_id(b) {
                    crate::arcblob::unmark_mine(&id);
                }
            }
            update(|c| {
                // A forgotten blob's pins, and keeps of a forgotten segment, go.
                let gone: BTreeSet<String> = e.blobs.iter().cloned().collect();
                c.entries.retain(|x| !(x.kind == "pin" && x.blobs.iter().any(|b| gone.contains(b))));
                for x in c.entries.iter_mut().filter(|x| x.kind == "keep") {
                    x.segs.retain(|s| !e.segs.contains(s));
                }
                c.entries.retain(|x| !(x.kind == "keep" && x.segs.is_empty()));
                c.entries.push(e.clone());
            });
            let freed = drop_refused(node);
            let _ = node.app.event("chains", &format!("Storage: forget {}", e.label));
            Ok(json!({"done": true, "id": e.id, "kind": e.kind, "freed_bytes": freed, "handing_over": e.handover.len(), "card": p.card}))
        }
        "undo" => {
            let (card, e) = undo_plan(req)?;
            update(|c| {
                c.entries.retain(|x| x.id != e.id);
                // A segment handed over and taken back may be sealed here again.
                if e.kind == "forget" {
                    c.released.retain(|r| !e.segs.iter().any(|(f, i)| r.1 == *f && r.2 == i * RANGE));
                }
            });
            if e.kind == "forget" {
                for b in &e.was_mine {
                    if let Ok(id) = hex_id(b) {
                        crate::arcblob::mark_mine(&id);
                    }
                }
            }
            let _ = node.app.event("chains", &format!("Storage: undo {}", e.label));
            Ok(json!({"done": true, "id": e.id, "undone": e.kind, "card": card}))
        }
        _ => Err(format!("no storage op called {op}: keep, forget or undo")),
    }
}

/// `fog.keep`, `fog.forget`, `fog.undo` on the API (the operator's own): `preview: true` gives the card alone.
pub fn api(node: &Node, op: &str, req: &Value) -> Result<Value, String> {
    if req["preview"] == true {
        return card(node, op, req).map(|c| json!({"done": false, "card": c}));
    }
    apply(node, op, req)
}

/// `fog.mine`: what this arcade keeps by choice, pins, forgot and hands over, its keep share, and its blobs by role.
pub fn mine(node: &Node) -> Result<Value, String> {
    let c = snapshot();
    let segs_all = by_segment();
    let kept = fogstore::kept_segs(node);
    let borrowed = crate::arcsvc::svc_net().is_none();
    let share = fogstore::keep_mb(node);
    let used = fogstore::kept_mb_used(node);
    let mut chosen = Vec::new();
    let mut pins = Vec::new();
    let mut forgotten = Vec::new();
    let mut handing = Vec::new();
    for e in &c.entries {
        match e.kind.as_str() {
            "keep" => {
                let segs: Vec<Value> = e
                    .segs
                    .iter()
                    .map(|(f, i)| {
                        let n = seg_now(node, &segs_all, *f, *i);
                        let (_, m) = pay_estimate(n.bytes, n.copies.max(1));
                        json!({"segment": crate::fogmap::seg_name(*f, *i), "bytes": n.bytes, "blobs": n.blobs, "copies": n.copies, "floor": n.floor,
                               "kept_now": kept.contains(&(*f, *i)), "listed": n.listed, "pay_month": if kept.contains(&(*f, *i)) { m } else { 0 }})
                    })
                    .collect();
                chosen.push(json!({"id": e.id, "what": e.what, "key": e.key, "label": e.label, "at": e.at,
                                   "bytes": segs.iter().filter_map(|s| s["bytes"].as_u64()).sum::<u64>(),
                                   "pay_month": segs.iter().filter_map(|s| s["pay_month"].as_u64()).sum::<u64>(), "segments": segs}));
            }
            "pin" => {
                let bytes: u64 = e.blobs.iter().filter_map(|b| hex_id(b).ok()).map(|b| crate::arcblob::len_of(&b).unwrap_or(0)).sum();
                let here = e.blobs.iter().filter_map(|b| hex_id(b).ok()).all(|b| crate::arcblob::bytes_here(&b));
                let seg = hex_id(&e.key).ok().and_then(|b| key_of(&b).seg);
                let n = seg.map(|(f, i)| seg_now(node, &segs_all, f, i));
                pins.push(json!({"id": e.id, "blob": e.key, "label": e.label, "at": e.at, "bytes": bytes, "here": here, "parts": e.blobs.len(),
                                 "segment": seg.map(|(f, i)| crate::fogmap::seg_name(f, i)), "copies": n.as_ref().map(|n| n.copies), "floor": n.as_ref().map(|n| n.floor), "pay_month": 0}));
            }
            _ => {
                for h in &e.handover {
                    handing.push(json!({"id": e.id, "segment": crate::fogmap::seg_name(h.family, h.index), "since": h.since, "before": h.before, "floor": h.floor,
                                        "need": h.floor.min(h.before), "now": h.now, "listed": h.listed, "done": h.done, "note": h.note,
                                        "label": if h.done.is_some() { format!("handed over ({}/{})", h.now, h.floor) } else { format!("handing over ({}/{})", h.now, h.floor) }}));
                }
                forgotten.push(json!({"id": e.id, "what": e.what, "key": e.key, "label": e.label, "at": e.at, "why": "forgotten by you",
                                      "blobs": e.blobs.len(), "unpinned_own": e.was_mine.len(),
                                      "handing_over": e.handover.iter().any(|h| h.done.is_none())}));
            }
        }
    }
    // Every blob held here by role (as the fog map counts them), and how much of the map is in the fog for it.
    let now = arcade_app::now();
    let grace = fogstore::grace_secs();
    let known = crate::arcblob::known();
    let held: BTreeSet<Hash> = crate::arcblob::held_ids().into_iter().collect();
    let mut roles = json!({"keeper": 0, "publisher": 0, "grace": 0, "viewer": 0, "pinned": 0});
    let mut held_bytes = 0u64;
    for (id, m) in &known {
        if !held.contains(id) {
            continue;
        }
        held_bytes += crate::arcblob::size_here(id);
        let k = key_of(id);
        let r = if k.seg.is_some_and(|s| kept.contains(&s)) {
            "keeper"
        } else if crate::arcblob::is_mine(id) {
            "publisher"
        } else if c.pinned(&k.id) {
            "pinned"
        } else if m.time.saturating_add(grace) > now {
            "grace"
        } else {
            "viewer"
        };
        roles[r] = json!(roles[r].as_u64().unwrap_or(0) + 1);
    }
    let held_known = known.iter().filter(|(id, _)| held.contains(id)).count();
    Ok(json!({
        "node": node.id_hex(),
        "chain": crate::arcsvc::defined().map(|p| p.key),
        "borrowed": borrowed,
        "can_keep": cant_keep(node).is_none(),
        "why_not": cant_keep(node),
        "keep_share": {"mb": share, "used_mb": used, "free_mb": share.saturating_sub(used), "none": share == 0,
                       "label": if borrowed { "none · borrows everything".to_string() } else if share == 0 { "none (Off)".to_string() } else { format!("{used} of {share} MB used") },
                       "options": "Options → Keep ARCADE data (arc.keep)"},
        "kept_segments": kept.iter().map(|(f, i)| {
            // Whether this arcade's own copy counts now (registered and audited), and the segment's copies.
            let held = fogstore::seg_holders(node, *f, *i);
            let me = node.id_hex();
            let n = seg_now(node, &segs_all, *f, *i);
            json!({"segment": crate::fogmap::seg_name(*f, *i), "chosen": c.chosen().contains(&(*f, *i)), "handing_over": c.handing_over().contains(&(*f, *i)),
                   "counted_here": held.registered.as_ref().is_some_and(|r| r.iter().any(|x| x.0 == me && x.1)), "listed": held.registered.is_some(),
                   "copies": held.counted(), "keepers": held.keepers.len(), "floor": n.floor, "bytes": n.bytes, "blobs": n.blobs})
        }).collect::<Vec<_>>(),
        "chosen": chosen,
        "pins": pins,
        "forgotten": forgotten,
        "handing_over": handing,
        "totals": {"roles": roles, "held": held_known, "held_bytes": held_bytes, "known": known.len(), "in_the_fog": known.len() - held_known,
                   "fog_share": if known.is_empty() { 0.0 } else { (known.len() - held_known) as f64 / known.len() as f64 }},
        "commands": ["fog.keep", "fog.forget", "fog.undo", "fog.mine"],
    }))
}

// ---------------- app requests (POST /r/fog/<op>) ----------------

/// `POST /r/fog/<keep|forget|undo>` from app page `page`: checked now (its card), then filed in the wallet's approval
/// queue for Arcade V1.0 to show; nothing changes until the operator approves.
pub fn file(node: &Node, page: &str, op: &str, b: &Value) -> (u16, Value) {
    let c = match card(node, op, b) {
        Ok(c) => c,
        Err(e) => return (400, json!({"error": e})),
    };
    let mut args = json!({});
    for k in ["segment", "blob", "publisher", "mode", "id"] {
        if !b[k].is_null() {
            args[k] = b[k].clone();
        }
    }
    let what = c["title"].as_str().unwrap_or("change what this arcade keeps").to_string();
    let r = json!({"id": arcade_mesh::random::hex(8), "page": page, "kind": "fog", "op": op, "args": args, "what": what,
                   "to": "this arcade's storage", "to_tag": Value::Null, "units": 0, "status": "pending", "created": arcade_app::now(),
                   "txid": Value::Null, "error": Value::Null, "fog": {"state": "asked", "card": c}});
    crate::walletsvc::file_other(node, r)
}

/// Approved (crate::walletsvc): applied now. (txid "", fee 0, no outputs: nothing is written on a chain.)
pub fn approved(node: &Node, r: &Value) -> Result<(String, u64, Vec<Value>), String> {
    let res = apply(node, r["op"].as_str().unwrap_or(""), &r["args"])?;
    if let Some(mut x) = crate::walletsvc::get_request(node, r["id"].as_str().unwrap_or("")) {
        x["fog"] = json!({"state": "done", "result": res});
        crate::walletsvc::put_request(node, &x);
    }
    Ok((String::new(), 0, vec![]))
}

/// `GET /r/fog/req/<id>` for the page that filed it.
pub fn status(node: &Node, page: &str, id: &str) -> (u16, Value) {
    match crate::walletsvc::get_request(node, id) {
        Some(r) if r["page"] == page && r["kind"] == "fog" => (200, json!({"id": id, "status": r["status"], "op": r["op"], "what": r["what"], "fog": r["fog"], "error": r["error"]})),
        _ => (404, json!({"error": "no such request"})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bk(id: &str, publisher: &str, seg: (u8, u64)) -> BlobKey {
        BlobKey { id: id.into(), publisher: publisher.into(), seg: Some(seg) }
    }

    fn forget(what: &str, key: &str, segs: Vec<(u8, u64)>) -> Entry {
        Entry { id: format!("f{key}"), kind: "forget".into(), what: what.into(), key: key.into(), segs, ..Default::default() }
    }

    #[test]
    fn the_forget_list_covers_blobs_publishers_and_segments() {
        let c = Choices { entries: vec![forget("blob", "aa", vec![]), forget("publisher", "Pub1", vec![]), forget("segment", "P#2", vec![(FAMILY_BLOBS, 2)])], ..Default::default() };
        let none = BTreeSet::new();
        assert!(c.refused(&bk("aa", "x", (FAMILY_BLOBS, 0)), &none), "a forgotten blob");
        assert!(c.refused(&bk("bb", "Pub1", (FAMILY_BLOBS, 0)), &none), "a forgotten publisher's blob");
        assert!(c.refused(&bk("cc", "x", (FAMILY_BLOBS, 2)), &none), "a blob of a forgotten segment");
        assert!(!c.refused(&bk("cc", "x", (FAMILY_BLOBS_TEMP, 2)), &none), "the other family's segment 2 isn't forgotten");
        assert!(!c.refused(&bk("dd", "y", (FAMILY_BLOBS, 0)), &none));
        // A forgotten segment still kept here (handing over) is served until it's dropped; a forgotten blob never is.
        let kept: BTreeSet<(u8, u64)> = [(FAMILY_BLOBS, 2), (FAMILY_BLOBS, 0)].into();
        assert!(!c.refused(&bk("cc", "x", (FAMILY_BLOBS, 2)), &kept));
        assert!(c.refused(&bk("aa", "x", (FAMILY_BLOBS, 0)), &kept));
        assert!(c.seg_forgotten(FAMILY_BLOBS, 2) && !c.seg_forgotten(FAMILY_BLOBS, 0));
    }

    #[test]
    fn pins_and_chosen_segments() {
        let c = Choices {
            entries: vec![
                Entry { id: "p1".into(), kind: "pin".into(), what: "blob".into(), key: "aa".into(), blobs: vec!["aa".into(), "ab".into()], ..Default::default() },
                Entry { id: "k1".into(), kind: "keep".into(), what: "publisher".into(), key: "Pub".into(), segs: vec![(FAMILY_BLOBS, 0), (FAMILY_BLOBS_TEMP, 0)], ..Default::default() },
            ],
            ..Default::default()
        };
        assert!(c.pinned("aa") && c.pinned("ab") && !c.pinned("ac"), "a large blob's parts are pinned with it");
        assert_eq!(c.chosen().len(), 2);
        assert!(c.handing_over().is_empty());
    }

    fn ho(before: usize, floor: usize, listed: bool) -> Handover {
        Handover { family: FAMILY_BLOBS, index: 0, before, floor, listed, ..Default::default() }
    }

    #[test]
    fn a_hand_over_waits_for_the_new_keeper_to_be_audited() {
        let me = "me".to_string();
        let leaving = BTreeSet::new();
        let none = |_: &str| false;
        // Listed, 3 of 3 with this arcade: the other two audited aren't enough; a new keeper registered but not audited
        // isn't either; once it passes its audit, drop.
        let h = ho(3, 3, true);
        let held = |reg: Vec<(&str, bool)>| SegHolders { registered: Some(reg.into_iter().map(|(a, b)| (a.to_string(), b)).collect()), keepers: vec![] };
        let n = counted_without(&me, &held(vec![("me", true), ("b", true), ("c", true)]), &leaving, &none);
        assert!(matches!(handover_step(&h, n, true), Step::Wait { now: 2, need: 3, .. }));
        let n = counted_without(&me, &held(vec![("me", true), ("b", true), ("c", true), ("d", false)]), &leaving, &none);
        assert!(matches!(handover_step(&h, n, true), Step::Wait { now: 2, .. }), "registered, not audited yet");
        let n = counted_without(&me, &held(vec![("me", true), ("b", true), ("c", true), ("d", true)]), &leaving, &none);
        assert_eq!(handover_step(&h, n, true), Step::Drop);
        // Above the floor when forgotten (4 of 3): drops at once, the floor holds without it.
        assert_eq!(handover_step(&ho(4, 3, true), 3, true), Step::Drop);
        // Another arcade leaving too doesn't count as a replacement.
        let also: BTreeSet<String> = ["d".to_string()].into();
        let n = counted_without(&me, &held(vec![("me", true), ("b", true), ("c", true), ("d", true)]), &also, &none);
        assert_eq!(n, 2);
        // Nobody with room: stays a keeper and says so.
        match handover_step(&h, 2, false) {
            Step::Wait { why, .. } => assert!(why.contains("no arcade has room") && why.contains("stays a keeper")),
            s => panic!("{s:?}"),
        }
    }

    #[test]
    fn an_open_segment_hands_over_to_a_keeper_holding_every_blob() {
        let me = "me".to_string();
        let held = SegHolders { registered: None, keepers: vec!["me".into(), "b".into(), "c".into()] };
        let h = ho(2, 3, false);
        // c announces keeping it but doesn't hold its blobs yet: only b counts.
        let ready_b = |k: &str| k == "b";
        let n = counted_without(&me, &held, &BTreeSet::new(), &ready_b);
        assert!(matches!(handover_step(&h, n, true), Step::Wait { now: 1, need: 2, .. }));
        let ready_bc = |k: &str| k == "b" || k == "c";
        let n = counted_without(&me, &held, &BTreeSet::new(), &ready_bc);
        assert_eq!(handover_step(&h, n, true), Step::Drop);
    }

    #[test]
    fn segment_names_parse() {
        assert_eq!(parse_seg("P#3"), Ok((FAMILY_BLOBS, 3)));
        assert_eq!(parse_seg("t#0"), Ok((FAMILY_BLOBS_TEMP, 0)));
        assert_eq!(parse_seg("p12"), Ok((FAMILY_BLOBS, 12)));
        assert!(parse_seg("Q#1").is_err() && parse_seg("P#x").is_err());
        assert!(target(&json!({"segment": "P#0", "blob": "aa"})).is_err());
        assert!(target(&json!({})).is_err());
        assert_eq!(target(&json!({"publisher": "@x"})).unwrap().0, "publisher");
    }

    #[test]
    fn released_sealed_copies_are_never_sealed_again() {
        let c = Choices { released: vec![("arctest".into(), FAMILY_BLOBS, 0, RANGE - 1)], ..Default::default() };
        assert_eq!(c.released_end("arctest", FAMILY_BLOBS), RANGE);
        assert_eq!(c.released_end("arctest", FAMILY_BLOBS_TEMP), 0);
    }

    #[test]
    fn refused_and_pinned_read_the_saved_choices() {
        // The one test that sets the shared choices (no blob store here: keys from the id alone).
        let id = [7u8; 32];
        set_for_test(Choices { entries: vec![forget("blob", &hex::encode(id), vec![]),
                                             Entry { id: "p".into(), kind: "pin".into(), blobs: vec![hex::encode([8u8; 32])], ..Default::default() }], ..Default::default() });
        assert!(refused(&id));
        assert!(!refused(&[9u8; 32]));
        assert!(snapshot().pinned(&hex::encode([8u8; 32])) && !snapshot().pinned(&hex::encode(id)));
        set_for_test(Choices::default());
    }
}
