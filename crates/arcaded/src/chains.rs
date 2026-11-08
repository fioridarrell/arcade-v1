//! Chains & Coins: install, follow, pause and remove extra chains by hand (Pepecoin testnet is
//! the built-in home chain). Each installed chain gets its own sync thread and its own choice
//! of indexes; the choice is kept in arcade.mdbx so it survives restarts.

use crate::node::Node;
use arcade_chain::p2p::Peer;
use arcade_chain::params::{by_key, ChainParams, ALL_CHAINS};
use arcade_chaindb::sync::Syncer;
use serde_json::{json, Value};
use std::net::ToSocketAddrs;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const MODES: &[&str] = &["light", "chain", "index", "full"];
pub const INDEXES: &[&str] = &["ledger", "ord", "names", "profiles"];

pub struct ChainRun {
    pub params: &'static ChainParams,
    pub mode: String,
    pub indexes: Vec<String>,
    pub state: Mutex<String>,
    pub peer_tip: AtomicU32,
    pub paused: AtomicBool,
    pub stop: AtomicBool,
    pub added: u64,
}

fn indexer_name(index: &str) -> &'static str {
    match index {
        "ledger" => "ledger.v1",
        "profiles" => "profiles.v1",
        _ => "",
    }
}

fn register(node: &Node, p: &'static ChainParams, indexes: &[String]) -> Result<(), String> {
    for ix in indexes {
        let r = match ix.as_str() {
            "ledger" => node.chain.add_indexer(p.chain_no, Arc::new(arcade_ledger::engine::LedgerIndexer)),
            "profiles" => node.chain.add_indexer(p.chain_no, Arc::new(arcade_ledger::index::ProfileIndexer)),
            _ => Ok(()),
        };
        r.map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn saved(node: &Node) -> Vec<Value> {
    node.app
        .store
        .read(|r| r.get(arcade_app::META, b"chains"))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or_default()
}

fn save(node: &Node) {
    let list: Vec<Value> = node
        .extra
        .lock()
        .unwrap()
        .values()
        .map(|r| json!({"key": r.params.key, "mode": r.mode, "indexes": r.indexes, "added": r.added, "paused": r.paused.load(Ordering::Relaxed)}))
        .collect();
    let _ = node.app.store.write(|w| w.put(arcade_app::META, b"chains", &serde_json::to_vec(&list).unwrap()));
}

/// Start every chain the user installed earlier.
pub fn start_saved(node: &Arc<Node>) {
    for c in saved(node) {
        let (Some(key), Some(mode)) = (c["key"].as_str(), c["mode"].as_str()) else { continue };
        let indexes: Vec<String> = c["indexes"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        if let Err(e) = launch(node, key, mode, indexes, c["added"].as_u64().unwrap_or(0), c["paused"] == true) {
            let _ = node.app.event("chains", &format!("could not start {key}: {e}"));
        }
    }
}

fn launch(node: &Arc<Node>, key: &str, mode: &str, indexes: Vec<String>, added: u64, paused: bool) -> Result<(), String> {
    let p = by_key(key).ok_or_else(|| format!("unknown chain {key}"))?;
    register(node, p, &indexes)?;
    let run = Arc::new(ChainRun {
        params: p,
        mode: mode.into(),
        indexes,
        state: Mutex::new("starting".into()),
        peer_tip: AtomicU32::new(0),
        paused: AtomicBool::new(paused),
        stop: AtomicBool::new(false),
        added,
    });
    node.extra.lock().unwrap().insert(key.into(), run.clone());
    let n = node.clone();
    std::thread::Builder::new().name(format!("chain {key}")).spawn(move || follow(n, run)).unwrap();
    Ok(())
}

/// Install a chain (by hand only).
pub fn add(node: &Arc<Node>, key: &str, mode: &str, indexes: Vec<String>) -> Result<Value, String> {
    let p = by_key(key).ok_or_else(|| format!("unknown chain {key}; see  arcade chain list"))?;
    if p.chain_no == node.params.chain_no {
        return Err(format!("{} is this arcade's home chain and is always installed", p.name));
    }
    if node.extra.lock().unwrap().contains_key(key) {
        return Err(format!("{} is already installed", p.name));
    }
    if !MODES.contains(&mode) {
        return Err(format!("mode must be one of {}", MODES.join(", ")));
    }
    if mode == "light" || mode == "index" {
        return Err("Light and Index-only storage (borrowing blocks from the mesh) are coming; use full or chain for now".into());
    }
    for ix in &indexes {
        if !INDEXES.contains(&ix.as_str()) {
            return Err(format!("unknown index {ix}; indexes: {}", INDEXES.join(", ")));
        }
        if ix == "ord" || ix == "names" {
            return Err(format!("the {ix} index is coming; install without it now and add it later with  arcade chain index add {key} {ix}"));
        }
    }
    if mode == "chain" && !indexes.is_empty() {
        return Err("Blockchain-only storage keeps no protocol indexes; choose full to add indexes".into());
    }
    launch(node, key, mode, indexes, arcade_app::now(), false)?;
    save(node);
    let _ = node.app.event("chains", &format!("installing {} ({mode})", p.name));
    Ok(status(node, key)?)
}

pub fn set_paused(node: &Node, key: &str, paused: bool) -> Result<Value, String> {
    let r = node.extra.lock().unwrap().get(key).cloned().ok_or_else(|| format!("{key} is not installed"))?;
    r.paused.store(paused, Ordering::Relaxed);
    save(node);
    status(node, key)
}

pub fn add_index(node: &Arc<Node>, key: &str, index: &str) -> Result<Value, String> {
    if key == node.params.key {
        // Home chain: register it again; the background catch-up rebuilds it from stored blocks.
        if !INDEXES.contains(&index) || index == "ord" || index == "names" {
            return Err(format!("the {index} index is not available yet"));
        }
        if node.chain.indexers_for(node.params.chain_no).contains(&indexer_name(index)) {
            return Err(format!("{index} is already on {key}"));
        }
        register(node, node.params, &[index.to_string()])?;
        return status(node, key);
    }
    let r = node.extra.lock().unwrap().get(key).cloned().ok_or_else(|| format!("{key} is not installed"))?;
    if r.mode == "chain" {
        return Err("Blockchain-only storage keeps no protocol indexes".into());
    }
    if !INDEXES.contains(&index) || index == "ord" || index == "names" {
        return Err(format!("the {index} index is not available yet"));
    }
    if r.indexes.iter().any(|i| i == index) {
        return Err(format!("{index} is already on {key}"));
    }
    let mut indexes = r.indexes.clone();
    indexes.push(index.into());
    // Restart the chain's follower with the new set (the index backfills from stored blocks).
    r.stop.store(true, Ordering::Relaxed);
    node.extra.lock().unwrap().remove(key);
    launch(node, key, &r.mode, indexes, r.added, r.paused.load(Ordering::Relaxed))?;
    save(node);
    status(node, key)
}

/// Remove one index from a chain: stop it and delete its rows (in the background).
pub fn remove_index(node: &Arc<Node>, key: &str, index: &str) -> Result<Value, String> {
    let p = by_key(key).ok_or_else(|| format!("unknown chain {key}"))?;
    let home = p.chain_no == node.params.chain_no;
    if home && index == "ledger" {
        return Err("the home chain's ledger holds your @handle and claims, so it stays".into());
    }
    let name = indexer_name(index);
    if name.is_empty() {
        return Err(format!("unknown index {index}"));
    }
    let tables: Vec<String> = match index {
        "ledger" => arcade_ledger::engine::LedgerIndexer.tables_named(),
        _ => vec![arcade_ledger::index::PROFILE.into(), arcade_ledger::index::LINKED.into()],
    };
    if !home {
        let r = node.extra.lock().unwrap().get(key).cloned().ok_or_else(|| format!("{key} is not installed"))?;
        if !r.indexes.iter().any(|i| i == index) {
            return Err(format!("{index} is not on {key}"));
        }
        let indexes: Vec<String> = r.indexes.iter().filter(|i| *i != index).cloned().collect();
        r.stop.store(true, Ordering::Relaxed);
        node.extra.lock().unwrap().remove(key);
        launch(node, key, &r.mode, indexes, r.added, r.paused.load(Ordering::Relaxed))?;
        save(node);
    }
    let n = node.clone();
    let c = p.chain_no;
    let label = format!("{index} index on {}", p.name);
    std::thread::spawn(move || {
        while n.chain.remove_index_batch(c, name, &tables, 20_000).map(|x| x > 0).unwrap_or(false) {}
        let _ = n.app.event("chains", &format!("removed the {label}"));
    });
    Ok(json!({"removed": index, "chain": key}))
}

/// Stop following a chain and delete its data (in the background).
pub fn remove(node: &Arc<Node>, key: &str) -> Result<Value, String> {
    let r = node.extra.lock().unwrap().remove(key).ok_or_else(|| format!("{key} is not installed"))?;
    r.stop.store(true, Ordering::Relaxed);
    for ix in node.chain.indexers_for(r.params.chain_no) {
        node.chain.remove_indexer(r.params.chain_no, ix);
    }
    save(node);
    let n = node.clone();
    let c = r.params.chain_no;
    let name = r.params.name;
    std::thread::spawn(move || {
        while n.chain.remove_chain_batch(c, 20_000).map(|x| x > 0).unwrap_or(false) {}
        let _ = n.app.event("chains", &format!("removed {name} and its data"));
    });
    Ok(json!({"removed": key}))
}

fn pct(done: u64, total: u64) -> f64 {
    if total == 0 { 0.0 } else { (done as f64 * 100.0 / total as f64).min(100.0) }
}

/// Staged progress, as the Chains & Coins tab and `arcade chain status` show it.
pub fn status(node: &Node, key: &str) -> Result<Value, String> {
    let p = by_key(key).ok_or_else(|| format!("unknown chain {key}"))?;
    let c = p.chain_no;
    let home = c == node.params.chain_no;
    let run = node.extra.lock().unwrap().get(key).cloned();
    if !home && run.is_none() {
        return Ok(json!({"key": key, "name": p.name, "installed": false}));
    }
    let tip = node.chain.tip(c).ok().flatten().map(|t| t.height as u64).unwrap_or(0);
    let best = node.chain.best_header(c).ok().flatten().and_then(|h| node.chain.header(c, &h).ok().flatten()).map(|r| r.height as u64).unwrap_or(0);
    let peer = run.as_ref().map(|r| r.peer_tip.load(Ordering::Relaxed) as u64).unwrap_or(best).max(best);
    let mut stages = vec![
        json!({"name": "Headers", "detail": "checked by proof of work", "pct": pct(best, peer), "at": best, "of": peer}),
        json!({"name": "Blocks", "detail": "stored once, from the mesh or P2P", "pct": pct(tip, peer.max(1)), "at": tip, "of": peer}),
        json!({"name": "Coins and addresses", "detail": "every address and balance", "pct": pct(tip, peer.max(1)), "at": tip, "of": peer}),
    ];
    let (mode, indexes): (String, Vec<String>) = match &run {
        Some(r) => (r.mode.clone(), r.indexes.clone()),
        None => (
            "full".into(),
            ["ledger", "profiles"].iter().filter(|i| node.chain.indexers_for(c).contains(&indexer_name(i))).map(|i| i.to_string()).collect(),
        ),
    };
    for ix in &indexes {
        let name = indexer_name(ix);
        let start = match ix.as_str() {
            "ledger" => arcade_ledger::engine::gates(p).activation as u64,
            _ => p.index_start as u64,
        };
        let at = node.chain.indexed_to(c, name).ok().flatten().map(|h| h as u64).unwrap_or(start.saturating_sub(1));
        let span = tip.saturating_sub(start) + 1;
        let label = match ix.as_str() {
            "ledger" => "Arcade ledger",
            "profiles" => "Profiles",
            _ => ix.as_str(),
        };
        stages.push(json!({"name": label, "detail": format!("from block {start}"), "pct": if tip < start { 0.0 } else { pct(at.saturating_sub(start) + 1, span) }, "at": at, "of": tip}));
    }
    let overall = stages.iter().map(|s| s["pct"].as_f64().unwrap_or(0.0)).sum::<f64>() / stages.len() as f64;
    let state = match &run {
        Some(r) => {
            if r.paused.load(Ordering::Relaxed) {
                "paused".to_string()
            } else {
                r.state.lock().unwrap().clone()
            }
        }
        None => node.sync_state.lock().unwrap().clone(),
    };
    Ok(json!({"key": key, "name": p.name, "installed": true, "home": home, "mode": mode, "indexes": indexes,
              "state": state, "overall": overall, "stages": stages, "tip": tip}))
}

pub fn list(node: &Node) -> Value {
    json!(ALL_CHAINS
        .iter()
        .map(|p| {
            let st = status(node, p.key).unwrap_or(json!({}));
            json!({"key": p.key, "name": p.name, "installed": st["installed"], "home": st["home"], "mode": st["mode"],
                   "indexes": st["indexes"], "overall": st["overall"], "state": st["state"],
                   "ord": p.chain_no != 5 && !p.key.starts_with("ltctest")})
        })
        .collect::<Vec<_>>())
}

fn candidates(p: &ChainParams) -> Vec<String> {
    p.fixed_peers
        .iter()
        .map(|s| s.to_string())
        .chain(p.dns_seeds.iter().flat_map(|d| (*d, p.p2p_port).to_socket_addrs().into_iter().flatten().map(|a| a.to_string())))
        .collect()
}

/// Follow one installed chain until it is stopped: headers, blocks (each batch one commit,
/// indexes in the same commit), backfill of any lagging index, then wait for new blocks.
fn follow(node: Arc<Node>, run: Arc<ChainRun>) {
    let p = run.params;
    let syncer = Syncer::new(&node.chain, p);
    let set = |s: String| *run.state.lock().unwrap() = s;
    'outer: while !run.stop.load(Ordering::Relaxed) {
        if run.paused.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        let Some(mut peer) = candidates(p).iter().find_map(|a| Peer::connect(p, a, Duration::from_secs(20)).ok()) else {
            set(format!("no {} peer reachable; retrying", p.name));
            std::thread::sleep(Duration::from_secs(20));
            continue;
        };
        run.peer_tip.store(peer.version.start_height.max(0) as u32, Ordering::Relaxed);
        if let Err(e) = syncer.ensure_genesis(&mut peer) {
            set(format!("genesis: {e}"));
            std::thread::sleep(Duration::from_secs(10));
            continue;
        }
        loop {
            if run.stop.load(Ordering::Relaxed) || run.paused.load(Ordering::Relaxed) {
                continue 'outer;
            }
            let h = match syncer.sync_headers_once(&mut peer) {
                Ok(n) => n,
                Err(e) => {
                    set(format!("headers: {e}"));
                    continue 'outer;
                }
            };
            let b = match syncer.sync_blocks(&mut peer, 2000, 250) {
                Ok(pr) => pr.blocks,
                Err(e) => {
                    set(format!("blocks: {e}"));
                    continue 'outer;
                }
            };
            for ix in &run.indexes {
                let name = indexer_name(ix);
                while node.chain.backfill(p, name, 2000).map(|x| x > 0).unwrap_or(false) {
                    if run.stop.load(Ordering::Relaxed) {
                        continue 'outer;
                    }
                }
            }
            let tip = node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height).unwrap_or(0);
            set(if h == 0 && b == 0 { format!("synced at {tip}") } else { format!("syncing {tip}") });
            if h == 0 && b == 0 {
                std::thread::sleep(Duration::from_secs(15));
            }
        }
    }
    set("stopped".into());
}
