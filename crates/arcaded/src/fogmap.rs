//! The Storage app's FOG MAP reads (docs/FOGSTORE.md), every ARCADE blob as fog storage sees it from here:
//!
//! - `explorer.arc_blobs`: per item (a blob, or a large blob as one item with its parts): id, publisher (@tag and
//!   address), class, type, bytes, segment ("P#0": the blobs family, "T#0": temporary blobs), holders by role, copies
//!   counted against the floor (registered, audited replicas only) beside the announced keepers not counted yet, all
//!   copies, and when it was last viewed here. With `id`, that one item and its journey (published, grace until, the
//!   chosen keepers' seal and audit heights, keepers that came and went, viewer copies). Paged, filtered, with totals
//!   for the whole filtered set, per publisher and per holder.
//! - `explorer.arc_fog_nodes`: per node: its keep share (setting, used, free), how many blobs it holds by role (chosen
//!   keeper, published there, grace, viewed) and how many are in the fog for it, the segments it keeps ("n/floor"
//!   each), its earning weight, and the nodes it shares the most blobs with.
//!
//! Holder roles: **keeper** (a chosen keeper: its replica of the segment is registered, audited or not, or it announces
//! keeping the segment), **publisher** (its announcement's bloom of own blobs, `bm`), **grace** (it holds the bytes
//! while the blob is new), **viewer** (it holds the bytes past that: viewed or served there). Bytes held come from each
//! node's bloom (`bf`, so a false positive is possible); this arcade's own from its store.
//!
//! What every arcade agrees on: the blobs, their records and the registry's replicas (the chain's state). What is this
//! arcade's own view: the announcements it heard, its copies and view times, the keepers it saw come and go. Nothing
//! here is a rule.

use crate::fogstore;
use crate::node::Node;
use arcade_arc::merkle::Hash;
use arcade_arc::params::RANGE;
use arcade_arc::segment::{FAMILY_BLOBS, FAMILY_BLOBS_TEMP};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};

type R = Result<Value, String>;

const DEFAULT_LIMIT: usize = 200;
const MAX_LIMIT: usize = 500;
/// Most publishers and holders in the totals.
const MAX_TOTALS: usize = 500;
/// Most other nodes in a node's overlap list.
const OVERLAP_TOP: usize = 10;

pub fn class_name(c: u8) -> &'static str {
    match c {
        0 => "permanent",
        1 => "release",
        2 => "temporary",
        3 => "boost",
        _ => "other",
    }
}

fn class_of(v: &Value) -> Option<u8> {
    v.as_u64().map(|c| c as u8).or_else(|| v.as_str().and_then(|s| (0..4u8).find(|c| class_name(*c) == s.to_ascii_lowercase())))
}

fn class_enum(c: u8) -> arcade_arc::blob::Class {
    arcade_arc::blob::Class::from_u8(c).unwrap_or(arcade_arc::blob::Class::Permanent)
}

/// A segment's short name: "P#3" (the blobs family) or "T#3" (temporary blobs), the number its 10,000-block range.
pub fn seg_name(family: u8, index: u64) -> String {
    format!("{}#{index}", if family == FAMILY_BLOBS_TEMP { "T" } else { "P" })
}

/// A registered replica of a segment, as the registry has it.
#[derive(Clone, Debug)]
struct Rep {
    owner: String,
    audited: bool,
    /// The block that held its proof (sealed and registered there).
    height: u64,
    missed: u32,
    /// The height it counts from (its first passed audit), None before one.
    audited_from: Option<u64>,
}

/// One holder of an item, merged from the registry, the announcements and this arcade.
#[derive(Default, Clone)]
struct Holder {
    registered: bool,
    audited: bool,
    keeps: bool,
    bytes: bool,
    publisher: bool,
    here: bool,
    /// Parts of a large blob it holds (by any of the above).
    parts: usize,
}

impl Holder {
    /// "registered+audited", "registered" (not audited: no passed audit yet, or missed its last), "here" (this arcade,
    /// its copy not registered) or "announced".
    fn state(&self) -> &'static str {
        if self.registered && self.audited {
            "registered+audited"
        } else if self.registered {
            "registered"
        } else if self.here {
            "here"
        } else {
            "announced"
        }
    }

    /// Its role for this blob: keeper, publisher, grace or viewer.
    fn role(&self, new: bool) -> &'static str {
        if self.registered || self.keeps {
            "keeper"
        } else if self.publisher {
            "publisher"
        } else if new {
            "grace"
        } else {
            "viewer"
        }
    }
}

