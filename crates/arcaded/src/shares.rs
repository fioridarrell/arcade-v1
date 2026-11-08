//! Shared storage: a chain's blocks, its ord records and its token balances spread over the
//! arcades that run it in "shared" mode, instead of every arcade keeping everything.
//!
//! * Blocks: 10,000-block ranges. Each range is held by COPIES arcades chosen by rendezvous
//!   hashing over the sharing arcades (anyone can work out who holds what). An arcade drops a
//!   range it isn't assigned only once at least COPIES other arcades announce holding it, and
//!   never inside the last 1,000 blocks. A dropped block keeps its header, txids, coins and
//!   address history; only the body goes (crate arcade_chaindb prune_block).
//! * Ord: inscription records in 64 groups by number (number % 64); the location map (which
//!   output holds each inscription) stays whole, because any transaction can move any
//!   inscription and the reinscription, parent and transfer rules depend on it.
//! * Tokens: PRC-20/DRC-20 ticks in 64 groups by ticker (sha256(tick)[0] % 64).
//!   Ord and token groups are chosen when the chain is installed (the least-held first) and
//!   kept, since changing them means re-walking the chain.
//! Anything not held here is asked of an arcade that holds it ("sh" q/a); blocks are checked
//! against our own headers and can also come from the coin's P2P network.
//!
//! Mesh (V1.0 only; existing arcades ignore unknown kinds):
//!   sh hold {chains:{key:{mode, h, blocks:[[from,to],...]|"all", ord:[g...]|"all", tok:[g...]|"all"}}}
//!   sh q {id, net, what:"insc"|"tick"|"block", arg}  ->  sh a {id, ok, result | err} [+ raw block]

use crate::node::Node;
use arcade_chain::params::ChainParams;
use arcade_chain::{Block, Hash256};
use arcade_mesh::link::{Frame, Link, LinkError};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::time::Duration;

pub const RANGE: u32 = 10_000;
pub const GROUPS: u8 = 64;
const KEEP_RECENT: u32 = 1_000;

/// How many arcades should hold each piece (ARCADE_SHARE_COPIES overrides, for tests).
pub fn copies() -> usize {
    std::env::var("ARCADE_SHARE_COPIES").ok().and_then(|v| v.parse().ok()).filter(|n: &usize| *n >= 1).unwrap_or(3)
}

fn me(node: &Node) -> String {
    hex::encode(node.identity.verifying_key().to_bytes())
}

fn score(id: &str, chain: &str, kind: &str, unit: u64) -> u64 {
    let h = Sha256::digest(format!("{id}:{chain}:{kind}:{unit}").as_bytes());
    u64::from_be_bytes(h[..8].try_into().unwrap())
}

/// Arcades sharing a chain (announced "shared" within the last hour), us included.
pub fn members(node: &Node, chain: &str) -> Vec<String> {
    let cutoff = arcade_app::now().saturating_sub(3600);
    let mut v: BTreeSet<String> = node
        .holds
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, (t, c))| *t >= cutoff && c[chain]["mode"] == "shared")
        .map(|(id, _)| id.clone())
        .collect();
    v.insert(me(node));
    v.into_iter().collect()
}

/// Is `unit` of `kind` assigned to us among `members`?
pub fn assigned(node: &Node, chain: &str, kind: &str, unit: u64, members: &[String]) -> bool {
    let n = copies();
    if members.len() <= n {
        return true;
    }
    let mine = me(node);
    let mut s: Vec<(u64, &String)> = members.iter().map(|m| (score(m, chain, kind, unit), m)).collect();
    s.sort_by(|a, b| b.0.cmp(&a.0));
    s.iter().take(n).any(|(_, m)| **m == mine)
}

fn holds_unit(v: &Value, kind: &str, unit: u64) -> bool {
    match &v[kind] {
        Value::String(s) if s == "all" => true,
        Value::Array(a) if kind == "blocks" => a.iter().any(|r| r[0].as_u64().is_some_and(|f| f <= unit) && r[1].as_u64().is_some_and(|t| unit <= t)),
        Value::Array(a) => a.iter().any(|g| g.as_u64() == Some(unit)),
        _ => false,
    }
}

