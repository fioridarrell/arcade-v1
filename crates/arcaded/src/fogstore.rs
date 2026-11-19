//! Fog-of-war storage for the ARCADE chain's blobs (rules version 5; docs/FOGSTORE.md; Martin 2026-11-18).
//!
//! No arcade has to keep every blob's bytes. From rules version 5 a block is valid with its blobs' records only, and
//! each arcade keeps the bytes of:
//!
//! - **its own**: blobs it published (a `<id>.mine` mark beside them), for good;
//! - **new ones**: every blob for its first [`grace_secs`] (7 days), so new data outlives its publisher going away;
//! - **segments it keeps by choice** (`arc.keep`): whole blob-family segments, sealed, audited and paid. That is the
//!   network's redundancy. Segments short of their copy floor are taken first;
//! - **what was viewed or served through it** (fog of war): kept and served from then on, dropped once nobody has
//!   viewed or asked for it here for [`idle_secs`] (30 days). Unaudited, a bonus.
//!
//! **Copies count only when audited.** A segment's copies are the registry's replicas of its catalog entry
//! (`Registry::holders_of`): sealed, registered, and still passing audits (a replica goes after
//! `params::MAX_MISSED_AUDITS` misses). That state is the chain's, so every arcade counts the same. While a segment is
//! still open (its range not complete, so nothing to register yet), its keepers are those that announce keeping it. That
//! count is only used to decide who takes a segment; it never replaces the registry's once the segment is listed.
//!
//! **Floors**: a blob-family segment needs [`blob::floor`] of its strongest class (Permanent 7, Release 5); a
//! Temporary segment ([`segment::FAMILY_BLOBS_TEMP`]) needs 3 until its time is up, then none. Below its floor a segment
//! is **short**. Arcades with room in their keep share step in, ranked by a hash of node id and segment, so they don't
//! all take it at once, until the floor is met. A keeper above the floor whose rank is the worst lets go. A keeper that
//! goes down misses audits, its replica is removed, the segment is short, and the next-ranked arcade with room takes it.
//! With nobody having room it stays short and is shown as a warning, never forced on anyone.
//!
//! **Holder announcements** (in the "sh hold" frame, crate::shares): a bloom filter of the blob ids held here (bytes,
//! plain or cached), the segments kept by choice and the free room. They're only for finding someone to fetch from, and
//! for ranking who steps in.

use crate::node::Node;
use arcade_arc::blob::{self, Class};
use arcade_arc::merkle::Hash;
use arcade_arc::params::{self, Net, RANGE};
use arcade_arc::segment::{FAMILY_BLOBS, FAMILY_BLOBS_TEMP};
use serde_json::{json, Value};
use sha2::Digest;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Whether rules version 5 holds for the block at `height` of `net`.
pub fn on(net: &Net, height: u64) -> bool {
    params::rules_v5(net, height)
}

fn env_secs(key: &str, default: u64) -> u64 {
    if crate::arcsvc::local() {
        if let Some(v) = std::env::var(key).ok().and_then(|v| v.parse().ok()) {
            return v;
        }
    }
    default
}

/// How long every arcade keeps a new blob: 7 days (a closed test net: `ARCADE_FOG_GRACE_SECS`).
pub fn grace_secs() -> u64 {
    env_secs("ARCADE_FOG_GRACE_SECS", 7 * 86_400)
}

/// How long a blob viewed or served here stays without being viewed or asked for again: 30 days
/// (`ARCADE_FOG_IDLE_SECS` on a closed test net).
pub fn idle_secs() -> u64 {
    env_secs("ARCADE_FOG_IDLE_SECS", 30 * 86_400)
}

// ---------------- keep by choice ----------------

const KEEP_MB: &[u8] = b"arc_keep_mb";
const KEEP_SEGS: &[u8] = b"arc_keep_segs";

/// The share of the chain this arcade keeps by choice, in MB (0: none).
pub fn keep_mb(node: &Node) -> u64 {
    node.app.store.read(|r| r.get(arcade_app::META, KEEP_MB)).ok().flatten().and_then(|v| String::from_utf8(v).ok()?.parse().ok()).unwrap_or(0)
}

