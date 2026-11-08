//! Chains & Coins: install, follow, pause and remove extra chains by hand (Pepecoin testnet is
//! the built-in home chain). Each installed chain gets its own sync thread and its own choice
//! of indexes, every one opt-in; the choice is kept in arcade.mdbx so it survives restarts.
//!
//! Indexes: ledger, profiles, ord, and riding on ord (same commit, need ord): names, maps,
//! tokens (PRC-20 / DRC-20). Adding a rider to a chain whose ord index exists rebuilds ord
//! from its start; removing a rider only deletes that rider's rows.

use crate::node::Node;
use arcade_chain::p2p::Peer;
use arcade_chain::params::{by_key, ChainParams, ALL_CHAINS};
use arcade_chaindb::sync::Syncer;
use arcade_ledger::ord::{ord_start, OrdIndexer, RIDERS};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const MODES: &[&str] = &["light", "chain", "index", "full"];
pub const INDEXES: &[&str] = &["ledger", "profiles", "ord", "names", "maps", "tokens", "runes", "dunes"];
pub const COMING: &[&str] = &["runes", "dunes"];
const HOME_DEFAULT: &[&str] = &["ledger", "profiles"];

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

pub fn indexer_name(index: &str) -> &'static str {
    match index {
        "ledger" => "ledger.v1",
        "profiles" => "profiles.v1",
        "ord" | "names" | "maps" | "tokens" => "ord.v1",
        _ => "",
    }
}

fn is_rider(ix: &str) -> bool {
    RIDERS.contains(&ix)
}

/// Rows an index owns.
fn tables_of(index: &str) -> Vec<String> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    match index {
        "ledger" => arcade_ledger::engine::LedgerIndexer.tables_named(),
        "profiles" => s(&[arcade_ledger::index::PROFILE, arcade_ledger::index::LINKED]),
        "ord" => s(arcade_ledger::ord::TABLES),
        "names" => s(&[arcade_ledger::names::CLAIM, arcade_ledger::names::WIN]),
        "maps" => s(&[arcade_ledger::maps::MAP]),
        "tokens" => s(&[arcade_ledger::tokens::TICK, arcade_ledger::tokens::BAL, arcade_ledger::tokens::HOLDER, arcade_ledger::tokens::XFER]),
        _ => vec![],
    }
}

/// Check a requested set of indexes for a chain; riders bring ord with them.
fn normalize(p: &ChainParams, indexes: &[String]) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for ix in indexes {
        if !INDEXES.contains(&ix.as_str()) {
            return Err(format!("unknown index {ix}; indexes: {}", INDEXES.join(", ")));
        }
        // Runes live on Bitcoin and dunes on Dogecoin only.
        if ix == "runes" && p.key != "btc" {
            return Err("runes are a Bitcoin protocol; they aren't on ".to_string() + p.name);
        }
        if ix == "dunes" && p.key != "doge" {
            return Err("dunes are a Dogecoin protocol; they aren't on ".to_string() + p.name);
        }
        if COMING.contains(&ix.as_str()) {
            return Err(format!("the {ix} index is coming; it is not available yet"));
        }
        if (ix == "ord" || is_rider(ix)) && ord_start(p).is_none() {
            return Err(format!("the {ix} index is not available on {} yet", p.name));
        }
        if is_rider(ix) && !indexes.iter().any(|i| i == "ord") && !out.iter().any(|i| i == "ord") {
            out.push("ord".into());
        }
        if !out.contains(ix) {
            out.push(ix.clone());
        }
    }
    Ok(out)
}

