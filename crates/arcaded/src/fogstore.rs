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
//! **Copies count only when registered and audited.** A segment's copies against its floor are the registry's replicas
//! of its catalog entry that passed an audit and haven't missed one since ([`SegHolders::counted`]; a replica that
//! misses stops counting at once and is removed after `params::MAX_MISSED_AUDITS` misses). That state is the chain's, so
//! every arcade counts the same. Arcades announcing they keep a segment whose copy isn't counted (not registered yet, or
//! not audited) are reported apart, as "announced, not yet counted". While a segment is still open (its range not
//! complete, so nothing can be registered) nothing is counted; who takes it is decided by the announced keepers then, so
//! arcades with room don't all pile on ([`SegHolders::for_decision`]). The registry's own count (`Registry::holders_of`,
//! which splits pay) is consensus and isn't touched here.
//!
//! **Floors**: a blob-family segment needs the highest [`blob::floor_at`] of its classes (Permanent 7, from rules version
//! 6 Permanent 3; Release 5); a Temporary segment ([`segment::FAMILY_BLOBS_TEMP`]) needs 3 until its time is up, then
//! none (the Permanent floor for good once a blob in it is upgraded, rules version 6). Below its floor a segment
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
const KEEP_PROMPT_DISMISSED: &[u8] = b"arc_keep_prompt_dismissed";

/// The keep share an arcade has before its operator sets one, in MB: **0, none** (Martin decides the amount; switching a
/// default on later is this one line). An operator's own setting, 0 included, always wins over it.
pub const DEFAULT_KEEP_MB: u64 = 0;

/// The free disk an arcade earning ARCADE needs before the app suggests a keep share (desktop prompt): 50 GB.
pub const SUGGEST_FREE_BYTES: u64 = 50 << 30;

/// The share the operator set (`arc.keep {mb}`), None while it never set one.
fn keep_set(node: &Node) -> Option<u64> {
    node.app.store.read(|r| r.get(arcade_app::META, KEEP_MB)).ok().flatten().and_then(|v| String::from_utf8(v).ok()?.parse().ok())
}

const KEEP_PCT: &[u8] = b"arc_keep_pct";

/// The free space a share of free space never goes past: 20 GB stay free.
pub const KEEP_FREE_FLOOR: u64 = 20 << 30;
/// The "share of my free space" presets, in percent.
pub const KEEP_PCTS: &[u64] = &[5, 10, 25];

/// The share of free space the operator chose (`arc.keep {pct}`), None for a fixed size.
fn keep_pct(node: &Node) -> Option<u64> {
    node.app.store.read(|r| r.get(arcade_app::META, KEEP_PCT)).ok().flatten().and_then(|v| String::from_utf8(v).ok()?.parse().ok()).filter(|p| *p > 0)
}

/// What `pct` percent of the data volume's free space comes to, in bytes: counted on the free space (past what the
/// installed chains will still grow by, crate::diskbudget) plus what the share holds already (so it doesn't shrink as
/// it fills), and never leaving less than [`KEEP_FREE_FLOOR`] free.
pub fn share_of_free(free: u64, used: u64, pct: u64) -> u64 {
    let target = ((free as u128 + used as u128) * pct.min(100) as u128 / 100) as u64;
    target.min(used + free.saturating_sub(KEEP_FREE_FLOOR))
}

/// The share of the chain this arcade keeps by choice, in MB (0: none): a share of free space (recomputed as free space
/// changes, at most once a minute), else the operator's fixed size, else [`DEFAULT_KEEP_MB`].
pub fn keep_mb(node: &Node) -> u64 {
    if let Some(pct) = keep_pct(node) {
        static LAST: std::sync::Mutex<Option<(std::time::Instant, u64, u64)>> = std::sync::Mutex::new(None);
        if let Some((t, p, mb)) = *LAST.lock().unwrap() {
            if p == pct && t.elapsed().as_secs() < 60 {
                return mb;
            }
        }
        let b = crate::diskbudget::budget(node);
        let mb = share_of_free(b.free.saturating_sub(b.reserve()), kept_bytes(node), pct) >> 20;
        *LAST.lock().unwrap() = Some((std::time::Instant::now(), pct, mb));
        return mb;
    }
    keep_set(node).unwrap_or(DEFAULT_KEEP_MB)
}