/// The blob-family segments kept by choice: (family, range index).
pub fn kept_segs(node: &Node) -> BTreeSet<(u8, u64)> {
    node.app
        .store
        .read(|r| r.get(arcade_app::META, KEEP_SEGS))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_slice::<Vec<(u8, u64)>>(&v).ok())
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn save_segs(node: &Node, segs: &BTreeSet<(u8, u64)>) {
    let v: Vec<(u8, u64)> = segs.iter().copied().collect();
    let _ = node.app.store.write(|w| w.put(arcade_app::META, KEEP_SEGS, &serde_json::to_vec(&v).unwrap_or_default()));
}

/// `arc.keep`: the share kept by choice (`mb`, 0 or `off` for none) and what it keeps.
pub fn api_keep(node: &Node, req: &Value) -> Result<Value, String> {
    let set = if req["mb"] == "off" { Some(0) } else { req["mb"].as_u64() };
    if let Some(mb) = set {
        node.app.store.write(|w| w.put(arcade_app::META, KEEP_MB, mb.to_string().as_bytes())).map_err(|e| e.to_string())?;
        if mb == 0 {
            save_segs(node, &BTreeSet::new());
        }
    }
    Ok(json!({"mb": keep_mb(node), "segments": kept_segs(node).into_iter().map(|(f, i)| json!({"family": family_name(f), "first": i * RANGE, "last": i * RANGE + RANGE - 1})).collect::<Vec<_>>(),
              "used_mb": kept_bytes(node) >> 20}))
}

/// The family name `arc.fog` lists segments under (for the storage reads).
pub fn family_label(f: u8) -> &'static str {
    family_name(f)
}

fn family_name(f: u8) -> &'static str {
    if f == FAMILY_BLOBS_TEMP { "temporary blobs" } else { "blobs" }
}

/// Whether this arcade keeps any segment of `family` by choice (so it seals that family).
pub fn keeps_any(node: &Node, family: u8) -> bool {
    kept_segs(node).iter().any(|(f, _)| *f == family)
}

/// The family a blob of `class` is sealed in from rules version 5.
pub fn family_of(class: u8) -> u8 {
    if class == Class::Temporary as u8 || class == Class::Boost as u8 { FAMILY_BLOBS_TEMP } else { FAMILY_BLOBS }
}

fn kept_bytes(node: &Node) -> u64 {
    let segs = kept_segs(node);
    crate::arcblob::known().iter().filter(|(_, m)| segs.contains(&(family_of(m.class), m.height / RANGE))).map(|(id, _)| crate::arcblob::size_here(id)).sum()
}

// ---------------- the holder announcement ----------------

const BLOOM_K: usize = 6;
/// Most bytes of a bloom in an announcement (as hex, twice that): 16 kB, so about 13,000 blobs at 10 bits each.
const BLOOM_MAX: usize = 16 << 10;

fn bloom_bits(id: &Hash, m: usize) -> impl Iterator<Item = usize> {
    let h = sha2::Sha256::digest([b"fogbloom".as_slice(), id].concat());
    (0..BLOOM_K).map(move |i| u32::from_le_bytes([h[4 * i], h[4 * i + 1], h[4 * i + 2], h[4 * i + 3]]) as usize % m)
}

/// A bloom filter over `ids`, as hex.
pub fn bloom(ids: &[Hash]) -> String {
    let bytes = (ids.len() * 10).div_ceil(8).clamp(128, BLOOM_MAX).next_power_of_two().min(BLOOM_MAX);
    let m = bytes * 8;
    let mut b = vec![0u8; bytes];
    for id in ids {
        for bit in bloom_bits(id, m) {
            b[bit / 8] |= 1 << (bit % 8);
        }
    }
    hex::encode(b)
}