/// One item before paging.
struct Item {
    id: Hash,
    meta: crate::arcblob::Meta,
    large: Option<crate::largeblob::Manifest>,
    family: u8,
    fog: bool,
    bytes: u64,
    segments: Vec<u64>,
    copies: usize,
    announced: usize,
    floor: Option<usize>,
    holders: BTreeMap<String, Holder>,
    last_viewed: Option<u64>,
    here: bool,
    mine: bool,
    /// Still in its grace window (every arcade keeps it).
    new: bool,
}

impl Item {
    /// Its class now: Permanent once upgraded (rules version 6), else its record's (a large blob's, its manifest's).
    fn class(&self) -> u8 {
        if self.meta.upgraded.is_some() {
            arcade_arc::blob::Class::Permanent as u8
        } else {
            self.large.as_ref().map_or(self.meta.class, |l| l.class)
        }
    }

    fn short(&self) -> bool {
        self.floor.is_some_and(|f| self.copies < f)
    }
}

/// Everything both reads build on.
struct View {
    me: String,
    tip: u64,
    net: arcade_arc::params::Net,
    items: Vec<Item>,
    metas: HashMap<Hash, crate::arcblob::Meta>,
    tags: HashMap<String, Option<String>>,
    /// Registered replicas per listed segment (family, index).
    reps: HashMap<(u8, u64), Vec<Rep>>,
    /// Each owner's earning (pay) weight, by node id.
    weights: HashMap<String, u64>,
    ann: Vec<(String, Value)>,
    kept: std::collections::BTreeSet<(u8, u64)>,
    grace: u64,
}