/// What a share of `bytes` would hold of the network's segments as the last round saw them: the ones kept here
/// already, then those short of copies (most missing first), then the rest, each whole if it fits.
pub fn would_hold(view: &[SegView], bytes: u64) -> Value {
    let mut order: Vec<&SegView> = view.iter().filter(|s| s.floor > 0 || s.mine).collect();
    order.sort_by_key(|s| (!s.mine, s.copies >= s.floor, std::cmp::Reverse(s.floor.saturating_sub(s.copies)), s.family, s.index));
    let (mut left, mut segs, mut blobs, mut used, mut short_held, mut fills) = (bytes, 0, 0, 0u64, 0, 0);
    for s in order {
        if s.bytes > left {
            continue;
        }
        left -= s.bytes;
        segs += 1;
        blobs += s.blobs;
        used += s.bytes;
        if s.copies < s.floor || (s.mine && s.copies <= s.floor) {
            short_held += 1;
            // With this copy (one not counted yet) it reaches its floor.
            if s.mine && s.copies >= s.floor || !s.mine && s.copies + 1 >= s.floor {
                fills += 1;
            }
        }
    }
    let short: Vec<&SegView> = view.iter().filter(|s| s.copies < s.floor).collect();
    json!({"segments": segs, "of_segments": view.len(), "blobs": blobs, "of_blobs": view.iter().map(|s| s.blobs).sum::<usize>(), "bytes": used,
           "short_held": short_held.min(short.len()), "short": short.len(), "short_filled": fills.min(short.len()),
           "short_families": short.iter().map(|s| family_name(s.family)).collect::<BTreeSet<_>>(),
           "all_short_held": short.iter().all(|s| s.bytes <= bytes) && short.iter().map(|s| s.bytes).sum::<u64>() <= bytes})
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

/// `arc.keep`: the share kept by choice (`mb`, or `gb`; 0 or `off` for none) and what it keeps; `dismiss: true` hides
/// the app's keep-share suggestion for good (it never sets a share by itself). Also what the app needs to offer one:
/// the default, whether the operator chose, free disk, whether Earn ARCADE is on here, whether to suggest a share, and
/// what a share would hold (the network's segments short of copies, as the last fog round saw them).
pub fn api_keep(node: &Node, req: &Value) -> Result<Value, String> {
    let set = if req["mb"] == "off" || req["gb"] == "off" {
        Some(0)
    } else {
        req["mb"].as_u64().or_else(|| req["gb"].as_f64().filter(|g| g.is_finite() && *g >= 0.0).map(|g| (g * 1024.0).round() as u64))
    };
    if let Some(pct) = req["pct"].as_u64() {
        if !KEEP_PCTS.contains(&pct) {
            return Err(format!("pct is one of {KEEP_PCTS:?}"));
        }
        node.app.store.write(|w| w.put(arcade_app::META, KEEP_PCT, pct.to_string().as_bytes())).map_err(|e| e.to_string())?;
        let _ = node.app.event("chains", &format!("Keep ARCADE data: {pct}% of free space"));
    } else if let Some(mb) = set {
        node.app.store.write(|w| {
            w.put(arcade_app::META, KEEP_MB, mb.to_string().as_bytes())?;
            w.del(arcade_app::META, KEEP_PCT).map(|_| ())
        }).map_err(|e| e.to_string())?;
        if mb == 0 {
            save_segs(node, &BTreeSet::new());
        }
        let _ = node.app.event("chains", &if mb == 0 { "Keep ARCADE data: off".to_string() } else { format!("Keep ARCADE data: {mb} MB") });
    }
    if req["dismiss"] == true {
        node.app.store.write(|w| w.put(arcade_app::META, KEEP_PROMPT_DISMISSED, b"1")).map_err(|e| e.to_string())?;
    }
    let mb = keep_mb(node);
    let budget = crate::diskbudget::budget(node);
    let free = budget.free;
    let earn = crate::arcsvc::earning_any(node);
    let dismissed = node.app.store.read(|r| r.get(arcade_app::META, KEEP_PROMPT_DISMISSED)).ok().flatten().is_some();
    let short = short_of_copies();
    let short_bytes: u64 = short.iter().filter_map(|s| s["bytes"].as_u64()).sum();
    let st = STATUS.lock().unwrap().clone().unwrap_or(Value::Null);
    // The presets the Options tab offers, each with what it would hold and whether it fits the free space.
    let used = kept_bytes(node);
    let view = SEG_VIEW.lock().unwrap().clone();
    let pct = keep_pct(node);
    // What the chains will still grow by isn't the keep share's (crate::diskbudget).
    let room = (free + used).saturating_sub(budget.reserve());
    let preset = |id: String, label: String, bytes: u64| json!({"id": id, "label": label, "mb": bytes >> 20, "bytes": bytes,
                                                              "fits": id == "off" || (bytes > 0 && bytes <= room.saturating_sub(KEEP_FREE_FLOOR)),
                                                              "leaves_free": (free + used).saturating_sub(bytes), "holds": would_hold(&view, bytes)});
    let mut presets = vec![preset("off".into(), "Off".into(), 0)];
    for p in KEEP_PCTS {
        presets.push(preset(format!("pct:{p}"), format!("{p}% of my free space"), share_of_free(free.saturating_sub(budget.reserve()), used, *p)));
    }
    let suggested = budget.suggested();
    let choice = match pct {
        Some(p) => format!("pct:{p}"),
        None if mb == 0 => "off".into(),
        None => "custom".into(),
    };
    Ok(json!({"mb": mb, "choice": choice, "pct": pct, "presets": presets, "free_floor_gb": KEEP_FREE_FLOOR >> 30,
              "suggested": {"mb": suggested >> 20, "bytes": suggested, "holds": would_hold(&view, suggested)},
              "budget": crate::diskbudget::json(&budget, used, mb << 20),
              "custom_holds": would_hold(&view, mb << 20), "segments": kept_segs(node).into_iter().map(|(f, i)| json!({"family": family_name(f), "first": i * RANGE, "last": i * RANGE + RANGE - 1})).collect::<Vec<_>>(),
              "used_mb": kept_bytes(node) >> 20, "default_mb": DEFAULT_KEEP_MB, "chosen": keep_set(node).is_some() || pct.is_some(),
              "free_bytes": free, "free_gb": free >> 30, "earn_on": earn,
              "suggest": suggest(mb, earn, free, dismissed) && suggested > 0, "suggest_dismissed": dismissed, "suggest_free_gb": SUGGEST_FREE_BYTES >> 30,
              "would_hold": {"short_segments": short.len(), "short_bytes": short_bytes,
                             "network_bytes": st["network_bytes"], "network_segments": st["network_segments"], "on": st["on"] == true}}))
}

/// Whether the app should suggest a keep share: Earn ARCADE is on here, at least [`SUGGEST_FREE_BYTES`] are free, it
/// keeps none, and the operator didn't dismiss the suggestion (and, in `arc.keep`, the machine's budget suggests some:
/// a fully borrowed arcade on small hardware is never asked).
pub fn suggest(mb: u64, earn: bool, free: u64, dismissed: bool) -> bool {
    mb == 0 && earn && free >= SUGGEST_FREE_BYTES && !dismissed
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

/// What the keep share holds now, in MB (crate::fogmap).
pub fn kept_mb_used(node: &Node) -> u64 {
    kept_bytes(node) >> 20
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
/// `bm`: a bloom of the blobs this arcade published (the fog map's publisher role); `km`, `ku`: its keep share and what
/// the share holds, in MB (the fog map's per-node panel).
pub fn announce(node: &Node) -> Value {
    let ids = crate::arcblob::held_ids();
    let mine: Vec<Hash> = ids.iter().filter(|id| crate::arcblob::is_mine(id)).copied().collect();
    let keep: Vec<Value> = kept_segs(node).into_iter().take(256).map(|(f, i)| json!([f, i])).collect();
    json!({"bf": bloom(&ids), "bn": ids.len(), "bm": bloom(&mine), "mn": mine.len(), "keep": keep, "room": room_mb(node),
           "km": keep_mb(node), "ku": kept_bytes(node) >> 20})
}

/// An announcement as kept from another arcade: only these fields, each in its form and size.
pub fn clean(v: &Value) -> Option<Value> {
    let bf = v["bf"].as_str().filter(|s| s.len() <= 2 * BLOOM_MAX && s.bytes().all(|c| c.is_ascii_hexdigit()))?;
    let keep: Vec<Value> = v["keep"].as_array().map(|a| a.iter().take(256).filter_map(|x| Some(json!([x[0].as_u64().filter(|f| *f < 256)?, x[1].as_u64()?]))).collect()).unwrap_or_default();
    let bm = v["bm"].as_str().filter(|s| s.len() <= 2 * BLOOM_MAX && s.bytes().all(|c| c.is_ascii_hexdigit())).unwrap_or("");
    Some(json!({"bf": bf, "bn": v["bn"].as_u64().unwrap_or(0), "bm": bm, "mn": v["mn"].as_u64().unwrap_or(0), "keep": keep, "room": v["room"].as_u64().unwrap_or(0),
                "km": v["km"].as_u64().unwrap_or(0), "ku": v["ku"].as_u64().unwrap_or(0)}))
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

/// The most copies fog storage asks of a segment here: no limit, but `ARCADE_FOG_FLOOR_MAX` on a closed test net with
/// fewer arcades than a floor (the floors themselves, and what blobs pay for them, are the rules': `blob::floor`).
pub fn floor_max() -> usize {
    env_secs("ARCADE_FOG_FLOOR_MAX", u64::MAX).min(usize::MAX as u64) as usize
}

/// A class's copy floor as fog storage uses it here: the rules' for the next block ([`blob::floor_at`]: Permanent 7 in
/// rules version 5, 3 from version 6), at most [`floor_max`]. Before the chain runs here, version 5's.
pub fn class_floor(class: Class) -> usize {
    let floor = match crate::arcsvc::svc_net() {
        Some(net) => blob::floor_at(&net, crate::arcblob::next_height(), class),
        None => blob::priced_copies(class),
    };
    floor.min(floor_max())
}

/// A segment's copy floor, by `floor` per class: the highest of its classes' floors (Permanent, Release); a Temporary segment the
/// Temporary floor until its time is up (every blob in it past its time: Meta::timed_done, in blocks as the chain counts
/// it, with `next` the next block's height), then 0, unless a blob in it
/// was upgraded to Permanent (rules version 6): the Permanent floor then, for good. An empty one 0.
pub fn floor_of(family: u8, metas: &[&crate::arcblob::Meta], next: u64, now: u64, temporary: u64, floor: impl Fn(Class) -> usize) -> usize {
    if metas.is_empty() {
        return 0;
    }
    if family == FAMILY_BLOBS_TEMP {
        if metas.iter().any(|m| m.upgraded.is_some()) {
            return floor(Class::Permanent).max(floor(Class::Temporary));
        }
        // Within its time while any blob in it is (as the chain counts it, Meta::timed_done).
        return if metas.iter().any(|m| !m.timed_done(next, now, temporary)) { floor(Class::Temporary) } else { 0 };
    }
    // The highest floor of the classes in it (from rules version 6 Release's 5 is above Permanent's 3).
    metas.iter().map(|m| floor(if m.class == Class::Permanent as u8 { Class::Permanent } else { Class::Release })).max().unwrap_or(0)
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

/// The room class of a keep share's free room (MB): its bit length, so arcades with far more room come first and
/// small differences (a share filling up) don't reorder them.
pub fn room_class(room_mb: u64) -> u32 {
    64 - room_mb.leading_zeros()
}

/// What a round decides for one segment, from what it's told (pure, for the tests): whether this arcade should keep
/// it. `copies` the counted copies with this arcade's own among them when `mine_kept`; `candidates` the arcades that could
/// take it (announcing room enough for it, or keeping it already), this one included when it has room or keeps it.
/// `rooms`: each arcade's free keep room in MB, as announced (this one's own). The arcades with the most room step in
/// first (by [`room_class`], then by [`rank`]), so a big machine takes most and a small one only what it can; above the
/// floor the last in that order lets go.
pub fn decide(me: &str, family: u8, index: u64, floor: usize, copies: usize, mine_kept: bool, keepers: &[String], candidates: &[String], rooms: &HashMap<String, u64>) -> bool {
    if floor == 0 {
        return false;
    }
    let order = |c: &String| (std::cmp::Reverse(room_class(rooms.get(c).copied().unwrap_or(0))), rank(c, family, index));
    if mine_kept {
        // Above the floor: the keeper last in the order lets go.
        if copies > floor {
            let worst = keepers.iter().max_by_key(|k| order(k));
            return worst.is_none_or(|w| w != me);
        }
        return true;
    }
    if copies >= floor {
        return false;
    }
    // Short: the first candidates not keeping it yet step in, as many as are missing.
    let mut free: Vec<&String> = candidates.iter().filter(|c| !keepers.contains(c)).collect();
    free.sort_by_key(|c| order(c));
    free.iter().take(floor - copies).any(|c| *c == me)
}

/// One segment as the status shows it.
#[derive(Clone, Debug)]
pub struct Seg {
    pub family: u8,
    pub index: u64,
    pub floor: usize,
    /// Copies counted against the floor: registered replicas that are audited (0 while the segment isn't listed).
    pub copies: usize,
    /// Arcades announcing they keep it whose copy isn't counted (yet): not registered, or not audited.
    pub announced: usize,
    pub counted: Counted,
    pub bytes: u64,
}

/// Who has a segment: the registry's replicas once it is listed (each with whether it is audited), and the arcades
/// announcing they keep it by choice (this one included when it does).
#[derive(Clone, Debug, Default)]
pub struct SegHolders {
    /// Registered replicas' owners (node ids) and whether each is audited; None while the segment isn't listed.
    pub registered: Option<Vec<(String, bool)>>,
    pub keepers: Vec<String>,
}

impl SegHolders {
    /// The copies that count against the floor: registered and audited replicas only (docs/FOGSTORE.md).
    pub fn counted(&self) -> usize {
        self.registered.as_ref().map_or(0, |r| r.iter().filter(|x| x.1).count())
    }

    /// Announced keepers whose copy isn't counted: no audited replica of theirs is registered (yet).
    pub fn announced_only(&self) -> usize {
        self.keepers.iter().filter(|k| !self.registered.as_ref().is_some_and(|r| r.iter().any(|x| x.1 && x.0 == **k))).count()
    }

    /// The count a keep decision goes by: the audited copies once listed; while the segment is open (nothing can be
    /// registered before its range is complete) the announced keepers, so arcades with room don't all pile on.
    pub fn for_decision(&self) -> usize {
        if self.registered.is_some() { self.counted() } else { self.keepers.len() }
    }

    pub fn how(&self) -> Counted {
        if self.registered.is_some() { Counted::Registered } else { Counted::Announced }
    }
}

/// Who has segment (family, index): see [`SegHolders`]. `ann`: the announcements, `mine`: whether this arcade keeps it.
fn holders_of_seg(ann: &[(String, Value)], me: &str, mine: bool, family: u8, index: u64) -> SegHolders {
    let mut keepers: Vec<String> = ann.iter().filter(|(_, f)| f["keep"].as_array().is_some_and(|a| a.iter().any(|x| x[0] == family as u64 && x[1] == index))).map(|(n, _)| n.clone()).collect();
    if mine && !keepers.iter().any(|k| k == me) {
        keepers.push(me.to_string());
    }
    SegHolders { registered: crate::arcsvc::segment_holders(family, index * RANGE), keepers }
}

/// Every other arcade's fresh announcement (node id → its "fog" part), for the storage reads (crate::fogmap).
pub fn announcements_now(node: &Node) -> Vec<(String, Value)> {
    announcements(node)
}

// ---------------- the round ----------------

static STATUS: std::sync::Mutex<Option<Value>> = std::sync::Mutex::new(None);
/// Each segment's copy floor as the last round saw it.
static SEG_FLOORS: std::sync::Mutex<Option<HashMap<(u8, u64), usize>>> = std::sync::Mutex::new(None);
/// Each segment as the last round saw it, for what a keep share would hold (arc.keep's presets).
static SEG_VIEW: std::sync::Mutex<Vec<SegView>> = std::sync::Mutex::new(Vec::new());

/// A segment as a keep-share preset weighs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegView {
    pub family: u8,
    pub index: u64,
    pub bytes: u64,
    pub blobs: usize,
    /// The copies its keep decision goes by ([`SegHolders::for_decision`]), and its floor.
    pub copies: usize,
    pub floor: usize,
    /// Whether this arcade keeps it already.
    pub mine: bool,
}

/// A segment's copy floor as the last fog round saw it (the highest of its classes'; a Temporary one past its time 0): None
/// before the first round, or for a segment it didn't see (crate::fogmap).
pub fn seg_floor(family: u8, index: u64) -> Option<usize> {
    SEG_FLOORS.lock().unwrap().as_ref()?.get(&(family, index)).copied()
}

/// Keepers coming and going per segment, as this arcade saw them (its rounds, since it started): (time, node, joined).
static HISTORY: std::sync::Mutex<Option<HashMap<(u8, u64), (BTreeSet<String>, Vec<(u64, String, bool)>)>>> = std::sync::Mutex::new(None);
/// Most events kept per segment.
const HISTORY_MAX: usize = 32;

fn note_keepers(family: u8, index: u64, keepers: &[String], now: u64) {
    let mut g = HISTORY.lock().unwrap();
    let (prev, events) = g.get_or_insert_with(HashMap::new).entry((family, index)).or_insert_with(|| (BTreeSet::new(), Vec::new()));
    let new: BTreeSet<String> = keepers.iter().cloned().collect();
    for n in new.difference(prev) {
        events.push((now, n.clone(), true));
    }
    for n in prev.difference(&new) {
        events.push((now, n.clone(), false));
    }
    if events.len() > HISTORY_MAX {
        let cut = events.len() - HISTORY_MAX;
        events.drain(..cut);
    }
    *prev = new;
}

/// A segment's keepers coming and going as this arcade's rounds saw them: (time, node id, joined) (crate::fogmap).
pub fn keeper_history(family: u8, index: u64) -> Vec<(u64, String, bool)> {
    HISTORY.lock().unwrap().as_ref().and_then(|h| h.get(&(family, index)).map(|x| x.1.clone())).unwrap_or_default()
}

/// When the last fog round ran (None before the first).
pub fn last_round() -> Option<u64> {
    STATUS.lock().unwrap().as_ref().and_then(|s| s["at"].as_u64())
}
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
    let mut view: Vec<SegView> = Vec::new();
    let mut network_bytes = 0u64;
    for (&(family, index), metas) in &by_seg {
        let floor = floor_of(family, metas, crate::arcblob::next_height(), now, temporary, class_floor);
        let mine_kept = segs.contains(&(family, index));
        let held = holders_of_seg(&ann, &me, mine_kept, family, index);
        // Against the floor count only registered, audited replicas; announced keepers not counted (yet) are told apart.
        let (counted_copies, announced, counted) = (held.counted(), held.announced_only(), held.how());
        let copies = held.for_decision();
        let keepers = held.keepers.clone();
        note_keepers(family, index, &keepers, now);
        seg_info.insert((family, index), (counted_copies, floor));
        view.push(SegView { family, index, bytes: seg_bytes.get(&(family, index)).copied().unwrap_or(0), blobs: metas.len(), copies, floor, mine: mine_kept });
        network_bytes += seg_bytes.get(&(family, index)).copied().unwrap_or(0);
        let size = seg_bytes.get(&(family, index)).copied().unwrap_or(0);
        // Another arcade is a candidate only with room enough for the whole segment (one with less wouldn't take it,
        // and the segment would wait on it).
        let mut candidates: Vec<String> = ann.iter().filter(|(_, f)| f["room"].as_u64().is_some_and(|r| r > 0 && r.saturating_add(1) << 20 > size)).map(|(n, _)| n.clone()).collect();
        if keep_on && (mine_kept || room >= size) {
            candidates.push(me.clone());
        }
        let mut rooms: HashMap<String, u64> = ann.iter().map(|(n, f)| (n.clone(), f["room"].as_u64().unwrap_or(0))).collect();
        rooms.insert(me.clone(), room_mb(node));
        let keep = decide(&me, family, index, floor, copies, mine_kept, &keepers, &candidates, &rooms);
        if keep && !mine_kept {
            segs.insert((family, index));
            room = room.saturating_sub(size);
            eprintln!("arc: fog: keeping the {} segment {}..{} ({copies} of {floor} copies)", family_name(family), index * RANGE, index * RANGE + RANGE - 1);
        } else if !keep && mine_kept {
            segs.remove(&(family, index));
            eprintln!("arc: fog: letting the {} segment {}..{} go ({copies} copies, floor {floor})", family_name(family), index * RANGE, index * RANGE + RANGE - 1);
        }
        if counted_copies < floor {
            short.push(Seg { family, index, floor, copies: counted_copies, announced, counted, bytes: size });
        }
        // Short, even the announced keepers fewer than the floor, and nobody with room left to step in: a warning.
        if copies < floor && keepers.len() < floor && candidates.iter().all(|c| keepers.contains(c)) && !segs.contains(&(family, index)) {
            warnings.push(format!(
                "the {} segment {}..{} has {counted_copies} of its {floor} copies counted ({announced} more announced, not yet counted) and no arcade has room to keep it",
                family_name(family), index * RANGE, index * RANGE + RANGE - 1));
        }
    }
    // Over the share (a segment grew after it was taken: a large blob's parts came block by block, or the operator chose
    // a smaller share): let go of segments until what's kept fits, those with the most copies first, then the biggest.
    let share = keep_mb(node) << 20;
    let mut over: Vec<((u8, u64), u64, usize)> = segs.iter().map(|k| (*k, seg_bytes.get(k).copied().unwrap_or(0), view.iter().find(|v| (v.family, v.index) == *k).map_or(0, |v| v.copies))).collect();
    let mut total: u64 = over.iter().map(|o| o.1).sum();
    over.sort_by_key(|o| (std::cmp::Reverse(o.2), std::cmp::Reverse(o.1)));
    for (k, bytes, copies) in over {
        if total <= share {
            break;
        }
        segs.remove(&k);
        total -= bytes;
        eprintln!("arc: fog: letting the {} segment {}..{} go: it doesn't fit the keep share any more ({bytes} bytes, {copies} copies)", family_name(k.0), k.1 * RANGE, k.1 * RANGE + RANGE - 1);
    }
    save_segs(node, &segs);
    *SEG_FLOORS.lock().unwrap() = Some(seg_info.iter().map(|(k, v)| (*k, v.1)).collect());
    *SEG_VIEW.lock().unwrap() = view;
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
            // Past its time as the chain counts it (an upgraded one never is: Meta::timed_done).
            let temp_done = family_of(m.class) == FAMILY_BLOBS_TEMP && m.timed_done(crate::arcblob::next_height(), now, temporary);
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
                        "announced_not_counted": s.announced, "listed": s.counted == Counted::Registered,
                        "counted": if s.counted == Counted::Registered { "registered, audited replicas" } else { "registered, audited replicas (none: not listed yet)" }, "bytes": s.bytes}))
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
        "network_bytes": network_bytes, "network_segments": by_seg.len(), "floor_max": (floor_max() != usize::MAX).then(floor_max),
    }));
    Some(())
}