/// Whether a bloom (hex) may hold `id`.
pub fn bloom_has(hex_bloom: &str, id: &Hash) -> bool {
    let Ok(b) = hex::decode(hex_bloom) else { return false };
    if b.is_empty() {
        return false;
    }
    let m = b.len() * 8;
    bloom_bits(id, m).all(|bit| b[bit / 8] & (1 << (bit % 8)) != 0)
}

/// Free room in the keep share, in MB.
fn room_mb(node: &Node) -> u64 {
    keep_mb(node).saturating_sub(kept_bytes(node) >> 20)
}

/// What the "sh hold" frame says of the blobs here (crate::shares::hold_frame, under the ARCADE chain).
pub fn announce(node: &Node) -> Value {
    let ids = crate::arcblob::held_ids();
    let keep: Vec<Value> = kept_segs(node).into_iter().take(256).map(|(f, i)| json!([f, i])).collect();
    json!({"bf": bloom(&ids), "bn": ids.len(), "keep": keep, "room": room_mb(node)})
}

/// An announcement as kept from another arcade: only these fields, each in its form and size.
pub fn clean(v: &Value) -> Option<Value> {
    let bf = v["bf"].as_str().filter(|s| s.len() <= 2 * BLOOM_MAX && s.bytes().all(|c| c.is_ascii_hexdigit()))?;
    let keep: Vec<Value> = v["keep"].as_array().map(|a| a.iter().take(256).filter_map(|x| Some(json!([x[0].as_u64().filter(|f| *f < 256)?, x[1].as_u64()?]))).collect()).unwrap_or_default();
    Some(json!({"bf": bf, "bn": v["bn"].as_u64().unwrap_or(0), "keep": keep, "room": v["room"].as_u64().unwrap_or(0)}))
}

/// How old an arcade's last announcement may be and still count it as keeping an open segment, or as a candidate:
/// a day (`ARCADE_FOG_STALE_SECS` on a closed test net), so a restart doesn't move segments. Listed segments count the
/// registry's audited replicas instead (a keeper that is gone misses its audits).
pub fn stale_secs() -> u64 {
    env_secs("ARCADE_FOG_STALE_SECS", 86_400)
}

/// Every other arcade's announcement (node id → its "fog" part), from the holds the mesh told us, newer than
/// [`stale_secs`].
fn announcements(node: &Node) -> Vec<(String, Value)> {
    let Some(net) = crate::arcsvc::defined().map(|p| p.net) else { return Vec::new() };
    let now = arcade_app::now();
    let stale = stale_secs();
    node.holds
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, (t, v))| v["_at"].as_u64().unwrap_or(*t).saturating_add(stale) >= now)
        .filter_map(|(id, (_, v))| {
            let f = v[net]["fog"].clone();
            f.is_object().then(|| (id.clone(), f))
        })
        .collect()
}

/// Arcades that announce holding blob `id`'s bytes: whom to fetch it from.
pub fn holders(node: &Node, id: &Hash) -> Vec<String> {
    announcements(node).into_iter().filter(|(_, f)| f["bf"].as_str().is_some_and(|b| bloom_has(b, id))).map(|(n, _)| n).collect()
}

// ---------------- floors and copies ----------------

/// A segment's copy floor: its strongest class's (Permanent 7, Release 5); a Temporary segment 3 until its time is up
/// (the last blob in it older than `temporary` seconds), then 0; an empty one 0.
pub fn floor_of(family: u8, metas: &[&crate::arcblob::Meta], now: u64, temporary: u64) -> usize {
    if metas.is_empty() {
        return 0;
    }
    if family == FAMILY_BLOBS_TEMP {
        let last = metas.iter().map(|m| m.time.max(m.boosted_until.saturating_sub(temporary))).max().unwrap_or(0);
        return if last.saturating_add(temporary) > now { blob::floor(Class::Temporary) } else { 0 };
    }
    if metas.iter().any(|m| m.class == Class::Permanent as u8) {
        blob::floor(Class::Permanent)
    } else {
        blob::floor(Class::Release)
    }
}

/// Where a segment's copies come from: its catalog entry's audited replicas once it's listed, else the arcades
/// announcing they keep it while it is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Counted {
    Registered,
    Announced,
}