fn view(node: &Node) -> Result<View, String> {
    crate::largeblob::refresh();
    let known = crate::arcblob::known();
    // The chain's part, under its lock: the registry's replicas of every listed blob-family segment, and pay weights.
    let (net, tip, reps, weights) = crate::arcsvc::with_chain(|net, chain| {
        let reg = chain.storage();
        let mut reps: HashMap<(u8, u64), Vec<Rep>> = HashMap::new();
        let mut listed: HashMap<Hash, (u8, u64)> = HashMap::new();
        for family in [FAMILY_BLOBS, FAMILY_BLOBS_TEMP] {
            for (first, _, l) in reg.catalog().spans(net.key, family) {
                if let Some(l) = l {
                    listed.insert(l.id, (family, first / RANGE));
                    reps.entry((family, first / RANGE)).or_default();
                }
            }
        }
        for (id, seg, owner, _) in reg.all_replicas() {
            if let Some(k) = listed.get(&seg) {
                let missed = reg.replica_audit(&id).map_or(0, |a| a.1);
                let from = reg.draw_from(&id);
                reps.entry(*k).or_default().push(Rep {
                    owner: hex::encode(owner),
                    audited: missed == 0 && from.is_some(),
                    height: reg.replica(&id).map_or(0, |r| r.2),
                    missed,
                    audited_from: from,
                });
            }
        }
        let weights: HashMap<String, u64> = reg.owners().iter().map(|o| (hex::encode(o), reg.owner_pay_weight(o))).collect();
        (*net, chain.tip().height, reps, weights)
    })
    .ok_or("the ARCADE chain isn't running here (asked of arcades that run it)")?;
    let mut known = known;
    fill_publishers(&mut known);

    let me = node.id_hex();
    let ann = fogstore::announcements_now(node);
    let kept = fogstore::kept_segs(node);
    let held: HashSet<Hash> = crate::arcblob::held_ids().into_iter().collect();
    let now = arcade_app::now();
    let grace = fogstore::grace_secs();
    let fam_of = |m: &crate::arcblob::Meta| if fogstore::on(&net, m.height) { fogstore::family_of(m.class) } else { FAMILY_BLOBS };
    let seg_keepers = |family: u8, index: u64| -> Vec<String> {
        let mut k: Vec<String> = ann
            .iter()
            .filter(|(_, f)| f["keep"].as_array().is_some_and(|a| a.iter().any(|x| x[0] == family as u64 && x[1] == index)))
            .map(|(n, _)| n.clone())
            .collect();
        if kept.contains(&(family, index)) {
            k.push(me.clone());
        }
        k
    };
    let seg_counts = |family: u8, index: u64| -> fogstore::SegHolders {
        fogstore::SegHolders { registered: reps.get(&(family, index)).map(|r| r.iter().map(|x| (x.owner.clone(), x.audited)).collect()), keepers: seg_keepers(family, index) }
    };
    // Who holds one blob (a part, or a whole small blob), merged into `into`.
    let holders_of = |id: &Hash, family: u8, index: u64, into: &mut BTreeMap<String, Holder>| {
        let mut touched: HashSet<String> = HashSet::new();
        for r in reps.get(&(family, index)).into_iter().flatten() {
            let h = into.entry(r.owner.clone()).or_default();
            h.registered = true;
            h.audited |= r.audited;
            touched.insert(r.owner.clone());
        }
        for n in seg_keepers(family, index) {
            into.entry(n.clone()).or_default().keeps = true;
            touched.insert(n);
        }
        for (n, f) in &ann {
            let has = |k: &str| f[k].as_str().is_some_and(|b| fogstore::bloom_has(b, id));
            if has("bf") {
                into.entry(n.clone()).or_default().bytes = true;
                touched.insert(n.clone());
            }
            if has("bm") {
                into.entry(n.clone()).or_default().publisher = true;
                touched.insert(n.clone());
            }
        }
        if held.contains(id) {
            let h = into.entry(me.clone()).or_default();
            h.here = true;
            h.bytes = true;
            h.publisher |= crate::arcblob::is_mine(id);
            touched.insert(me.clone());
        }
        for n in touched {
            into.entry(n).or_default().parts += 1;
        }
    };

    let metas: HashMap<Hash, crate::arcblob::Meta> = known.iter().cloned().collect();
    let mut in_large: HashSet<Hash> = HashSet::new();
    let mut items: Vec<Item> = Vec::new();
    for (mid, man) in crate::largeblob::all().into_iter().filter(|g| g.1.large()) {
        let Some(mm) = metas.get(&mid).cloned() else { continue };
        in_large.insert(mid);
        let mut holders = BTreeMap::new();
        let (mut segments, mut copies, mut announced, mut floor) = (Vec::new(), usize::MAX, 0, None::<usize>);
        for (pid, _) in &man.parts {
            in_large.insert(*pid);
            let Some(pm) = metas.get(pid) else { continue };
            let (family, index) = (fam_of(pm), pm.height / RANGE);
            if !segments.contains(&index) {
                segments.push(index);
                let s = seg_counts(family, index);
                copies = copies.min(s.counted());
                announced = announced.max(s.announced_only());
                if fogstore::on(&net, pm.height) {
                    let f = fogstore::seg_floor(family, index).unwrap_or_else(|| fogstore::class_floor(class_enum(pm.class_now())));
                    floor = Some(floor.map_or(f, |x: usize| x.max(f)));
                }
            }
            holders_of(pid, family, index, &mut holders);
        }
        let last_viewed = std::iter::once(mid).chain(man.parts.iter().map(|p| p.0)).filter_map(|x| crate::arcblob::last_view(&x)).max();
        items.push(Item {
            id: mid,
            family: fam_of(&mm),
            fog: fogstore::on(&net, mm.height),
            bytes: man.size,
            segments,
            copies: if copies == usize::MAX { 0 } else { copies },
            announced,
            floor,
            holders,
            last_viewed,
            here: man.parts.iter().all(|p| held.contains(&p.0)),
            mine: crate::arcblob::is_mine(&mid),
            new: mm.time.saturating_add(grace) > now,
            meta: mm,
            large: Some(man),
        });
    }
    for (id, m) in &known {
        if in_large.contains(id) {
            continue;
        }
        let (family, index) = (fam_of(m), m.height / RANGE);
        let s = seg_counts(family, index);
        let fog = fogstore::on(&net, m.height);
        let mut holders = BTreeMap::new();
        holders_of(id, family, index, &mut holders);
        items.push(Item {
            id: *id,
            family,
            fog,
            bytes: crate::arcblob::len_of(id).unwrap_or(0),
            segments: vec![index],
            copies: s.counted(),
            announced: s.announced_only(),
            floor: fog.then(|| fogstore::seg_floor(family, index).unwrap_or_else(|| fogstore::class_floor(class_enum(m.class_now())))),
            holders,
            last_viewed: crate::arcblob::last_view(id),
            here: held.contains(id),
            mine: crate::arcblob::is_mine(id),
            new: m.time.saturating_add(grace) > now,
            meta: m.clone(),
            large: None,
        });
    }
    // Publishers' @tags (this arcade's ledger of ARCADE).
    let arc = crate::arcsvc::defined();
    let mut tags: HashMap<String, Option<String>> = HashMap::new();
    for it in &items {
        let a = &it.meta.publisher;
        if !tags.contains_key(a) {
            tags.insert(a.clone(), arc.and_then(|p| arcade_ledger::index::tag_of(&node.chain, p.chain_no, a).ok().flatten()));
        }
    }
    Ok(View { me, tip, net, items, metas, tags, reps, weights, ann, kept, grace })
}

