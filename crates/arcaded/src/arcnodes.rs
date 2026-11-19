//! `explorer.arc_nodes` (the Storage app's Nodes and Coins tabs): per arcade on the mesh, and this one, what it keeps
//! (each chain's mode, indexes and bytes), how much of it is sealed (earning), how much is ready to seal (blocks of a
//! listed chain it keeps in full, not sealed yet) and how much can't be sealed yet (unlisted chains, every index,
//! anything an old "beta" arcade keeps: it has no sealing); per chain, its keepers, its size and how far the ARCADE
//! chain is in cutting and sealing it; and network totals.
//!
//! Where the numbers come from, and how sure they are (each chain's `size`):
//! - `measured`: what that arcade measured itself (crate::sizes): its "sz" frame when linked to us, else the `sz` part of
//!   its signed hold report, which is passed on across the mesh (both carry the same groups); ours for this arcade.
//! - `estimated`: no measurement of its own reached us, so the network's median for that chain and mode
//!   (`crate::sizes::for_chain`), or, when no arcade reports the chain at all, a rough built-in figure ([`ROUGH`]).
//!   Old (beta) arcades report no sizes: theirs are always estimated.
//! - `unknown`: nothing to go by.
//!
//! Sealed bytes are the ARCADE chain's registry (every arcade running it agrees): each replica owner's replicas, by
//! the chain of their segment. An owner is an arcade's mesh identity, so it is matched to an arcade by node id, never
//! by address or @tag (one IP or one operator can run several arcades). Owners not heard on the mesh lately come in
//! `offmesh`: what else they keep isn't visible.
//!
//! Gossip and the ARCADE chain only, never asked on: answered at once from what this arcade already knows, the same
//! for at most [`CACHE_SECS`] so a page that refreshes often costs nothing; at most [`MAX_ARCADES`] arcades and
//! [`MAX_OFFMESH`] off-mesh owners (`truncated` says so).

use crate::node::Node;
use arcade_chain::params::{by_key, by_net};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const MAX_ARCADES: usize = 500;
const MAX_CHAINS: usize = 40;
const MAX_OFFMESH: usize = 500;
/// An answer stands this long.
const CACHE_SECS: u64 = 10;
/// How long a hold report counts (as mesh.overview).
const HOLD_FRESH: u64 = 600;
/// How long an offer or a copy announcement counts (as mesh.overview).
const FRESH: u64 = 900;
/// A size report older than this isn't used (as crate::sizes::for_chain).
const SIZE_FRESH: u64 = 30 * 86400;

/// Rough full sizes (headers, blocks and coins) of chains no arcade reports a size for, in bytes: a chain kept
/// through its own Core isn't in an arcade's tables, so nothing measures it. Shown as estimated.
const ROUGH: &[(&str, u64)] = &[("btc", 700_000_000_000)];