/// Rank of `node_id` for segment (family, index): lower takes it first.
pub fn rank(node_id: &str, family: u8, index: u64) -> [u8; 32] {
    sha2::Sha256::digest([node_id.as_bytes(), &[family], &index.to_le_bytes()].concat()).into()
}

/// What a round decides for one segment, from what it's told (pure, for the tests): whether this arcade should keep
/// it. `copies` the counted copies with this arcade's own among them when `mine_kept`; `candidates` the arcades that could
/// take it (announcing room or keeping it already), this one included when it has room or keeps it.
pub fn decide(me: &str, family: u8, index: u64, floor: usize, copies: usize, mine_kept: bool, keepers: &[String], candidates: &[String]) -> bool {
    if floor == 0 {
        return false;
    }
    if mine_kept {
        // Above the floor: the keeper ranked last among the keepers lets go.
        if copies > floor {
            let worst = keepers.iter().max_by_key(|k| rank(k, family, index));
            return worst.is_none_or(|w| w != me);
        }
        return true;
    }
    if copies >= floor {
        return false;
    }
    // Short: the best-ranked candidates not keeping it yet step in, as many as are missing.
    let mut free: Vec<&String> = candidates.iter().filter(|c| !keepers.contains(c)).collect();
    free.sort_by_key(|c| rank(c, family, index));
    free.iter().take(floor - copies).any(|c| *c == me)
}

/// One segment as the status shows it.
#[derive(Clone, Debug)]
pub struct Seg {
    pub family: u8,
    pub index: u64,
    pub floor: usize,
    pub copies: usize,
    pub counted: Counted,
    pub bytes: u64,
}

/// The copies of a segment: the registry's replicas when it is listed, else the announced keepers (and this arcade).
fn copies_of(node: &Node, family: u8, index: u64) -> (usize, Counted, Vec<String>) {
    let me = node.id_hex();
    let mut keepers: Vec<String> = announcements(node).into_iter().filter(|(_, f)| f["keep"].as_array().is_some_and(|a| a.iter().any(|x| x[0] == family as u64 && x[1] == index))).map(|(n, _)| n).collect();
    if kept_segs(node).contains(&(family, index)) {
        keepers.push(me);
    }
    match crate::arcsvc::segment_replicas(family, index * RANGE) {
        Some(n) => (n as usize, Counted::Registered, keepers),
        None => (keepers.len(), Counted::Announced, keepers),
    }
}

// ---------------- the round ----------------

static STATUS: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);
static NODE: std::sync::OnceLock<std::sync::Weak<Node>> = std::sync::OnceLock::new();

/// [`holders`] for code without the node at hand (crate::arcblob::view): empty before the first round.
pub fn holders_now(id: &Hash) -> Vec<String> {
    NODE.get().and_then(|w| w.upgrade()).map(|n| holders(&n, id)).unwrap_or_default()
}

/// Ask for a link to an arcade that announces holding a blob (through a relay, or UDP): for the next try.
pub fn want_link(peer: &str) {
    crate::udpsvc::want(peer);
    if let Some(n) = NODE.get().and_then(|w| w.upgrade()) {
        let _ = crate::relaysvc::want(&n, peer);
    }
}

/// One round (the blob pruner's thread, every few minutes): take or let go of segments, fetch what kept segments lack,
/// and drop the bytes nothing keeps here any more. Blobs from before rules version 5 are left to the old classes.
pub fn round(node: &Node, net: &Net, temporary: u64) {
    let _ = NODE.set(std::sync::Arc::downgrade(&node.self_arc()));
    if !crate::arcsvc::archive(node) {
        let _ = round_inner(node, net, temporary, false);
    } else {
        let _ = round_inner(node, net, temporary, true);
    }
}