fn register(node: &Node, p: &'static ChainParams, indexes: &[String]) -> Result<(), String> {
    let c = p.chain_no;
    for ix in indexes {
        let r = match ix.as_str() {
            "ledger" => node.chain.add_indexer(c, Arc::new(arcade_ledger::engine::LedgerIndexer)),
            "profiles" => node.chain.add_indexer(c, Arc::new(arcade_ledger::index::ProfileIndexer)),
            "ord" => {
                let riders: Vec<&str> = indexes.iter().map(|s| s.as_str()).filter(|i| is_rider(i)).collect();
                node.chain.remove_indexer(c, "ord.v1");
                node.chain.add_indexer(c, Arc::new(OrdIndexer::new(&riders)))
            }
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

/// The home chain's indexes (ledger and profiles unless changed by hand).
pub fn home_indexes(node: &Node) -> Vec<String> {
    node.app
        .store
        .read(|r| r.get(arcade_app::META, b"home_indexes"))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_slice(&v).ok())
        .unwrap_or_else(|| HOME_DEFAULT.iter().map(|s| s.to_string()).collect())
}

fn save_home(node: &Node, ix: &[String]) {
    let _ = node.app.store.write(|w| w.put(arcade_app::META, b"home_indexes", &serde_json::to_vec(ix).unwrap()));
}

/// Register the home chain's indexes (at start).
pub fn start_home(node: &Node) -> Result<(), String> {
    register(node, node.params, &home_indexes(node))
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

/// Install a chain (by hand only), with the indexes the user opted into.
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
    let indexes = normalize(p, &indexes)?;
    if mode == "chain" && !indexes.is_empty() {
        return Err("Blockchain-only storage keeps no protocol indexes; choose full to add indexes".into());
    }
    launch(node, key, mode, indexes, arcade_app::now(), false)?;
    save(node);
    let _ = node.app.event("chains", &format!("installing {} ({mode})", p.name));
    status(node, key)
}

pub fn set_paused(node: &Node, key: &str, paused: bool) -> Result<Value, String> {
    let r = node.extra.lock().unwrap().get(key).cloned().ok_or_else(|| format!("{key} is not installed"))?;
    r.paused.store(paused, Ordering::Relaxed);
    save(node);
    status(node, key)
}

/// Current indexes of an installed chain (home included).
fn current(node: &Node, key: &str) -> Result<(bool, Vec<String>, Option<Arc<ChainRun>>), String> {
    if key == node.params.key {
        return Ok((true, home_indexes(node), None));
    }
    let r = node.extra.lock().unwrap().get(key).cloned().ok_or_else(|| format!("{key} is not installed"))?;
    Ok((false, r.indexes.clone(), Some(r)))
}

/// Put a new index set in force: re-register (home) or restart the follower (extra chain).
fn apply_set(node: &Arc<Node>, key: &str, home: bool, run: Option<Arc<ChainRun>>, set: Vec<String>) -> Result<(), String> {
    let p = by_key(key).ok_or_else(|| format!("unknown chain {key}"))?;
    if home {
        register(node, p, &set)?;
        save_home(node, &set);
        return Ok(());
    }
    let r = run.ok_or("not installed")?;
    r.stop.store(true, Ordering::Relaxed);
    node.extra.lock().unwrap().remove(key);
    launch(node, key, &r.mode, set, r.added, r.paused.load(Ordering::Relaxed))?;
    save(node);
    Ok(())
}

pub fn add_index(node: &Arc<Node>, key: &str, index: &str) -> Result<Value, String> {
    let p = by_key(key).ok_or_else(|| format!("unknown chain {key}"))?;
    let (home, cur, run) = current(node, key)?;
    if run.as_ref().is_some_and(|r| r.mode == "chain") {
        return Err("Blockchain-only storage keeps no protocol indexes".into());
    }
    if cur.iter().any(|i| i == index) {
        return Err(format!("{index} is already on {key}"));
    }
    let mut want = cur.clone();
    want.push(index.into());
    let set = normalize(p, &want)?;
    let rebuild_ord = is_rider(index) && cur.iter().any(|i| i == "ord");
    if !rebuild_ord {
        apply_set(node, key, home, run, set)?;
        return status(node, key);
    }
    // A rider needs every inscription from the start: drop the ord rows, then rebuild with it.
    if let Some(r) = &run {
        r.stop.store(true, Ordering::Relaxed);
    }
    node.chain.remove_indexer(p.chain_no, "ord.v1");
    let n = node.clone();
    let key = key.to_string();
    let reply = json!({"adding": index, "chain": key, "rebuilding": "ord"});
    let label = format!("{index} index on {}", p.name);
    let mut tables = tables_of("ord");
    for rd in RIDERS {
        tables.extend(tables_of(rd));
    }
    std::thread::spawn(move || {
        while n.chain.remove_index_batch(p.chain_no, "ord.v1", &tables, 20_000).map(|x| x > 0).unwrap_or(false) {}
        match apply_set(&n, &key, home, run, set) {
            Ok(()) => { let _ = n.app.event("chains", &format!("adding the {label}: rebuilding the ord index from block {}", ord_start(p).unwrap_or(0))); }
            Err(e) => { let _ = n.app.event("chains", &format!("could not add the {label}: {e}")); }
        }
    });
    Ok(reply)
}

/// Remove one index from a chain: stop it and delete its rows (in the background). Removing
/// ord removes its riders too.
pub fn remove_index(node: &Arc<Node>, key: &str, index: &str) -> Result<Value, String> {
    let p = by_key(key).ok_or_else(|| format!("unknown chain {key}"))?;
    let (home, cur, run) = current(node, key)?;
    if home && index == "ledger" {
        return Err("the home chain's ledger holds your @handle and claims, so it stays".into());
    }
    if indexer_name(index).is_empty() {
        return Err(format!("unknown index {index}"));
    }
    if !cur.iter().any(|i| i == index) {
        return Err(format!("{index} is not on {key}"));
    }
    let c = p.chain_no;
    let gone: Vec<String> = if index == "ord" {
        cur.iter().filter(|i| *i == "ord" || is_rider(i)).cloned().collect()
    } else {
        vec![index.to_string()]
    };
    let set: Vec<String> = cur.iter().filter(|i| !gone.contains(i)).cloned().collect();
    let tables: Vec<String> = gone.iter().flat_map(|g| tables_of(g)).collect();
    let rider = is_rider(index);
    let name = indexer_name(index);
    if !rider {
        node.chain.remove_indexer(c, name);
    }
    apply_set(node, key, home, run, set)?;
    let n = node.clone();
    let label = format!("{index} index on {}", p.name);
    std::thread::spawn(move || {
        if rider {
            while n.chain.clear_rows_batch(c, &tables, 20_000).map(|x| x > 0).unwrap_or(false) {}
        } else {
            while n.chain.remove_index_batch(c, name, &tables, 20_000).map(|x| x > 0).unwrap_or(false) {}
        }
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

fn label(p: &ChainParams, ix: &str) -> String {
    match ix {
        "ledger" => "Arcade ledger".into(),
        "profiles" => "Profiles".into(),
        "ord" => "Ord index".into(),
        "names" => "Names".into(),
        "maps" => "Maps".into(),
        "tokens" => format!("{} tokens", arcade_ledger::tokens::protocol(p).0.to_uppercase()),
        _ => ix.into(),
    }
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
        None => ("full".into(), home_indexes(node)),
    };
    for ix in &indexes {
        let name = indexer_name(ix);
        let start = match ix.as_str() {
            "ledger" => arcade_ledger::engine::gates(p).activation as u64,
            "profiles" => p.index_start as u64,
            _ => ord_start(p).unwrap_or(0) as u64,
        };
        let running = node.chain.indexers_for(c).contains(&name);
        let at = node.chain.indexed_to(c, name).ok().flatten().map(|h| h as u64).unwrap_or(start.saturating_sub(1));
        let span = tip.saturating_sub(start) + 1;
        let detail = if !running { "waiting to start (rebuilding)".to_string() } else if is_rider(ix) { format!("from block {start}, with the ord index") } else { format!("from block {start}") };
        stages.push(json!({"name": label(p, ix), "index": ix, "detail": detail,
                           "pct": if tip < start || !running { 0.0 } else { pct(at.saturating_sub(start) + 1, span) }, "at": at, "of": tip}));
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
                   "ord": ord_start(p).is_some(), "tokens": arcade_ledger::tokens::protocol(p).0.to_uppercase(),
                   "dunes": p.key == "doge", "runes": p.key == "btc", "sizes": crate::sizes::for_chain(node, p.key)})
        })
        .collect::<Vec<_>>())
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
        let Some(mut peer) = node.coin_candidates(p).iter().find_map(|a| Peer::connect(p, a, Duration::from_secs(20)).ok()) else {
            set(format!("no {} peer reachable; retrying", p.name));
            std::thread::sleep(Duration::from_secs(20));
            continue;
        };
        run.peer_tip.store(peer.version.start_height.max(0) as u32, Ordering::Relaxed);
        node.note_coin_peer(p.key, &peer.addr.to_string());
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
            let mut names: Vec<&str> = run.indexes.iter().map(|i| indexer_name(i)).collect();
            names.dedup();
            for name in names {
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