/// Other arcades announcing they hold `unit` of `kind` on a chain: their node ids.
pub fn holders(node: &Node, chain: &str, kind: &str, unit: u64) -> Vec<String> {
    let cutoff = arcade_app::now().saturating_sub(3600);
    node.holds
        .lock()
        .unwrap()
        .iter()
        .filter(|(_, (t, c))| *t >= cutoff && holds_unit(&c[chain], kind, unit))
        .map(|(id, _)| id.clone())
        .collect()
}

// ---------------- what we hold ----------------

fn pruned_key(chain: &str) -> Vec<u8> {
    format!("pruned:{chain}").into_bytes()
}

/// Block ranges whose bodies we dropped.
pub fn pruned(node: &Node, chain: &str) -> BTreeSet<u32> {
    node.app
        .store
        .read(|r| r.get(arcade_app::META, &pruned_key(chain)))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or_default()
}

fn set_pruned(node: &Node, chain: &str, s: &BTreeSet<u32>) {
    let _ = node.app.store.write(|w| w.put(arcade_app::META, &pruned_key(chain), &serde_json::to_vec(s).unwrap()));
}

fn runs(held: impl Iterator<Item = u32>) -> Vec<[u32; 2]> {
    let mut out: Vec<[u32; 2]> = Vec::new();
    for r in held {
        match out.last_mut() {
            Some(x) if x[1] + 1 == r => x[1] = r,
            _ => out.push([r, r]),
        }
    }
    out
}

/// Our "sh hold" announcement.
pub fn hold_frame(node: &Node) -> Frame {
    let mut chains = Map::new();
    let home = node.params;
    let tip = |p: &ChainParams| node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height).unwrap_or(0);
    chains.insert(home.key.into(), json!({"mode": "full", "h": tip(home), "blocks": "all", "ord": "all", "tok": "all"}));
    for r in node.extra.lock().unwrap().values() {
        let p = r.params;
        let h = tip(p);
        let v = if r.mode == "shared" {
            let gone = pruned(node, p.key);
            let held = runs((0..=h / RANGE).filter(|x| !gone.contains(x)));
            json!({"mode": "shared", "h": h, "blocks": held, "ord": r.share["ord"], "tok": r.share["tok"]})
        } else {
            json!({"mode": r.mode, "h": h, "blocks": "all", "ord": "all", "tok": "all"})
        };
        chains.insert(p.key.into(), v);
    }
    Frame::new(json!({"t": "sh", "op": "hold", "chains": chains}))
}

/// Groups to keep when a chain is installed in shared mode: the least-held first, until we
/// carry our part (COPIES * 64 / sharing arcades), or all of them while few arcades share.
pub fn choose_groups(node: &Node, chain: &str, kind: &str) -> Value {
    let m = members(node, chain);
    let n = copies();
    if m.len() <= n {
        return json!("all");
    }
    let want = ((n * GROUPS as usize) + m.len() - 1) / m.len();
    let mine = me(node);
    let mut g: Vec<(usize, std::cmp::Reverse<u64>, u8)> =
        (0..GROUPS).map(|x| (holders(node, chain, kind, x as u64).len(), std::cmp::Reverse(score(&mine, chain, kind, x as u64)), x)).collect();
    g.sort();
    let mut pick: Vec<u8> = g.iter().take(want).map(|x| x.2).collect();
    pick.sort();
    json!(pick)
}

pub fn groups_of(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Array(a) => Some(a.iter().filter_map(|x| x.as_u64().map(|g| g as u8)).collect()),
        _ => None, // "all"
    }
}


// ---------------- fetching blocks we don't hold ----------------