fn round_inner(node: &Node, net: &Net, temporary: u64, archive: bool) -> Option<()> {
    let now = arcade_app::now();
    let me = node.id_hex();
    let known: Vec<(Hash, crate::arcblob::Meta)> = crate::arcblob::known().into_iter().filter(|(_, m)| on(net, m.height)).collect();
    // Large blobs (crate::largeblob): every arcade keeps the small manifests, so it knows which parts go together.
    let mut got_manifest = 0;
    for (id, m) in &known {
        if is_manifest(m) && !crate::arcblob::bytes_here(id) && got_manifest < 16 && crate::arcblob::fetch_keep(node, id) {
            got_manifest += 1;
        }
    }
    crate::largeblob::refresh();
    let metas: HashMap<Hash, &crate::arcblob::Meta> = known.iter().map(|(id, m)| (*id, m)).collect();
    let mut by_seg: BTreeMap<(u8, u64), Vec<&crate::arcblob::Meta>> = BTreeMap::new();
    let mut seg_bytes: HashMap<(u8, u64), u64> = HashMap::new();
    for (_, m) in &known {
        let key = (family_of(m.class), m.height / RANGE);
        by_seg.entry(key).or_default().push(m);
    }
    for (id, m) in &known {
        *seg_bytes.entry((family_of(m.class), m.height / RANGE)).or_default() += crate::arcblob::len_of(id).unwrap_or(0);
    }
    let ann = announcements(node);
    let keep_on = keep_mb(node) > 0;
    let mut segs = kept_segs(node);
    let mut room = room_mb(node) << 20;
    let mut short = Vec::new();
    let mut warnings = Vec::new();
    let mut seg_info: HashMap<(u8, u64), (usize, usize)> = HashMap::new();
    for (&(family, index), metas) in &by_seg {
        let floor = floor_of(family, metas, now, temporary);
        let (copies, counted, keepers) = copies_of(node, family, index);
        seg_info.insert((family, index), (copies, floor));
        let mine_kept = segs.contains(&(family, index));
        let mut candidates: Vec<String> = ann.iter().filter(|(_, f)| f["room"].as_u64().unwrap_or(0) > 0).map(|(n, _)| n.clone()).collect();
        let size = seg_bytes.get(&(family, index)).copied().unwrap_or(0);
        if keep_on && (mine_kept || room >= size) {
            candidates.push(me.clone());
        }
        let keep = decide(&me, family, index, floor, copies, mine_kept, &keepers, &candidates);
        if keep && !mine_kept {
            segs.insert((family, index));
            room = room.saturating_sub(size);
            eprintln!("arc: fog: keeping the {} segment {}..{} ({copies} of {floor} copies)", family_name(family), index * RANGE, index * RANGE + RANGE - 1);
        } else if !keep && mine_kept {
            segs.remove(&(family, index));
            eprintln!("arc: fog: letting the {} segment {}..{} go ({copies} copies, floor {floor})", family_name(family), index * RANGE, index * RANGE + RANGE - 1);
        }
        if copies < floor {
            let s = Seg { family, index, floor, copies, counted, bytes: size };
            if candidates.iter().all(|c| keepers.contains(c)) && !segs.contains(&(family, index)) {
                warnings.push(format!("the {} segment {}..{} has {copies} of its {floor} copies and no arcade has room to keep it", family_name(family), index * RANGE, index * RANGE + RANGE - 1));
            }
            short.push(s);
        }
    }
    save_segs(node, &segs);
    // A large blob's parts live and fade together: one kept segment holding a part keeps the whole (its other parts
    // fetched too, wherever they are), and while any part is the publisher's, new, or viewed or asked for here, none is
    // dropped.
    let idle = idle_secs();
    let grace = grace_secs();
    let in_kept = |id: &Hash| metas.get(id).is_some_and(|m| segs.contains(&(family_of(m.class), m.height / RANGE)));
    let alive = |id: &Hash| {
        crate::arcblob::is_mine(id)
            || in_kept(id)
            || metas.get(id).is_some_and(|m| m.time.saturating_add(grace) > now)
            || crate::arcblob::last_use(id).is_some_and(|t| t.saturating_add(idle) > now)
    };
    let mut held_whole: std::collections::HashSet<Hash> = std::collections::HashSet::new();
    let mut whole_fetch: Vec<Hash> = Vec::new();
    let groups = crate::largeblob::all();
    for (mid, man) in &groups {
        let members: Vec<Hash> = std::iter::once(*mid).chain(man.parts.iter().map(|p| p.0)).collect();
        if members.iter().any(|m| alive(m)) {
            held_whole.extend(members.iter().copied());
        }
        if man.parts.iter().any(|p| in_kept(&p.0)) {
            whole_fetch.extend(man.parts.iter().map(|p| p.0).filter(|p| !crate::arcblob::here(p) && !in_kept(p)));
        }
    }
    // Fetch what a kept segment lacks, and the rest of the large blobs it keeps a part of (not a view: it doesn't count
    // as one).
    let mut fetched = 0;
    let wanted = known.iter().filter(|(id, m)| segs.contains(&(family_of(m.class), m.height / RANGE)) && !crate::arcblob::here(id)).map(|(id, _)| *id).chain(whole_fetch);
    for id in wanted {
        if fetched >= 64 {
            break;
        }
        if crate::arcblob::fetch_keep(node, &id) {
            fetched += 1;
        }
    }
    // Drop the bytes nothing here keeps.
    let (mut dropped, mut freed) = (0, 0u64);
    if !archive {
        for (id, m) in &known {
            if crate::arcblob::is_mine(id) {
                continue;
            }
            let temp_done = family_of(m.class) == FAMILY_BLOBS_TEMP && m.time.saturating_add(temporary).max(m.boosted_until) <= now;
            if temp_done {
                freed += crate::arcblob::forget(id);
                dropped += 1;
                continue;
            }
            if is_manifest(m) || held_whole.contains(id) {
                continue;
            }
            let kept = segs.contains(&(family_of(m.class), m.height / RANGE));
            let new = m.time.saturating_add(grace) > now;
            let used = crate::arcblob::last_use(id).is_some_and(|t| t.saturating_add(idle) > now);
            if !kept && !new && !used && crate::arcblob::plain_here(id) {
                freed += crate::arcblob::drop_plain(id);
                dropped += 1;
            }
        }
        freed += crate::arcblob::cache_drop_idle(now.saturating_sub(idle));
    }
    if dropped > 0 {
        eprintln!("arc: fog: {dropped} blobs' bytes dropped here ({freed} bytes)");
    }
    let short_json: Vec<Value> = short
        .iter()
        .map(|s| json!({"family": family_name(s.family), "first": s.index * RANGE, "last": s.index * RANGE + RANGE - 1, "copies": s.copies, "floor": s.floor,
                        "counted": if s.counted == Counted::Registered { "audited replicas" } else { "announced keepers (not listed yet)" }, "bytes": s.bytes}))
        .collect();
    // Each large blob as one item: its parts' copies are its segments' (the fewest), its floor its class's.
    let mut large = Vec::new();
    for (mid, man) in groups.iter().filter(|g| g.1.large()) {
        let segs_of: Vec<(u8, u64)> = man.parts.iter().filter_map(|p| metas.get(&p.0).map(|m| (family_of(m.class), m.height / RANGE))).collect();
        let infos: Vec<(usize, usize)> = segs_of.iter().filter_map(|k| seg_info.get(k).copied()).collect();
        let copies = infos.iter().map(|i| i.0).min().unwrap_or(0);
        let floor = infos.iter().map(|i| i.1).max().unwrap_or(0);
        let here = man.parts.iter().filter(|p| crate::arcblob::bytes_here(&p.0)).count();
        large.push(json!({"id": hex::encode(mid), "type": man.kind, "bytes": man.size, "parts": man.parts.len(), "parts_here": here,
                          "class": man.class, "copies": copies, "floor": floor, "short": copies < floor,
                          "segments": segs_of.iter().collect::<BTreeSet<_>>().len(), "mine": crate::arcblob::is_mine(mid)}));
    }
    large.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    let short_large: Vec<Value> = large.iter().filter(|l| l["short"] == true).cloned().collect();
    let held = crate::arcblob::held_ids();
    *STATUS.lock().unwrap() = Some(json!({
        "large": large.iter().take(200).collect::<Vec<_>>(), "short_large": short_large,
        "on": true, "grace_secs": grace_secs(), "idle_secs": idle_secs(), "keep_mb": keep_mb(node), "archive": archive,
        "kept": segs.iter().map(|(f, i)| json!({"family": family_name(*f), "first": i * RANGE, "last": i * RANGE + RANGE - 1})).collect::<Vec<_>>(),
        "held": held.len(), "held_ids": held.iter().take(200).map(hex::encode).collect::<Vec<_>>(),
        "mine": held.iter().filter(|id| crate::arcblob::is_mine(id)).count(),
        "short": short_json, "warnings": warnings, "fetched": fetched, "dropped": dropped, "at": now,
    }));
    Some(())
}