#[derive(Clone, Debug, Default)]
pub(crate) struct ChainIn {
    /// V1.0's key ("pep", "doge", ...).
    pub key: String,
    /// full, chain, index, light, shared, borrowed; "serves" when only its offers name it (no hold heard).
    pub mode: String,
    pub h: Option<u64>,
    pub ix: Vec<String>,
    /// Its own measurement ({b, ix}: crate::sizes::split).
    pub measured: Option<Value>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ArcadeIn {
    /// Its mesh identity (hex).
    pub node: String,
    pub tag: Option<String>,
    pub name: Option<String>,
    /// What its "op me" says ("V1.0" from every V1.0 arcade).
    pub version: Option<String>,
    pub me: bool,
    pub linked: bool,
    /// An old arcade (not V1.0): it keeps chains through its Core and can't seal.
    pub beta: bool,
    pub chains: Vec<ChainIn>,
    /// Its machine line when it shares system info (crate::sysinfo).
    pub machine: Option<Value>,
}

/// The ARCADE chain's side, when it runs here.
#[derive(Clone, Debug, Default)]
pub(crate) struct ArcIn {
    /// Its key (arc, arctest).
    pub chain: String,
    pub height: u64,
    /// The chains listed for the next block.
    pub listed: BTreeSet<String>,
    /// Replica owner (hex) -> chain key -> (bytes, replicas).
    pub sealed: BTreeMap<String, BTreeMap<String, (u64, u64)>>,
    /// Replica owner (hex) -> its @tag on the mesh, when known.
    pub tags: BTreeMap<String, Option<String>>,
    /// Chain key -> [segments, bytes, replicas, unheld segments] of every family.
    pub segs: BTreeMap<String, [u64; 4]>,
    /// The copies each segment is aimed at (the least copy floor).
    pub floor: u64,
}

/// `explorer.arc_nodes`.
pub fn read(node: &Node) -> Value {
    static C: OnceLock<Mutex<Option<(Instant, Value)>>> = OnceLock::new();
    let c = C.get_or_init(Default::default);
    if let Some((at, v)) = c.lock().unwrap().as_ref() {
        if at.elapsed() < Duration::from_secs(CACHE_SECS) {
            return v.clone();
        }
    }
    let v = gather(node);
    *c.lock().unwrap() = Some((Instant::now(), v.clone()));
    v
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array().into_iter().flatten().filter_map(|x| x.as_str().map(String::from)).take(16).collect()
}

fn gather(node: &Node) -> Value {
    let now = arcade_app::now();
    let links: BTreeSet<String> = node.links.lock().unwrap().values().map(|l| l.node_id.clone()).collect();
    let offers = node.offers.lock().unwrap().clone();
    let holds = node.holds.lock().unwrap().clone();
    let sizes = node.mesh_sizes.lock().unwrap().clone();
    let cb = crate::coreboot::heard_all();
    #[cfg(feature = "oldimport")]
    let (lx, dx) = (crate::lxfetch::heard_all(), crate::dxfetch::heard_all());
    #[cfg(not(feature = "oldimport"))]
    let (lx, dx): (Vec<(String, u64, Value)>, Vec<(String, String, u64, Value)>) = (Vec::new(), Vec::new());

    let mut arcades = vec![me_in(node)];
    let ids = crate::meshview::heard_ids(node);
    let truncated = ids.len() + 1 > MAX_ARCADES;
    for id in ids.into_iter().take(MAX_ARCADES - 1) {
        let op = crate::meshview::op_of(&id).unwrap_or(Value::Null);
        let hold = holds.get(&id).filter(|(at, _)| *at + HOLD_FRESH >= now).map(|x| x.1.clone());
        let machine = crate::sysinfo::machine(&id);
        // V1.0 arcades say so in their "op me", and only they send hold reports and system info.
        let v1 = op["version"] == "V1.0" || hold.is_some() || machine.is_some();
        let own = sizes.get(&id).filter(|(t, _)| *t + SIZE_FRESH >= now).map(|x| x.1.clone());
        let mut chains: BTreeMap<String, ChainIn> = BTreeMap::new();
        if let Some(h) = &hold {
            for (net, v) in h.as_object().into_iter().flatten() {
                let Some(p) = by_net(net).filter(|_| v.is_object()) else { continue };
                let measured = own.as_ref().and_then(|m| m[net.as_str()]["groups"].as_object().map(crate::sizes::split)).or_else(|| v.get("sz").filter(|x| x.is_object()).cloned());
                chains.insert(p.key.into(), ChainIn { key: p.key.into(), mode: v["mode"].as_str().unwrap_or("").into(), h: v["h"].as_u64(), ix: strs(&v["ix"]), measured });
            }
        } else {
            // No hold report: what its offers and copy announcements say. An old arcade keeps those chains whole, in
            // its Core; a V1.0 one without a hold lately is only known to serve them.
            let mode = if v1 { "serves" } else { "full" };
            let mut put = |net: &str, h: Option<u64>, ix: Option<&str>| {
                let Some(p) = by_net(net) else { return };
                let c = chains.entry(p.key.into()).or_insert_with(|| ChainIn { key: p.key.into(), mode: mode.into(), ..Default::default() });
                c.h = c.h.max(h);
                if let Some(ix) = ix.filter(|i| !c.ix.iter().any(|x| x == i)) {
                    c.ix.push(ix.into());
                }
            };
            if let Some(o) = offers.get(&id).filter(|o| o.at + FRESH >= now) {
                for (net, h) in o.cr.iter().chain(o.bk.iter()) {
                    put(net, Some(*h), None);
                }
            }
            if !v1 {
                for (_, _, nets) in cb.iter().filter(|x| x.0 == id && x.1 + FRESH >= now) {
                    for (net, v) in nets.as_object().into_iter().flatten() {
                        put(net, v["h"].as_u64(), None);
                    }
                }
                for (_, _, nets) in lx.iter().filter(|x| x.0 == id && x.1 + FRESH >= now) {
                    for (net, v) in nets.as_object().into_iter().flatten() {
                        put(net, v["h"].as_u64(), Some("ledger"));
                    }
                }
                for (_, net, _, _) in dx.iter().filter(|x| x.0 == id && x.2 + FRESH >= now) {
                    put(net, None, Some("ord"));
                }
            }
        }
        arcades.push(ArcadeIn {
            tag: op["tag"].as_str().map(String::from).or_else(|| crate::meshview::tag_of(&id)),
            name: op["name"].as_str().map(String::from),
            version: op["version"].as_str().map(String::from),
            me: false,
            linked: links.contains(&id),
            beta: !v1,
            chains: chains.into_values().collect(),
            machine,
            node: id,
        });
    }
    // The network's sizes per chain, for what nobody measured.
    let keys: BTreeSet<String> = arcades.iter().flat_map(|a| a.chains.iter().map(|c| c.key.clone())).collect();
    let est: BTreeMap<String, Value> = keys.into_iter().map(|k| (k.clone(), crate::sizes::for_chain(node, &k))).collect();
    let arc = crate::arcsvc::with_chain(|net, chain| arc_in(net, chain));
    assemble(arcades, &est, arc.as_ref(), truncated)
}

/// This arcade, from its own hold report and measurement.
fn me_in(node: &Node) -> ArcadeIn {
    let hold = crate::shares::hold_frame(node).head;
    let mine = node.my_sizes.lock().unwrap().clone();
    let mut chains = Vec::new();
    for (net, v) in hold["chains"].as_object().into_iter().flatten() {
        let Some(p) = by_net(net) else { continue };
        let mode = if crate::borrow::is_borrowed(p.key) { "borrowed" } else { v["mode"].as_str().unwrap_or("") };
        let measured = mine.get(net.as_str()).and_then(|m| m["groups"].as_object().map(crate::sizes::split));
        chains.push(ChainIn { key: p.key.into(), mode: mode.into(), h: v["h"].as_u64(), ix: strs(&v["ix"]), measured });
    }
    let tag = node.app.tag_keys(node.params.chain_no).unwrap_or_default().into_iter().find_map(|k| k.tag).or_else(|| crate::handlegate::own_proof(node).map(|(t, _)| t));
    ArcadeIn {
        node: node.id_hex(),
        name: tag.as_ref().map(|t| format!("@{t}")),
        tag,
        version: option_env!("ARCADE_VERSION").map(String::from),
        me: true,
        linked: false,
        beta: false,
        chains,
        machine: crate::sysinfo::own_machine(),
    }
}

fn arc_in(net: &arcade_arc::params::Net, chain: &crate::arcsvc::LocalChain) -> ArcIn {
    let reg = chain.storage();
    let height = chain.tip().height;
    let mut out = ArcIn {
        chain: net.key.into(),
        height,
        listed: net.listed_at(height + 1).map(String::from).collect(),
        floor: arcade_arc::blob::floor(arcade_arc::blob::Class::Temporary) as u64,
        ..Default::default()
    };
    let mut by_id: HashMap<arcade_arc::merkle::Hash, (String, u64, u64)> = HashMap::new();
    for l in crate::arcstats::all_listed(reg) {
        by_id.insert(l.id, (l.entry.chain.clone(), l.entry.bytes, 0));
    }
    for (_, seg, owner, _) in reg.all_replicas() {
        let Some(s) = by_id.get_mut(&seg) else { continue };
        s.2 += 1;
        let e = out.sealed.entry(hex::encode(owner)).or_default().entry(s.0.clone()).or_default();
        e.0 += s.1;
        e.1 += 1;
    }
    for (chain, bytes, n) in by_id.values() {
        let e = out.segs.entry(chain.clone()).or_default();
        e[0] += 1;
        e[1] += bytes;
        e[2] += n;
        e[3] += (*n == 0) as u64;
    }
    out.tags = out.sealed.keys().map(|o| (o.clone(), crate::meshview::tag_of(o))).collect();
    out
}

/// The size mode an estimate is looked up under (crate::sizes::for_chain's "modes").
fn size_mode(c: &ChainIn) -> &str {
    match c.mode.as_str() {
        m @ ("light" | "chain" | "index" | "full" | "shared") => m,
        _ => "full",
    }
}

/// One chain on one arcade: (the chain's own bytes, [(index, bytes)], size label).
fn sizes_of(c: &ChainIn, est: &BTreeMap<String, Value>) -> (Option<u64>, Vec<(String, Option<u64>)>, &'static str) {
    if c.mode == "borrowed" {
        return (Some(0), Vec::new(), "measured");
    }
    let e = est.get(&c.key);
    let est_ix = |name: &str| e.and_then(|e| e["indexes"][name]["bytes"].as_u64());
    if let Some(m) = &c.measured {
        let mut ix: Vec<(String, Option<u64>)> = m["ix"].as_object().into_iter().flatten().map(|(n, b)| (n.clone(), b.as_u64())).collect();
        let mut est_used = false;
        for name in &c.ix {
            if !ix.iter().any(|(n, _)| n == name) {
                // An index it keeps that its measurement didn't count (yet): the network's figure.
                let b = est_ix(name);
                est_used |= b.is_some();
                ix.push((name.clone(), b));
            }
        }
        return (m["b"].as_u64(), ix, if est_used { "estimated" } else { "measured" });
    }
    let b = e.and_then(|e| e["modes"][size_mode(c)].as_u64());
    let ix: Vec<(String, Option<u64>)> = c.ix.iter().map(|n| (n.clone(), est_ix(n))).collect();
    match b {
        Some(b) => (Some(b), ix, "estimated"),
        None => match ROUGH.iter().find(|(k, _)| *k == c.key).filter(|_| matches!(size_mode(c), "full" | "chain")) {
            Some((_, b)) => (Some(*b), ix, "estimated"),
            None => (None, ix, "unknown"),
        },
    }
}

fn median(mut v: Vec<u64>) -> Option<u64> {
    v.sort_unstable();
    v.get(v.len() / 2).copied()
}

fn chain_name(key: &str) -> String {
    by_key(key).map(|p| p.name.to_string()).unwrap_or_else(|| key.to_string())
}

/// The answer, from what was gathered (pure: the tests build its inputs).
pub(crate) fn assemble(arcades: Vec<ArcadeIn>, est: &BTreeMap<String, Value>, arc: Option<&ArcIn>, truncated: bool) -> Value {
    let listed = |k: &str| arc.is_some_and(|a| a.listed.contains(k));
    let mut on_mesh: BTreeSet<String> = BTreeSet::new();
    let mut rows = Vec::new();
    // Per chain: (full keepers, beta keepers, other keepers by mode, measured chain bytes, measured index bytes, heights).
    #[derive(Default)]
    struct Net {
        full: Vec<String>,
        beta: Vec<String>,
        other: BTreeMap<String, u64>,
        b: Vec<u64>,
        ix: Vec<u64>,
        h: Vec<u64>,
    }
    let mut nets: BTreeMap<String, Net> = BTreeMap::new();
    let mut t = [0u64; 5]; // kept, sealed, ready, other, estimated
    let (mut beta_n, mut keeping) = (0usize, 0usize);
    for a in &arcades {
        on_mesh.insert(a.node.clone());
        let sealed_of = arc.and_then(|x| x.sealed.get(&a.node));
        let who = a.tag.as_ref().map(|t| format!("@{t}")).or_else(|| a.name.clone()).unwrap_or_else(|| a.node[..a.node.len().min(8)].to_string());
        let (mut kept, mut ready, mut other, mut estd, mut unknown) = (0u64, 0u64, 0u64, 0u64, false);
        let mut chains = Vec::new();
        for c in a.chains.iter().take(MAX_CHAINS) {
            let (b, ix, size) = sizes_of(c, est);
            let ixb: u64 = ix.iter().filter_map(|x| x.1).sum();
            let bb = b.unwrap_or(0);
            let (sb, sn) = sealed_of.and_then(|s| s.get(&c.key)).copied().unwrap_or((0, 0));
            let sealable = !a.beta && c.mode == "full" && listed(&c.key);
            let r = if sealable { bb.saturating_sub(sb) } else { 0 };
            kept += bb + ixb;
            ready += r;
            other += ixb + if sealable { 0 } else { bb };
            if size != "measured" {
                estd += bb + ixb;
            }
            unknown |= size == "unknown";
            if c.mode != "borrowed" {
                let n = nets.entry(c.key.clone()).or_default();
                if c.mode == "full" {
                    if a.beta { n.beta.push(who.clone()) } else { n.full.push(who.clone()) }
                } else {
                    *n.other.entry(c.mode.clone()).or_default() += 1;
                }
                if let (Some(m), "full") = (&c.measured, c.mode.as_str()) {
                    n.b.extend(m["b"].as_u64());
                    n.ix.push(m["ix"].as_object().into_iter().flatten().filter_map(|(_, x)| x.as_u64()).sum());
                }
                n.h.extend(c.h);
            }
            chains.push(json!({
                "key": c.key, "name": chain_name(&c.key), "mode": c.mode, "h": c.h, "listed": listed(&c.key), "sealable": sealable,
                "blocks_bytes": b, "index_bytes": ixb,
                "indexes": ix.iter().map(|(n, b)| json!({"name": n, "bytes": b})).collect::<Vec<_>>(),
                "size": size, "sealed_bytes": sb, "sealed_copies": sn, "ready_bytes": r,
            }));
        }
        let sealed: u64 = sealed_of.map(|s| s.values().map(|x| x.0).sum()).unwrap_or(0);
        let copies: u64 = sealed_of.map(|s| s.values().map(|x| x.1).sum()).unwrap_or(0);
        t[0] += kept;
        t[1] += sealed;
        t[2] += ready;
        t[3] += other;
        t[4] += estd;
        beta_n += a.beta as usize;
        keeping += (kept > 0) as usize;
        rows.push(json!({
            "node": a.node, "tag": a.tag, "name": a.name, "version": a.version, "me": a.me, "linked": a.linked, "beta": a.beta,
            "chains": chains, "kept_bytes": kept, "sealed_bytes": sealed, "sealed_copies": copies, "ready_bytes": ready, "other_bytes": other,
            "estimated_bytes": estd, "size": if unknown { "unknown" } else if estd > 0 { "estimated" } else { "measured" },
            "machine": a.machine,
        }));
    }
    // Owners of sealed copies not heard on the mesh lately.
    let mut off: Vec<Value> = arc
        .map(|x| {
            x.sealed
                .iter()
                .filter(|(o, _)| !on_mesh.contains(*o))
                .map(|(o, s)| json!({"node": &o[..o.len().min(16)], "owner": o, "tag": x.tags.get(o).cloned().flatten(),
                                      "sealed_bytes": s.values().map(|v| v.0).sum::<u64>(), "sealed_copies": s.values().map(|v| v.1).sum::<u64>()}))
                .collect()
        })
        .unwrap_or_default();
    off.sort_by_key(|o| std::cmp::Reverse(o["sealed_bytes"].as_u64()));
    let off_total = off.len();
    let off_sealed: u64 = off.iter().filter_map(|o| o["sealed_bytes"].as_u64()).sum();
    off.truncate(MAX_OFFMESH);
    // Every chain someone keeps, and every chain with sealed chunks.
    let mut keys: BTreeSet<String> = nets.keys().cloned().collect();
    if let Some(a) = arc {
        keys.extend(a.segs.keys().filter(|k| by_key(k).is_some()).cloned());
    }
    let chains: Vec<Value> = keys
        .into_iter()
        .map(|k| {
            let n = nets.remove(&k).unwrap_or_default();
            let e = est.get(&k);
            // Its size: the median of what its full keepers measured, else the network's report, else rough.
            let (b, ix, size) = match median(n.b.clone()) {
                Some(b) => (Some(b), median(n.ix.clone()), "measured"),
                None => match e.and_then(|e| e["modes"]["full"].as_u64()) {
                    Some(b) => (Some(b), None, "estimated"),
                    None => (ROUGH.iter().find(|(r, _)| *r == k).map(|r| r.1), None, if ROUGH.iter().any(|(r, _)| *r == k) { "estimated" } else { "unknown" }),
                },
            };
            let ix = ix.or_else(|| {
                let o = e?["indexes"].as_object()?;
                (!o.is_empty()).then(|| o.values().filter_map(|x| x["bytes"].as_u64()).sum())
            });
            let s = arc.and_then(|a| a.segs.get(&k)).copied().unwrap_or_default();
            json!({
                "key": k, "name": chain_name(&k), "listed": listed(&k),
                "height": e.and_then(|e| e["height"].as_u64()).or_else(|| n.h.iter().max().copied()),
                "full": n.full, "beta": n.beta, "other_modes": n.other,
                "blocks_bytes": b, "index_bytes": ix, "size": size,
                "segments": s[0], "segment_bytes": s[1], "replicas": s[2], "unheld": s[3],
            })
        })
        .collect();
    json!({
        "chain": arc.map(|a| a.chain.clone()), "height": arc.map(|a| a.height), "arcade": arc.is_some(),
        "floor": arc.map(|a| a.floor),
        "arcades": rows,
        "offmesh": off,
        "chains": chains,
        "totals": {
            "arcades": arcades.len(), "keeping": keeping, "beta": beta_n, "linked": arcades.iter().filter(|a| a.linked).count(),
            "kept_bytes": t[0], "sealed_bytes": t[1], "ready_bytes": t[2], "other_bytes": t[3], "estimated_bytes": t[4],
            "offmesh": off_total, "offmesh_sealed_bytes": off_sealed,
        },
        "truncated": truncated || off_total > MAX_OFFMESH,
        "note": "Kept bytes per arcade: what it measured (size \"measured\"), else the network's figure for that chain and mode, or a rough one (\"estimated\"). Sealed: the ARCADE registry's replicas. Ready: blocks of a listed chain kept in full, not sealed yet. Beta (old) arcades can't seal.",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    const G: u64 = 1_000_000_000;

    fn id(n: u8) -> String {
        format!("{:02x}", n).repeat(32)
    }
    fn measured(b: u64, ix: &[(&str, u64)]) -> Option<Value> {
        Some(json!({"b": b, "ix": ix.iter().map(|(n, x)| (n.to_string(), json!(x))).collect::<Map<_, _>>()}))
    }
    fn chain(key: &str, mode: &str, ix: &[&str], m: Option<Value>) -> ChainIn {
        ChainIn { key: key.into(), mode: mode.into(), h: Some(100), ix: ix.iter().map(|s| s.to_string()).collect(), measured: m }
    }
    fn arcade(n: u8, tag: Option<&str>, beta: bool, chains: Vec<ChainIn>) -> ArcadeIn {
        ArcadeIn { node: id(n), tag: tag.map(String::from), version: Some(if beta { "0.9.80" } else { "V1.0" }.into()), beta, chains, linked: true, ..Default::default() }
    }
    fn est() -> BTreeMap<String, Value> {
        let mut e = BTreeMap::new();
        e.insert("pep".into(), json!({"height": 1_298_482, "modes": {"full": 18 * G, "chain": 18 * G, "light": G / 10}, "indexes": {"ledger": {"bytes": 30 * G}, "ord": {"bytes": 4 * G}}}));
        e.insert("doge".into(), json!({"height": 6_462_760, "modes": {"full": 252 * G}, "indexes": {"ord": {"bytes": 299 * G}}}));
        e.insert("btc".into(), json!({"height": null, "modes": {"full": null}, "indexes": {}}));
        e
    }
    fn arc() -> ArcIn {
        let mut a = ArcIn { chain: "arctest".into(), height: 2519, floor: 3, ..Default::default() };
        a.listed = ["pep", "peptest", "flop", "btc"].iter().map(|s| s.to_string()).collect();
        a.sealed.entry(id(1)).or_default().insert("pep".into(), (2 * G, 4));
        a.sealed.entry(id(1)).or_default().insert("arctest".into(), (G / 2, 1));
        a.sealed.entry(id(3)).or_default().insert("pep".into(), (G, 2));
        // An owner no arcade on the mesh is.
        a.sealed.entry(id(9)).or_default().insert("pep".into(), (G / 4, 1));
        a.tags.insert(id(9), Some("sadfrog".into()));
        a.segs.insert("pep".into(), [28, 4_700_000_000, 18, 11]);
        a
    }
    fn row<'a>(v: &'a Value, n: u8) -> &'a Value {
        v["arcades"].as_array().unwrap().iter().find(|a| a["node"] == id(n)).unwrap()
    }

    #[test]
    fn sealed_ready_and_the_rest_add_up_per_arcade() {
        let a = vec![arcade(1, Some("darrellfiori"), false, vec![chain("pep", "full", &["ledger"], measured(18 * G, &[("ledger", 30 * G)])), chain("doge", "full", &[], measured(252 * G, &[]))])];
        let v = assemble(a, &est(), Some(&arc()), false);
        let r = row(&v, 1);
        // Sealed counts every chain's replicas (ARCADE's own too); ready only the listed chain kept in full.
        assert_eq!(r["sealed_bytes"], json!(2 * G + G / 2));
        assert_eq!(r["ready_bytes"], json!(16 * G));
        // Doge isn't listed here: its blocks and every index can't be sealed yet.
        assert_eq!(r["other_bytes"], json!(252 * G + 30 * G));
        assert_eq!(r["kept_bytes"], json!(18 * G + 30 * G + 252 * G));
        assert_eq!(r["size"], "measured");
        assert_eq!(r["chains"][0]["sealable"], true);
        assert_eq!(v["totals"]["ready_bytes"], json!(16 * G));
    }

    #[test]
    fn a_beta_arcade_keeps_but_cannot_seal_and_its_sizes_are_estimated() {
        let a = vec![arcade(2, Some("darrellfiori"), true, vec![chain("pep", "full", &["ledger"], None)])];
        let v = assemble(a, &est(), Some(&arc()), false);
        let r = row(&v, 2);
        assert_eq!(r["beta"], true);
        assert_eq!(r["ready_bytes"], json!(0));
        assert_eq!(r["other_bytes"], json!(18 * G + 30 * G));
        assert_eq!(r["size"], "estimated");
        assert_eq!(r["estimated_bytes"], r["kept_bytes"]);
        assert_eq!(v["chains"][0]["beta"], json!(["@darrellfiori"]));
        assert_eq!(v["totals"]["beta"], json!(1));
    }

    #[test]
    fn arcades_on_one_address_or_with_one_tag_stay_apart() {
        // Darrell's V1.0 arcade, his old one (same @tag) and @testwallet (same IP): three rows, matched by node id only.
        let a = vec![
            arcade(1, Some("darrellfiori"), false, vec![chain("pep", "full", &[], measured(18 * G, &[]))]),
            arcade(2, Some("darrellfiori"), true, vec![chain("pep", "full", &[], None)]),
            arcade(3, Some("testwallet"), false, vec![chain("pep", "full", &[], measured(18 * G, &[]))]),
        ];
        let v = assemble(a, &est(), Some(&arc()), false);
        assert_eq!(v["arcades"].as_array().unwrap().len(), 3);
        assert_eq!(row(&v, 1)["sealed_bytes"], json!(2 * G + G / 2));
        assert_eq!(row(&v, 2)["sealed_bytes"], json!(0));
        assert_eq!(row(&v, 3)["sealed_bytes"], json!(G));
        let pep = &v["chains"][0];
        assert_eq!(pep["full"], json!(["@darrellfiori", "@testwallet"]));
        assert_eq!(pep["beta"], json!(["@darrellfiori"]));
    }

    #[test]
    fn holders_not_on_the_mesh_come_apart() {
        let a = vec![arcade(1, Some("a"), false, vec![]), arcade(3, Some("b"), false, vec![])];
        let v = assemble(a, &est(), Some(&arc()), false);
        assert_eq!(v["offmesh"].as_array().unwrap().len(), 1);
        assert_eq!(v["offmesh"][0]["owner"], json!(id(9)));
        assert_eq!(v["offmesh"][0]["node"], json!(&id(9)[..16]));
        assert_eq!(v["offmesh"][0]["tag"], "sadfrog");
        assert_eq!(v["totals"]["offmesh_sealed_bytes"], json!(G / 4));
    }

    #[test]
    fn a_chain_nobody_measures_gets_a_rough_estimate_or_none() {
        let a = vec![arcade(4, Some("shoe"), false, vec![chain("btc", "full", &[], None), chain("wjk", "full", &[], None)])];
        let v = assemble(a, &est(), Some(&arc()), false);
        let r = row(&v, 4);
        let btc = r["chains"].as_array().unwrap().iter().find(|c| c["key"] == "btc").unwrap();
        assert_eq!(btc["blocks_bytes"], json!(700 * G));
        assert_eq!(btc["size"], "estimated");
        // Listed and kept in full: the estimate counts as ready.
        assert_eq!(btc["ready_bytes"], json!(700 * G));
        let wjk = r["chains"].as_array().unwrap().iter().find(|c| c["key"] == "wjk").unwrap();
        assert_eq!(wjk["blocks_bytes"], Value::Null);
        assert_eq!(wjk["size"], "unknown");
        assert_eq!(r["size"], "unknown");
        let net_btc = v["chains"].as_array().unwrap().iter().find(|c| c["key"] == "btc").unwrap();
        assert_eq!(net_btc["size"], "estimated");
        assert_eq!(net_btc["blocks_bytes"], json!(700 * G));
    }

    #[test]
    fn an_index_the_measurement_missed_is_estimated() {
        let a = vec![arcade(1, None, false, vec![chain("pep", "full", &["ledger", "ord"], measured(18 * G, &[("ledger", 30 * G)]))])];
        let v = assemble(a, &est(), None, false);
        let c = &row(&v, 1)["chains"][0];
        assert_eq!(c["index_bytes"], json!(34 * G));
        assert_eq!(c["size"], "estimated");
        // No ARCADE chain here: nothing is listed, nothing ready, nothing sealed.
        assert_eq!(c["listed"], false);
        assert_eq!(row(&v, 1)["ready_bytes"], json!(0));
        assert_eq!(v["arcade"], false);
    }

    #[test]
    fn the_coins_view_counts_keepers_cuts_and_copies() {
        let a = vec![
            arcade(1, Some("a"), false, vec![chain("pep", "full", &[], measured(18 * G, &[("ledger", 30 * G)])), chain("doge", "light", &[], None)]),
            arcade(3, Some("b"), false, vec![chain("pep", "full", &[], measured(20 * G, &[("ledger", 32 * G)])), chain("doge", "borrowed", &[], None)]),
        ];
        let v = assemble(a, &est(), Some(&arc()), false);
        let pep = v["chains"].as_array().unwrap().iter().find(|c| c["key"] == "pep").unwrap();
        assert_eq!(pep["listed"], true);
        assert_eq!(pep["size"], "measured");
        assert_eq!(pep["blocks_bytes"], json!(20 * G), "the median of two is the upper");
        assert_eq!(pep["segments"], json!(28));
        assert_eq!(pep["segment_bytes"], json!(4_700_000_000u64));
        assert_eq!(pep["replicas"], json!(18));
        assert_eq!(pep["unheld"], json!(11));
        assert_eq!(pep["height"], json!(1_298_482));
        let doge = v["chains"].as_array().unwrap().iter().find(|c| c["key"] == "doge").unwrap();
        assert_eq!(doge["full"], json!([]));
        assert_eq!(doge["other_modes"], json!({"light": 1}), "a borrowed chain isn't kept");
        assert_eq!(doge["size"], "estimated");
        // The ARCADE chain itself isn't a coin row.
        assert!(!v["chains"].as_array().unwrap().iter().any(|c| c["key"] == "arctest"));
    }

    #[test]
    fn the_answer_stays_bounded() {
        let mut a = arc();
        for n in 0..(MAX_OFFMESH as u32 + 20) {
            a.sealed.entry(format!("{n:064x}")).or_default().insert("pep".into(), (1, 1));
        }
        let many: Vec<ChainIn> = (0..100).map(|_| chain("pep", "full", &[], None)).collect();
        let v = assemble(vec![arcade(1, None, false, many)], &est(), Some(&a), false);
        assert_eq!(v["offmesh"].as_array().unwrap().len(), MAX_OFFMESH);
        assert_eq!(v["truncated"], true);
        assert_eq!(row(&v, 1)["chains"].as_array().unwrap().len(), MAX_CHAINS);
        assert!(serde_json::to_vec(&v).unwrap().len() < 512 << 10);
    }

    #[test]
    fn hold_sizes_split_and_clean() {
        let g: Map<String, Value> = serde_json::from_value(json!({"headers": 1, "blocks": 10, "coins": 5, "ledger": 7, "ord": 0, "bogus": 3})).unwrap();
        assert_eq!(crate::sizes::split(&g), json!({"b": 16, "ix": {"ledger": 7}}));
        assert_eq!(crate::sizes::clean_hold(&json!({"b": 16, "ix": {"ledger": 7, "x": 1, "blocks": 2, "ord": "big"}})), Some(json!({"b": 16, "ix": {"ledger": 7}})));
        assert_eq!(crate::sizes::clean_hold(&json!({"ix": {}})), None);
        assert_eq!(crate::sizes::clean_hold(&Value::Null), None);
    }
}