/// Fill in publishers for metas written before they carried one, from the blobs' blocks (and write them back).
fn fill_publishers(known: &mut [(Hash, crate::arcblob::Meta)]) {
    let missing: Vec<usize> = (0..known.len()).filter(|i| known[*i].1.publisher.is_empty()).collect();
    if missing.is_empty() {
        return;
    }
    let found = crate::arcsvc::with_chain(|net, chain| {
        let mut by_height: HashMap<u64, Vec<usize>> = HashMap::new();
        for i in &missing {
            by_height.entry(known[*i].1.height).or_default().push(*i);
        }
        let mut out = Vec::new();
        for (h, idx) in by_height {
            let Some(block) = chain.id_at(h).and_then(|id| chain.block(&id)) else { continue };
            for tx in block.txs.iter().skip(1) {
                let publisher = tx.outputs.iter().find(|o| !arcade_arc::script::unspendable(&o.script_pubkey)).and_then(|o| crate::arcblob::address_of_script(net, &o.script_pubkey));
                for r in arcade_arc::blob::records(tx) {
                    let rid = r.id(net);
                    for i in &idx {
                        if known[*i].0 == rid {
                            out.push((*i, publisher.clone().unwrap_or_else(|| "unknown".into())));
                        }
                    }
                }
            }
        }
        out
    })
    .unwrap_or_default();
    for (i, p) in found {
        if known[i].1.publisher.is_empty() {
            known[i].1.publisher = p;
            crate::arcblob::put_meta(&known[i].0, &known[i].1);
        }
    }
}

fn who(n: &str, me: &str) -> Value {
    json!({"node": n, "tag": crate::meshview::tag_of(n), "name": crate::meshview::name_of_peer(n), "here": n == me})
}