/// A blob class's copy floor (for the storage reads, explorer.arc_*).
pub fn blob_floor(class: Class) -> usize {
    blob::floor(class)
}

/// The segments short of their copy floor, as the last round saw them (for the storage reads, explorer.arc_*):
/// [{family, first, last, copies, floor, counted, bytes}].
pub fn short_of_copies() -> Vec<Value> {
    STATUS.lock().unwrap().as_ref().and_then(|s| s["short"].as_array().cloned()).unwrap_or_default()
}

/// The large blobs short of their copy floor, each one item, as the last round saw them (for the storage reads):
/// [{id, type, bytes, parts, parts_here, class, copies, floor, short, segments, mine}].
pub fn short_large() -> Vec<Value> {
    STATUS.lock().unwrap().as_ref().and_then(|s| s["short_large"].as_array().cloned()).unwrap_or_default()
}

/// Every large blob known here as one item (as [`short_large`]), as the last round saw them.
pub fn large() -> Vec<Value> {
    STATUS.lock().unwrap().as_ref().and_then(|s| s["large"].as_array().cloned()).unwrap_or_default()
}

/// A small file manifest (a large blob's, crate::largeblob): every arcade keeps it.
fn is_manifest(m: &crate::arcblob::Meta) -> bool {
    m.kind == crate::arcblob::FILE_MANIFEST && m.len > 0 && m.len <= crate::largeblob::MANIFEST_KEEP_MAX
}