/// Blocks by hash for a chain: from arcades that hold them, else the coin's P2P network.
/// Every block is checked against the header we already have.
pub fn fetch_blocks(node: &Node, p: &'static ChainParams, hashes: &[Hash256]) -> Result<Vec<Block>, String> {
    let check = |raws: Vec<Vec<u8>>| -> Result<Vec<Block>, String> {
        let mut out = Vec::new();
        for (raw, want) in raws.into_iter().zip(hashes) {
            let b = Block::parse_with(&raw, &p.format).map_err(|e| e.to_string())?;
            if b.id != *want || b.computed_merkle_root() != b.header.merkle_root {
                return Err(format!("block {want} did not check out"));
            }
            out.push(b);
        }
        if out.len() != hashes.len() {
            return Err("missing blocks".into());
        }
        Ok(out)
    };
    use arcade_chaindb::sync::BlockSource;
    let mut last_err = String::from("no source");
    for (_, addr, kind, _) in node.offers_for(p.key) {
        let got = match kind {
            "bk" => crate::meshsource::MeshSource::dial_chain(node, &addr, p).and_then(|mut s| s.get_blocks(hashes)),
            _ => crate::crsource::CoreShareSource::dial(node, &addr, p).and_then(|mut s| s.get_blocks(hashes)),
        };
        match got.and_then(check) {
            Ok(b) => return Ok(b),
            Err(e) => last_err = e,
        }
    }
    for a in node.coin_candidates(p).iter().take(6) {
        if let Ok(mut peer) = arcade_chain::p2p::Peer::connect(p, a, Duration::from_secs(15)) {
            match peer.get_blocks(hashes).map_err(|e| e.to_string()).and_then(check) {
                Ok(b) => return Ok(b),
                Err(e) => last_err = e,
            }
        }
    }
    Err(format!("could not fetch blocks for {}: {last_err}", p.name))
}

/// One pass of keeping our share right on a shared chain: drop ranges others hold enough
/// of, fetch back ranges assigned to us. Does at most a few ranges per pass.
pub fn balance(node: &Node, p: &'static ChainParams) -> Result<String, String> {
    let c = p.chain_no;
    let Some(tip) = node.chain.tip(c).map_err(|e| e.to_string())? else { return Ok(String::new()) };
    let m = members(node, p.key);
    let mut gone = pruned(node, p.key);
    let mut work = 0;
    let last_full = tip.height.saturating_sub(KEEP_RECENT) / RANGE; // ranges before this are settled
    for r in 0..last_full {
        if work >= 2 {
            break;
        }
        let mine = assigned(node, p.key, "blocks", r as u64, &m);
        let others = holders(node, p.key, "blocks", r as u64).len();
        let (from, to) = (r * RANGE, (r + 1) * RANGE - 1);
        if !mine && !gone.contains(&r) && others >= copies() {
            let mut freed = 0u64;
            for h in from..=to {
                if let Some(hash) = node.chain.hash_at(c, h).map_err(|e| e.to_string())? {
                    if let Some(rec) = node.chain.header(c, &hash).map_err(|e| e.to_string())? {
                        freed += node.chain.prune_block(p, rec.b).map_err(|e| e.to_string())?;
                    }
                }
            }
            gone.insert(r);
            set_pruned(node, p.key, &gone);
            let _ = node.app.event("shares", &format!("{}: blocks {from}-{to} are held by {others} other arcades; dropped them here ({:.1} MB)", p.name, freed as f64 / 1e6));
            work += 1;
        } else if mine && gone.contains(&r) {
            let mut h = from;
            while h <= to {
                let mut want: Vec<(u32, Hash256)> = Vec::new();
                while h <= to && want.len() < 50 {
                    if let Some(hash) = node.chain.hash_at(c, h).map_err(|e| e.to_string())? {
                        let b = node.chain.header(c, &hash).map_err(|e| e.to_string())?.map(|r| r.b).unwrap_or(0);
                        if b != 0 && node.chain.is_block_pruned(c, b).map_err(|e| e.to_string())? {
                            want.push((b, hash));
                        }
                    }
                    h += 1;
                }
                if want.is_empty() {
                    continue;
                }
                let hashes: Vec<Hash256> = want.iter().map(|x| x.1).collect();
                let blocks = fetch_blocks(node, p, &hashes)?;
                for ((b, _), blk) in want.iter().zip(blocks.iter()) {
                    node.chain.restore_block(p, *b, blk).map_err(|e| e.to_string())?;
                }
            }
            gone.remove(&r);
            set_pruned(node, p.key, &gone);
            let _ = node.app.event("shares", &format!("{}: blocks {from}-{to} are ours to hold; fetched them back", p.name));
            work += 1;
        }
    }
    let total = tip.height / RANGE + 1;
    Ok(format!("holding {} of {} block ranges · {} arcade{} sharing", total as usize - gone.len(), total, m.len(), if m.len() == 1 { "" } else { "s" }))
}