/// `explorer.arc_blobs {id?, offset?, limit? (<= 500), publisher?, class?, short_only?, here_only?, sort?}`.
pub fn blobs(node: &Node, req: &Value) -> R {
    let offset = req["offset"].as_u64().unwrap_or(0) as usize;
    let limit = (req["limit"].as_u64().unwrap_or(DEFAULT_LIMIT as u64) as usize).clamp(1, MAX_LIMIT);
    let want_class = match &req["class"] {
        Value::Null => None,
        v => Some(class_of(v).ok_or("class is permanent, release, temporary or boost (or 0 to 3)")?),
    };
    let want_pub = req["publisher"].as_str().map(|s| s.trim().trim_start_matches('@').to_ascii_lowercase()).filter(|s| !s.is_empty());
    let want_id = req["id"].as_str().map(|s| s.trim().to_ascii_lowercase());
    let short_only = req["short_only"] == true;
    let here_only = req["here_only"] == true;
    let sort = req["sort"].as_str().unwrap_or("publisher");
    if !["publisher", "newest", "copies"].contains(&sort) {
        return Err("sort is publisher, newest or copies".into());
    }
    let mut v = view(node)?;
    let total_known = v.items.len();
    let tags = v.tags.clone();
    v.items.retain(|it| {
        want_id.as_ref().is_none_or(|i| hex::encode(it.id) == *i || it.large.as_ref().is_some_and(|l| l.parts.iter().any(|p| hex::encode(p.0) == *i)))
            && want_class.is_none_or(|c| it.class() == c)
            && want_pub.as_ref().is_none_or(|p| it.meta.publisher.to_ascii_lowercase() == *p || tags.get(&it.meta.publisher).and_then(|t| t.as_ref()).is_some_and(|t| t.to_ascii_lowercase() == *p))
            && (!short_only || it.short())
            && (!here_only || it.here)
    });
    if want_id.is_some() && v.items.is_empty() {
        return Err("no such blob known here".into());
    }
    match sort {
        "newest" => v.items.sort_by(|a, b| b.meta.height.cmp(&a.meta.height).then(a.id.cmp(&b.id))),
        "copies" => v.items.sort_by(|a, b| a.copies.cmp(&b.copies).then(b.meta.height.cmp(&a.meta.height)).then(a.id.cmp(&b.id))),
        _ => v.items.sort_by(|a, b| a.meta.publisher.cmp(&b.meta.publisher).then(a.meta.height.cmp(&b.meta.height)).then(a.id.cmp(&b.id))),
    }

    let band = |it: &Item| -> &'static str {
        if it.holders.is_empty() {
            "no_holder_seen"
        } else if it.floor.is_some_and(|f| it.copies >= f) {
            "at_floor"
        } else if it.copies == 0 {
            "none_counted"
        } else if it.copies <= 2 {
            "thin"
        } else {
            "below_floor"
        }
    };
    let mut totals = json!({"items": v.items.len(), "known": total_known, "blobs": 0, "bytes": 0, "large": 0, "short": 0, "here": 0, "mine": 0,
                            "bands": {"at_floor": 0, "below_floor": 0, "thin": 0, "none_counted": 0, "no_holder_seen": 0}, "classes": {}});
    let mut pubs: BTreeMap<String, Value> = BTreeMap::new();
    let mut nodes: BTreeMap<String, Value> = BTreeMap::new();
    let add = |v: &mut Value, k: &str, n: u64| v[k] = json!(v[k].as_u64().unwrap_or(0) + n);
    for it in &v.items {
        add(&mut totals, "blobs", it.large.as_ref().map_or(1, |m| m.parts.len() as u64 + 1));
        add(&mut totals, "bytes", it.bytes);
        add(&mut totals, "large", it.large.is_some() as u64);
        add(&mut totals, "short", it.short() as u64);
        add(&mut totals, "here", it.here as u64);
        add(&mut totals, "mine", it.mine as u64);
        add(&mut totals["bands"], band(it), 1);
        add(&mut totals["classes"], class_name(it.class()), 1);
        let p = pubs.entry(it.meta.publisher.clone()).or_insert_with(|| {
            json!({"address": it.meta.publisher, "tag": tags.get(&it.meta.publisher).cloned().flatten(), "items": 0, "bytes": 0, "short": 0,
                   "bands": {"at_floor": 0, "below_floor": 0, "thin": 0, "none_counted": 0, "no_holder_seen": 0}, "classes": {}, "types": {}})
        });
        add(p, "items", 1);
        add(p, "bytes", it.bytes);
        add(p, "short", it.short() as u64);
        add(&mut p["bands"], band(it), 1);
        add(&mut p["classes"], class_name(it.class()), 1);
        add(&mut p["types"], &it.large.as_ref().map_or(it.meta.kind.clone(), |m| m.kind.clone()), 1);
        for (n, h) in &it.holders {
            let e = nodes.entry(n.clone()).or_insert_with(|| {
                let mut w = who(n, &v.me);
                for k in ["items", "bytes", "keeper", "publisher", "grace", "viewer", "registered_audited", "registered"] {
                    w[k] = json!(0);
                }
                w
            });
            add(e, "items", 1);
            add(e, "bytes", it.bytes);
            add(e, h.role(it.new), 1);
            add(e, if h.audited { "registered_audited" } else { "registered" }, h.registered as u64);
        }
    }
    let mut publishers: Vec<Value> = pubs.into_values().collect();
    publishers.sort_by(|a, b| b["bytes"].as_u64().cmp(&a["bytes"].as_u64()).then(a["address"].as_str().cmp(&b["address"].as_str())));
    let mut holders: Vec<Value> = nodes.into_values().collect();
    holders.sort_by(|a, b| b["items"].as_u64().cmp(&a["items"].as_u64()).then(a["node"].as_str().cmp(&b["node"].as_str())));
    let page: Vec<Value> = v.items.iter().skip(offset).take(limit).map(|it| item_json(it, &v, want_id.is_some())).collect();
    Ok(json!({
        "chain": crate::arcsvc::defined().map(|p| p.key),
        "height": v.tip,
        "fog_round_at": fogstore::last_round(),
        "node": v.me,
        "grace_secs": v.grace,
        "idle_secs": fogstore::idle_secs(),
        "floors": {"permanent": fogstore::class_floor(class_enum(0)), "release": fogstore::class_floor(class_enum(1)), "temporary": fogstore::class_floor(class_enum(2))},
        "counted": "registered, audited replicas (announced keepers not counted yet are given apart)",
        "totals": totals,
        "publishers": publishers.into_iter().take(MAX_TOTALS).collect::<Vec<_>>(),
        "holders": holders.into_iter().take(MAX_TOTALS).collect::<Vec<_>>(),
        "items": page,
        "total": v.items.len(),
        "offset": offset,
        "limit": limit,
    }))
}

fn seg_json(family: u8, index: u64) -> Value {
    json!({"name": seg_name(family, index), "family": fogstore::family_label(family), "index": index, "first": index * RANGE, "last": index * RANGE + RANGE - 1})
}