/// `arc.fog`: what fog storage does here (the last round's view).
pub fn status() -> Value {
    let mut v = STATUS.lock().unwrap().clone().unwrap_or_else(|| json!({"on": false, "grace_secs": grace_secs(), "idle_secs": idle_secs()}));
    // What's held is read now, not as of the last round.
    let held = crate::arcblob::held_ids();
    v["held"] = json!(held.len());
    v["mine"] = json!(held.iter().filter(|id| crate::arcblob::is_mine(id)).count());
    v["held_ids"] = json!(held.iter().take(200).map(hex::encode).collect::<Vec<_>>());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("{i:064x}")).collect()
    }

    #[test]
    fn bloom_finds_what_it_holds() {
        let held: Vec<Hash> = (0..500u32).map(|i| sha2::Sha256::digest(i.to_le_bytes()).into()).collect();
        let b = bloom(&held);
        assert!(held.iter().all(|id| bloom_has(&b, id)));
        let other = (1000..3000u32).filter(|i| bloom_has(&b, &sha2::Sha256::digest(i.to_le_bytes()).into())).count();
        assert!(other < 100, "false positives {other} of 2000");
        assert!(b.len() <= 2 * BLOOM_MAX);
    }

    #[test]
    fn floors_by_class() {
        assert_eq!(blob::floor(Class::Temporary), 3);
        assert_eq!(blob::floor(Class::Release), 5);
        assert_eq!(blob::floor(Class::Permanent), 7);
        let m = |class: Class, time: u64| crate::arcblob::Meta { class: class as u8, kind: String::new(), encoding: 0, height: 1, time, line: String::new(), boosted_until: 0, len: 0 };
        let (p, r, t) = (m(Class::Permanent, 0), m(Class::Release, 0), m(Class::Temporary, 1000));
        assert_eq!(floor_of(FAMILY_BLOBS, &[&r, &p], 0, 90), 7);
        assert_eq!(floor_of(FAMILY_BLOBS, &[&r], 0, 90), 5);
        assert_eq!(floor_of(FAMILY_BLOBS_TEMP, &[&t], 1050, 90), 3);
        assert_eq!(floor_of(FAMILY_BLOBS_TEMP, &[&t], 1100, 90), 0, "a Temporary segment past its time needs no copies");
        assert_eq!(floor_of(FAMILY_BLOBS, &[], 0, 90), 0);
    }

    #[test]
    fn short_segments_are_taken_by_the_best_ranked_until_the_floor() {
        let all = ids(10);
        let keepers: Vec<String> = all[..5].to_vec();
        // 5 of 7: the two best-ranked candidates not keeping it step in, and nobody else.
        let mut free: Vec<&String> = all[5..].iter().collect();
        free.sort_by_key(|c| rank(c, FAMILY_BLOBS, 3));
        let takers: Vec<&String> = all[5..].iter().filter(|c| decide(c, FAMILY_BLOBS, 3, 7, 5, false, &keepers, &all)).collect();
        assert_eq!(takers.len(), 2);
        assert!(takers.contains(&free[0]) && takers.contains(&free[1]));
        // At the floor: no newcomer.
        let seven: Vec<String> = all[..7].to_vec();
        assert!(all[7..].iter().all(|c| !decide(c, FAMILY_BLOBS, 3, 7, 7, false, &seven, &all)));
        // Every keeper at the floor stays.
        assert!(seven.iter().all(|k| decide(k, FAMILY_BLOBS, 3, 7, 7, true, &seven, &all)));
    }

    #[test]
    fn a_keeper_coming_back_above_the_floor_lets_the_worst_ranked_go() {
        // 8 copies (one came back after another took its place): exactly one lets go, the worst-ranked.
        let eight = ids(8);
        let stay: Vec<&String> = eight.iter().filter(|k| decide(k, FAMILY_BLOBS, 4, 7, 8, true, &eight, &eight)).collect();
        assert_eq!(stay.len(), 7);
        let worst = eight.iter().max_by_key(|k| rank(k, FAMILY_BLOBS, 4)).unwrap();
        assert!(!stay.contains(&worst));
    }

    #[test]
    fn a_keeper_gone_is_replaced_by_the_next_ranked_with_room() {
        // 7 keepers; one misses its audits and its replica goes: 6 counted. One of the others with room steps in.
        let all = ids(12);
        let six: Vec<String> = all[..6].to_vec();
        let with_room: Vec<String> = all[7..].to_vec(); // the one gone (all[6]) doesn't announce any more
        let takers: Vec<&String> = with_room.iter().filter(|c| decide(c, FAMILY_BLOBS, 9, 7, 6, false, &six, &with_room)).collect();
        assert_eq!(takers.len(), 1);
        // No room anywhere: nobody is made to.
        assert!(!decide(&all[7], FAMILY_BLOBS, 9, 7, 6, false, &six, &[]));
    }

    #[test]
    fn prices_scale_with_copies_and_months() {
        assert_eq!(blob::COPY_MONTH_PER_KB, 4_700);
        assert_eq!(blob::class_fee_per_kb(Class::Temporary), 4_700 * 3 * 3);
        assert_eq!(blob::class_fee_per_kb(Class::Permanent), 4_700 * 7 * 12);
        assert_eq!(blob::class_fee_per_kb(Class::Release), 4_700 * 5 * 12);
        let net = params::Net { rules: &[(4, 0), (5, 100)], ..params::TESTNET };
        let paid = [(Class::Temporary, 2_500u64), (Class::Permanent, 1_000)];
        assert_eq!(blob::fee_at(&net, 99, &paid), blob::fee_for(3_500), "the flat rate before version 5");
        assert_eq!(blob::fee_at(&net, 100, &paid), 3 * 42_300 + 394_800);
    }
}