// ---------------- asking other arcades ----------------

/// A V1.0 arcade's answer to one question about a chain.
fn ask(node: &Node, addr: &str, q: Value) -> Result<(Value, Vec<u8>), String> {
    let mut link = Link::dial(addr, crate::meshsvc::NET, &node.identity, node.mesh_port).map_err(|e| e.to_string())?;
    link.set_read_timeout(Duration::from_secs(20)).map_err(|e| e.to_string())?;
    let mut q = q;
    q["t"] = json!("sh");
    q["op"] = json!("q");
    q["id"] = json!(1);
    link.send(&Frame::new(q)).map_err(|e| e.to_string())?;
    for _ in 0..200 {
        let f = link.recv().map_err(|e| e.to_string())?;
        if f.kind() == "ping" {
            let n = f.head["n"].clone();
            let _ = link.send(&Frame::new(json!({"t": "pong", "n": n})));
        } else if f.kind() == "sh" && f.head["op"] == "a" && f.head["id"] == 1 {
            if let Some(e) = f.head["err"].as_str() {
                return Err(e.to_string());
            }
            return Ok((f.head["result"].clone(), f.blob));
        }
    }
    Err("no answer".into())
}

fn addr_of(node: &Node, id: &str) -> Option<String> {
    node.offers.lock().unwrap().get(id).map(|o| o.addr.clone()).filter(|a| !a.is_empty())
}

/// Ask the arcades holding `unit` of `kind` until one answers.
pub fn ask_holders(node: &Node, p: &ChainParams, kind: &str, unit: u64, what: &str, arg: &str) -> Result<(Value, String), String> {
    let mut last = format!("no arcade on the mesh holds that part of {} right now", p.name);
    for id in holders(node, p.key, kind, unit) {
        let Some(addr) = addr_of(node, &id) else { continue };
        match ask(node, &addr, json!({"net": p.key, "what": what, "arg": arg})) {
            Ok((v, _)) => return Ok((v, id)),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Answer another arcade's question (only from what we hold).
pub fn answer(node: &Node, link: &mut Link, h: &Value) -> Result<(), LinkError> {
    let id = h["id"].clone();
    let res: Result<Value, String> = (|| {
        let p = node.chain_params(h["net"].as_str().unwrap_or("")).ok_or("chain not here")?;
        let arg = h["arg"].as_str().unwrap_or("");
        match h["what"].as_str().unwrap_or("") {
            "insc" => {
                let v = arcade_ledger::ord::inscription(&node.chain, p, arg).ok_or("not held here")?;
                Ok(v)
            }
            "tick" => crate::explorer::handle(node, "explorer.token", &json!({"chain": p.key, "tick": arg})),
            _ => Err("unknown question".to_string()),
        }
    })();
    match res {
        Ok(v) => link.send(&Frame::new(json!({"t": "sh", "op": "a", "id": id, "result": v}))),
        Err(e) => link.send(&Frame::new(json!({"t": "sh", "op": "a", "id": id, "err": e}))),
    }
}

/// Another arcade's hold announcement.
pub fn on_hold(node: &Node, from: &str, h: &Value) {
    let Some(c) = h["chains"].as_object() else { return };
    let mut clean = Map::new();
    for (k, v) in c.iter().take(32) {
        if arcade_chain::params::by_key(k).is_some() {
            clean.insert(k.clone(), json!({"mode": v["mode"], "h": v["h"], "blocks": v["blocks"], "ord": v["ord"], "tok": v["tok"]}));
        }
    }
    node.holds.lock().unwrap().insert(from.to_string(), (arcade_app::now(), Value::Object(clean)));
}