fn holders_json(it: &Item, me: &str) -> Vec<Value> {
    let rank = |r: &str| ["keeper", "publisher", "grace", "viewer"].iter().position(|x| *x == r).unwrap_or(4);
    let mut out: Vec<Value> = it
        .holders
        .iter()
        .map(|(n, x)| {
            let mut v = who(n, me);
            v["role"] = json!(x.role(it.new));
            v["state"] = json!(x.state());
            v["registered"] = json!(x.registered);
            v["audited"] = json!(x.audited);
            v["keeps"] = json!(x.keeps);
            v["holds_bytes"] = json!(x.bytes);
            v["publisher"] = json!(x.publisher);
            if it.large.is_some() {
                v["parts_held"] = json!(x.parts);
            }
            v
        })
        .collect();
    out.sort_by(|a, b| rank(a["role"].as_str().unwrap_or("")).cmp(&rank(b["role"].as_str().unwrap_or(""))).then(a["node"].as_str().cmp(&b["node"].as_str())));
    out
}

/// An item's upgrade to Permanent (rules version 6): `{by, at_height, paid, txid}`, null without one. A large blob's is its
/// manifest's, `paid` summed over the manifest and every part upgraded with it.
fn upgraded_json(it: &Item, v: &View) -> Value {
    let Some(u) = &it.meta.upgraded else { return Value::Null };
    let mut paid = u.paid;
    if let Some(l) = &it.large {
        paid += l.parts.iter().filter_map(|(pid, _)| v.metas.get(pid)?.upgraded.as_ref().map(|x| x.paid)).sum::<u64>();
    }
    json!({"by": crate::arcblob::address_of_script(&v.net, &u.by).unwrap_or_else(|| hex::encode(&u.by)), "at_height": u.height, "paid": paid, "txid": u.txid.to_hex()})
}

fn item_json(it: &Item, v: &View, journey: bool) -> Value {
    let m = &it.meta;
    let roles = |r: &str| it.holders.values().filter(|h| h.role(it.new) == r).count();
    let mut out = json!({
        "id": hex::encode(it.id),
        "kind": if it.large.is_some() { "large" } else { "blob" },
        "type": it.large.as_ref().map_or(m.kind.clone(), |l| l.kind.clone()),
        "publisher": {"address": m.publisher, "tag": v.tags.get(&m.publisher).cloned().flatten()},
        "class": class_name(it.class()),
        "upgraded": upgraded_json(it, v),
        "bytes": it.bytes,
        "height": m.height,
        "time": m.time,
        "fog": it.fog,
        "new": it.new,
        "segment": it.segments.first().map(|i| seg_json(it.family, *i)),
        "copies": it.copies,
        "floor": it.floor,
        "short": it.short(),
        "announced_not_counted": it.announced,
        "all_copies": it.holders.values().filter(|h| h.registered || h.keeps || h.bytes).count(),
        "by_role": {"keeper": roles("keeper"), "publisher": roles("publisher"), "grace": roles("grace"), "viewer": roles("viewer")},
        "holders": holders_json(it, &v.me),
        "holders_seen": it.holders.len(),
        "last_viewed": it.last_viewed,
        "here": it.here,
        "mine": it.mine,
        "boosted_until": (m.boosted_until > 0).then_some(m.boosted_until),
    });
    if let Some(l) = &it.large {
        out["segments"] = json!(it.segments.iter().map(|i| seg_json(it.family, *i)).collect::<Vec<_>>());
        out["parts"] = json!(l
            .parts
            .iter()
            .map(|(pid, len)| {
                let pm = v.metas.get(pid);
                let fam = pm.map(|pm| if fogstore::on(&v.net, pm.height) { fogstore::family_of(pm.class) } else { FAMILY_BLOBS });
                json!({"id": hex::encode(pid), "bytes": len, "height": pm.map(|p| p.height), "segment": pm.zip(fam).map(|(p, f)| seg_json(f, p.height / RANGE)),
                       "here": crate::arcblob::bytes_here(pid)})
            })
            .collect::<Vec<_>>());
        out["name"] = json!(l.name);
    }
    if journey {
        out["journey"] = journey_json(it, v);
    }
    out
}