/// A blob class's copy floor (for the storage reads, explorer.arc_*): the rules' for the next block.
pub fn blob_floor(class: Class) -> usize {
    match crate::arcsvc::svc_net() {
        Some(net) => blob::floor_at(&net, crate::arcblob::next_height(), class),
        None => blob::priced_copies(class),
    }
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

    fn no() -> HashMap<String, u64> {
        HashMap::new()
    }

    #[test]
    fn the_arcades_with_the_most_room_step_in_first() {
        let all = ids(6);
        let mut rooms = no();
        for (i, c) in all.iter().enumerate() {
            rooms.insert(c.clone(), [3, 200, 500_000, 50, 0, 1][i]); // MB: medium, medium, frontier, ...
        }
        // Short by 2: the frontier one and the next by room class step in, whatever their hash rank.
        let takers: Vec<&String> = all.iter().filter(|c| decide(c, FAMILY_BLOBS, 0, 2, 0, false, &[], &all, &rooms)).collect();
        assert_eq!(takers, vec![&all[1], &all[2]]);
        // Above the floor, the one with the least room lets go.
        let keep3 = vec![all[0].clone(), all[1].clone(), all[2].clone()];
        let stay: Vec<&String> = keep3.iter().filter(|k| decide(k, FAMILY_BLOBS, 0, 2, 3, true, &keep3, &keep3, &rooms)).collect();
        assert_eq!(stay, vec![&all[1], &all[2]]);
    }

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
        use arcade_arc::params::{Net, TESTNET};
        // Rules version 5: Permanent 7; version 6 (Martin 2026-11-19): Permanent 3. Prices don't move.
        let v6 = Net { rules: &[(3, 0), (5, 1), (6, 100)], ..TESTNET };
        let at = |h: u64| move |c: Class| blob::floor_at(&v6, h, c);
        assert_eq!((at(99)(Class::Temporary), at(99)(Class::Release), at(99)(Class::Permanent)), (3, 5, 7));
        assert_eq!((at(100)(Class::Temporary), at(100)(Class::Release), at(100)(Class::Permanent)), (3, 5, 3));
        let m = |class: Class, time: u64| crate::arcblob::Meta { class: class as u8, kind: String::new(), encoding: 0, height: 1, time, line: String::new(), boosted_until: 0, len: 0, publisher: String::new(), upgraded: None, until: None };
        let (p, r, t) = (m(Class::Permanent, 0), m(Class::Release, 0), m(Class::Temporary, 1000));
        assert_eq!(floor_of(FAMILY_BLOBS, &[&r, &p], 0, 0, 90, at(99)), 7);
        assert_eq!(floor_of(FAMILY_BLOBS, &[&r, &p], 0, 0, 90, at(100)), 5, "Release 5 is now the strongest floor");
        assert_eq!(floor_of(FAMILY_BLOBS, &[&p], 0, 0, 90, at(100)), 3);
        assert_eq!(floor_of(FAMILY_BLOBS, &[&r], 0, 0, 90, at(99)), 5);
        assert_eq!(floor_of(FAMILY_BLOBS_TEMP, &[&t], 0, 1050, 90, at(99)), 3);
        assert_eq!(floor_of(FAMILY_BLOBS_TEMP, &[&t], 0, 1100, 90, at(99)), 0, "a Temporary segment past its time needs no copies");
        assert_eq!(floor_of(FAMILY_BLOBS, &[], 0, 0, 90, at(99)), 0);
        // A Temporary blob upgraded to Permanent (rules version 6): its segment keeps the Permanent floor past its time.
        let up = crate::arcblob::Meta { upgraded: Some(blob::Upgraded { height: 100, txid: arcade_chain::hash::Hash256([1; 32]), paid: 352_500, by: Vec::new() }), ..t.clone() };
        assert_eq!(up.class_now(), Class::Permanent as u8);
        assert_eq!(floor_of(FAMILY_BLOBS_TEMP, &[&t, &up], 0, 1_000_000, 90, at(100)), 3);
        assert_eq!(floor_of(FAMILY_BLOBS_TEMP, &[&t, &up], 0, 1_000_000, 90, at(99)), 7);
        // The chain's count in blocks wins over the clock when it knows the blob: kept until the height an upgrade is
        // refused at, whatever the time says, and not after.
        let counted = crate::arcblob::Meta { until: Some(50), ..t.clone() };
        assert_eq!(floor_of(FAMILY_BLOBS_TEMP, &[&counted], 49, 1_000_000, 90, at(99)), 3);
        assert_eq!(floor_of(FAMILY_BLOBS_TEMP, &[&counted], 50, 1_050, 90, at(99)), 0);
        assert!(!counted.timed_done(49, u64::MAX, 90) && counted.timed_done(50, 0, 90));
        assert!(!up.timed_done(u64::MAX, u64::MAX, 90), "an upgraded blob is never past its time");
        assert!(!p.timed_done(u64::MAX, u64::MAX, 90));
    }

    #[test]
    fn short_segments_are_taken_by_the_best_ranked_until_the_floor() {
        let all = ids(10);
        let keepers: Vec<String> = all[..5].to_vec();
        // 5 of 7: the two best-ranked candidates not keeping it step in, and nobody else.
        let mut free: Vec<&String> = all[5..].iter().collect();
        free.sort_by_key(|c| rank(c, FAMILY_BLOBS, 3));
        let takers: Vec<&String> = all[5..].iter().filter(|c| decide(c, FAMILY_BLOBS, 3, 7, 5, false, &keepers, &all, &no())).collect();
        assert_eq!(takers.len(), 2);
        assert!(takers.contains(&free[0]) && takers.contains(&free[1]));
        // At the floor: no newcomer.
        let seven: Vec<String> = all[..7].to_vec();
        assert!(all[7..].iter().all(|c| !decide(c, FAMILY_BLOBS, 3, 7, 7, false, &seven, &all, &no())));
        // Every keeper at the floor stays.
        assert!(seven.iter().all(|k| decide(k, FAMILY_BLOBS, 3, 7, 7, true, &seven, &all, &no())));
    }

    #[test]
    fn a_keeper_coming_back_above_the_floor_lets_the_worst_ranked_go() {
        // 8 copies (one came back after another took its place): exactly one lets go, the worst-ranked.
        let eight = ids(8);
        let stay: Vec<&String> = eight.iter().filter(|k| decide(k, FAMILY_BLOBS, 4, 7, 8, true, &eight, &eight, &no())).collect();
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
        let takers: Vec<&String> = with_room.iter().filter(|c| decide(c, FAMILY_BLOBS, 9, 7, 6, false, &six, &with_room, &no())).collect();
        assert_eq!(takers.len(), 1);
        // No room anywhere: nobody is made to.
        assert!(!decide(&all[7], FAMILY_BLOBS, 9, 7, 6, false, &six, &[], &no()));
    }

    #[test]
    fn a_keep_share_is_suggested_only_to_earners_with_room_that_keep_none() {
        assert_eq!(DEFAULT_KEEP_MB, 0, "no default share until Martin picks one");
        let gb = 1u64 << 30;
        assert!(suggest(0, true, 50 * gb, false));
        assert!(!suggest(0, true, 49 * gb, false), "under 50 GB free");
        assert!(!suggest(0, false, 500 * gb, false), "Earn ARCADE off");
        assert!(!suggest(1024, true, 500 * gb, false), "keeps a share already");
        assert!(!suggest(0, true, 500 * gb, true), "dismissed");
    }

    #[test]
    fn a_share_of_free_space_leaves_20_gb_and_doesnt_shrink_as_it_fills() {
        let gb = 1u64 << 30;
        assert_eq!(share_of_free(400 * gb, 0, 10), 40 * gb);
        assert_eq!(share_of_free(360 * gb, 40 * gb, 10), 40 * gb, "the same once it holds 40 GB");
        assert_eq!(share_of_free(30 * gb, 0, 25), 7 * gb + gb / 2);
        assert_eq!(share_of_free(22 * gb, 0, 25), 2 * gb, "never past 20 GB left free");
        assert_eq!(share_of_free(10 * gb, 3 * gb, 25), 3 * gb, "below the floor: keeps what it has, takes nothing more");
    }

    #[test]
    fn what_a_share_would_hold_takes_short_segments_first() {
        let v = |index, bytes, copies, floor, mine| SegView { family: FAMILY_BLOBS, index, bytes, blobs: 10, copies, floor, mine };
        let view = [v(0, 100, 7, 7, false), v(1, 50, 6, 7, false), v(2, 80, 3, 7, false)];
        let h = would_hold(&view, 140);
        // Short ones first, most missing first: index 2 (80), then 1 (50); index 0 doesn't fit.
        assert_eq!((h["segments"].as_u64(), h["short_held"].as_u64(), h["short_filled"].as_u64(), h["bytes"].as_u64()), (Some(2), Some(2), Some(1), Some(130)));
        assert_eq!(h["short"], 2);
        assert_eq!(would_hold(&view, 0)["segments"], 0);
        assert_eq!(would_hold(&view, 1000)["segments"], 3);
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