/// One blob's journey (the fog map's timeline): published, grace until, the chosen keepers (registry: sealed at, audited
/// from, missed in a row; announced: since this arcade saw them), keepers that came and went, the copies outside the
/// floor (publisher, grace, viewers) and this arcade's own last view.
fn journey_json(it: &Item, v: &View) -> Value {
    let mut keepers = Vec::new();
    let mut events = Vec::new();
    for idx in &it.segments {
        let name = seg_name(it.family, *idx);
        for r in v.reps.get(&(it.family, *idx)).into_iter().flatten() {
            let mut k = who(&r.owner, &v.me);
            k["segment"] = json!(name);
            k["registered"] = json!(true);
            k["sealed_at"] = json!(r.height);
            k["audited_from"] = json!(r.audited_from);
            k["missed_in_a_row"] = json!(r.missed);
            k["counted"] = json!(r.audited);
            keepers.push(k);
        }
        let hist = fogstore::keeper_history(it.family, *idx);
        for (n, h) in &it.holders {
            if h.keeps && !h.registered {
                let mut k = who(n, &v.me);
                k["segment"] = json!(name);
                k["registered"] = json!(false);
                k["counted"] = json!(false);
                k["since"] = json!(hist.iter().filter(|e| e.1 == *n && e.2).map(|e| e.0).max());
                keepers.push(k);
            }
        }
        for (t, n, joined) in hist {
            let mut e = who(&n, &v.me);
            e["segment"] = json!(name);
            e["at"] = json!(t);
            e["event"] = json!(if joined { "keeper joined" } else { "keeper left (stopped announcing it, or its replica went)" });
            events.push(e);
        }
    }
    let extra: Vec<Value> = it
        .holders
        .iter()
        .filter(|(_, h)| !h.registered && !h.keeps)
        .map(|(n, h)| {
            let mut x = who(n, &v.me);
            x["role"] = json!(h.role(it.new));
            if *n == v.me {
                x["last_viewed"] = json!(it.last_viewed);
                x["fades_at"] = json!(it.last_viewed.map(|t| t + fogstore::idle_secs()));
            }
            x
        })
        .collect();
    let replaced: Vec<&Value> = events.iter().filter(|e| e["event"].as_str().is_some_and(|s| s.starts_with("keeper left"))).collect();
    json!({
        "published": {"height": it.meta.height, "time": it.meta.time, "publisher": it.meta.publisher},
        "grace_until": it.meta.time + v.grace,
        "keepers": keepers,
        "copies_outside_the_floor": extra,
        "events": events,
        "replaced_keepers": replaced.len(),
        "now": {"copies": it.copies, "floor": it.floor, "all_copies": it.holders.values().filter(|h| h.registered || h.keeps || h.bytes).count(), "height": v.tip, "time": arcade_app::now()},
        "note": "events are what this arcade's fog rounds saw since it started; other arcades' viewer copies are known by their announcements only (no view times)",
    })
}

/// `explorer.arc_fog_nodes {node?}`: every node fog storage knows of from here (its announcements, the registry's
/// owners, this arcade), with its keep share, blobs held by role, the segments it keeps, its earning weight and the
/// nodes it shares the most blobs with.
pub fn nodes(node: &Node, req: &Value) -> R {
    let v = view(node)?;
    let want = req["node"].as_str().map(|s| s.to_ascii_lowercase());
    let mut ids: std::collections::BTreeSet<String> = v.ann.iter().map(|a| a.0.clone()).collect();
    ids.insert(v.me.clone());
    for it in &v.items {
        ids.extend(it.holders.keys().cloned());
    }
    ids.extend(v.weights.keys().cloned());
    let ann: HashMap<&str, &Value> = v.ann.iter().map(|(n, f)| (n.as_str(), f)).collect();
    // Blobs each node holds (any role), for the overlap.
    let mut held_by: HashMap<&str, HashSet<usize>> = HashMap::new();
    for (i, it) in v.items.iter().enumerate() {
        for (n, h) in &it.holders {
            if h.registered || h.keeps || h.bytes || h.publisher {
                held_by.entry(n.as_str()).or_default().insert(i);
            }
        }
    }
    // Segments and their keepers / floors.
    let mut segs: BTreeMap<(u8, u64), (std::collections::BTreeSet<String>, usize, usize)> = BTreeMap::new();
    for it in &v.items {
        for idx in &it.segments {
            let e = segs.entry((it.family, *idx)).or_insert_with(|| (Default::default(), it.floor.unwrap_or(0), 0));
            e.1 = e.1.max(it.floor.unwrap_or(0));
            e.2 = it.copies;
            for (n, h) in &it.holders {
                if h.registered || h.keeps {
                    e.0.insert(n.clone());
                }
            }
        }
    }
    let total = v.items.len();
    let mut out = Vec::new();
    for n in ids.iter().filter(|n| want.as_ref().is_none_or(|w| *n == w || n.starts_with(w.as_str()))) {
        let mut w = who(n, &v.me);
        let mut by = json!({"keeper": 0, "publisher": 0, "grace": 0, "viewer": 0});
        for it in &v.items {
            if let Some(h) = it.holders.get(n) {
                let r = h.role(it.new);
                by[r] = json!(by[r].as_u64().unwrap_or(0) + 1);
            }
        }
        let held = held_by.get(n.as_str()).map_or(0, |s| s.len());
        let share = if *n == v.me {
            let mb = fogstore::keep_mb(node);
            let used = fogstore::kept_mb_used(node);
            json!({"mb": mb, "used_mb": used, "free_mb": mb.saturating_sub(used), "known": true})
        } else {
            match ann.get(n.as_str()) {
                Some(f) if f.get("km").is_some_and(|x| !x.is_null()) => {
                    json!({"mb": f["km"], "used_mb": f["ku"], "free_mb": f["room"], "known": true})
                }
                Some(f) => json!({"mb": Value::Null, "used_mb": Value::Null, "free_mb": f["room"], "known": false}),
                None => json!({"known": false}),
            }
        };
        let kept: Vec<Value> = segs
            .iter()
            .filter(|(k, s)| s.0.contains(n) || (*n == v.me && v.kept.contains(k)))
            .map(|((f, i), s)| json!({"name": seg_name(*f, *i), "label": format!("{}·{}/{}", seg_name(*f, *i), s.0.len(), s.1), "keepers": s.0.len(), "counted": s.2, "floor": s.1}))
            .collect();
        let mine = held_by.get(n.as_str());
        let mut overlap: Vec<(usize, &String)> = ids
            .iter()
            .filter(|o| *o != n)
            .filter_map(|o| {
                let shared = mine.zip(held_by.get(o.as_str())).map_or(0, |(a, b)| a.intersection(b).count());
                (shared > 0).then_some((shared, o))
            })
            .collect();
        overlap.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        w["online"] = json!(ann.contains_key(n.as_str()) || *n == v.me);
        w["keep_share"] = share;
        w["blobs"] = by;
        w["held"] = json!(held);
        w["in_the_fog"] = json!(total - held);
        w["segments_kept"] = json!(kept);
        w["earning_weight"] = json!(v.weights.get(n).copied().unwrap_or(0));
        w["registered"] = json!(v.weights.contains_key(n));
        w["overlap"] = json!(overlap.into_iter().take(OVERLAP_TOP).map(|(c, o)| {
            let mut x = who(o, &v.me);
            x["shared"] = json!(c);
            x
        }).collect::<Vec<_>>());
        out.push(w);
    }
    out.sort_by(|a, b| b["held"].as_u64().cmp(&a["held"].as_u64()).then(a["node"].as_str().cmp(&b["node"].as_str())));
    Ok(json!({
        "chain": crate::arcsvc::defined().map(|p| p.key),
        "height": v.tip,
        "node": v.me,
        "blobs": total,
        "segments": segs.iter().map(|((f, i), s)| json!({"name": seg_name(*f, *i), "keepers": s.0.iter().map(|n| who(n, &v.me)).collect::<Vec<_>>(), "counted": s.2, "floor": s.1})).collect::<Vec<_>>(),
        "nodes": out,
        "note": "earning_weight: the registry's pay weight (segment bytes split among each segment's audited replicas); keep shares of other nodes as their announcements say",
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn holder_states_roles_and_classes() {
        let h = Holder { registered: true, audited: true, ..Default::default() };
        assert_eq!((h.state(), h.role(false)), ("registered+audited", "keeper"));
        assert_eq!(Holder { registered: true, ..Default::default() }.state(), "registered");
        assert_eq!(Holder { keeps: true, bytes: true, ..Default::default() }.state(), "announced");
        assert_eq!(Holder { here: true, bytes: true, ..Default::default() }.state(), "here");
        assert_eq!(Holder { bytes: true, publisher: true, ..Default::default() }.role(true), "publisher");
        assert_eq!(Holder { bytes: true, ..Default::default() }.role(true), "grace");
        assert_eq!(Holder { bytes: true, ..Default::default() }.role(false), "viewer");
        assert_eq!(class_of(&json!("Boost")), Some(3));
        assert_eq!(class_of(&json!(1)), Some(1));
        assert_eq!(class_of(&json!("nope")), None);
        assert_eq!(seg_name(FAMILY_BLOBS, 3), "P#3");
        assert_eq!(seg_name(FAMILY_BLOBS_TEMP, 0), "T#0");
    }

    #[test]
    fn counted_copies_are_registered_and_audited_only() {
        let me = "aa".to_string();
        let s = fogstore::SegHolders { registered: Some(vec![("r1".into(), true), ("r2".into(), false)]), keepers: vec!["r1".into(), "k2".into(), me.clone()] };
        assert_eq!(s.counted(), 1);
        assert_eq!(s.announced_only(), 2, "k2 and this arcade: announced, not yet counted");
        assert_eq!(s.for_decision(), 1);
        let o = fogstore::SegHolders { registered: None, keepers: vec!["k1".into(), "k2".into()] };
        assert_eq!(o.counted(), 0);
        assert_eq!(o.announced_only(), 2);
        assert_eq!(o.for_decision(), 2);
    }
}
