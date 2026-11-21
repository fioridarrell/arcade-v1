//! Init installs in the chain thread (`arcade chain add <key> --init`, docs/INITSYNC.md): headers
//! agreed by several sources, then every block stored only (psync::run_store, any order), the
//! two repeated until nothing is new, then the bulk build (ChainDb::init_build). The normal sync
//! takes over at the end. Until then the chain is held: no balances (its tip is genesis: it
//! counts as far behind, so it is borrowed), not lent, not served over bk, no copies made.

use crate::chains::ChainRun;
use crate::node::Node;
use arcade_chaindb::initsync::Phase;
use arcade_chaindb::sync::{BlockSource, Syncer};
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// META: chains asked to install with --init, until their install starts (queued meanwhile).
const REQUESTS: &[u8] = b"init_requests";
/// Blocks the build leaves to the normal sync, which connects them with undo records (a reorg
/// at the tip needs them; under this depth the init chain can't reorg, as after a copy).
pub const TAIL: u32 = 288;
/// P2P peers asked in a headers round (besides the arcades offering the chain over bk).
const HEADER_PEERS: usize = 8;

static LOCK: Mutex<()> = Mutex::new(());
/// Chains whose build failed on something no retry fixes (a rule broken): left until a restart.
static FAILED: Mutex<Vec<u16>> = Mutex::new(Vec::new());

fn requests(node: &Node) -> Vec<String> {
    node.app.store.read(|r| r.get(arcade_app::META, REQUESTS)).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or_default()
}

fn save(node: &Node, v: &[String]) {
    let _ = node.app.store.write(|w| w.put(arcade_app::META, REQUESTS, &serde_json::to_vec(v).unwrap()));
}

/// chains.add with init: remember it until the install starts (now, or at its turn in the queue).
pub fn request(node: &Node, key: &str, on: bool) {
    let _g = LOCK.lock().unwrap();
    let mut v = requests(node);
    v.retain(|k| k != key);
    if on {
        v.push(key.to_string());
    }
    save(node, &v);
}

/// At the install: was it asked for with init? (Forgotten either way.)
pub fn take_request(node: &Node, key: &str) -> bool {
    let _g = LOCK.lock().unwrap();
    let mut v = requests(node);
    let had = v.iter().any(|k| k == key);
    v.retain(|k| k != key);
    if had {
        save(node, &v);
    }
    had
}

/// Can a chain be installed with init in `mode`? (Every block is kept and built from.)
pub fn check(mode: &str) -> Result<(), String> {
    if !matches!(mode, "full" | "chain") {
        return Err("an init install keeps every block: mode full or chain".into());
    }
    Ok(())
}

/// The init phases, until done (true) or the chain stops (false). Pausing waits here.
pub fn run(node: &Arc<Node>, run: &ChainRun, syncer: &Syncer) -> bool {
    let p = run.params;
    let c = p.chain_no;
    loop {
        if run.stop.load(Ordering::Relaxed) {
            return false;
        }
        if run.paused.load(Ordering::Relaxed) || FAILED.lock().unwrap().contains(&c) {
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        match node.chain.init_phase(c) {
            Ok(Some(Phase::Headers)) => headers(node, run, syncer),
            Ok(Some(Phase::Blocks)) => blocks(node, run),
            Ok(Some(Phase::Build)) => build(node, run),
            Ok(Some(Phase::Done)) | Ok(None) => return true,
            Err(e) => {
                set(run, format!("init: {e}"));
                std::thread::sleep(Duration::from_secs(10));
            }
        }
    }
}

fn set(run: &ChainRun, s: String) {
    *run.state.lock().unwrap() = s;
}

/// Sleep up to `secs`, waking for a stop.
fn rest(run: &ChainRun, secs: u64) {
    for _ in 0..secs * 5 {
        if run.stop.load(Ordering::Relaxed) {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The independence key of a P2P peer: its network group (/16 for IPv4, /32 for IPv6), so a
/// handful of addresses in one range count once.
pub fn group(addr: &std::net::SocketAddr) -> String {
    match addr.ip() {
        std::net::IpAddr::V4(a) => {
            let o = a.octets();
            format!("p2p {}.{}.0.0/16", o[0], o[1])
        }
        std::net::IpAddr::V6(a) => {
            let s = a.segments();
            format!("p2p {:x}:{:x}::/32", s[0], s[1])
        }
    }
}

/// Up to `want` P2P peers, one per network group, dialed several at a time.
fn dial_peers(node: &Node, p: &'static arcade_chain::params::ChainParams, want: usize) -> Vec<arcade_chain::p2p::Peer> {
    let cands: Vec<String> = node.coin_candidates(p).into_iter().take(want * 4).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    for a in cands {
        let tx = tx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(arcade_chain::p2p::Peer::connect(p, &a, Duration::from_secs(8)).ok());
        });
    }
    drop(tx);
    let mut out: Vec<arcade_chain::p2p::Peer> = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(12);
    while out.len() < want {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(Some(peer)) => {
                if !out.iter().any(|x| group(&x.addr) == group(&peer.addr)) {
                    node.note_coin_peer(p.net, &peer.addr.to_string());
                    out.push(peer);
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
    out
}

fn headers(node: &Arc<Node>, run: &ChainRun, syncer: &Syncer) {
    let p = run.params;
    let c = p.chain_no;
    let round = node.chain.init_round(c) + 1;
    set(run, format!("init · headers (round {round}): dialing sources"));
    let mut peers = dial_peers(node, p, HEADER_PEERS);
    for pr in &peers {
        run.peer_tip.fetch_max(pr.version.start_height.max(0) as u32, Ordering::Relaxed);
    }
    if let Some(first) = peers.first_mut() {
        if let Err(e) = syncer.ensure_genesis(first) {
            set(run, format!("init · headers: genesis: {e}"));
            rest(run, 10);
            return;
        }
    }
    // Arcades offering the chain over bk answer headers from their own checked chain. (Those
    // answering through a Core node send headers only with their blocks: no use here.)
    let mut mesh: Vec<(String, crate::meshsource::MeshSource)> = node
        .offers_for(p.key)
        .into_iter()
        .filter(|o| o.2 == "bk")
        .take(HEADER_PEERS)
        .filter_map(|(id, addr, _, _)| crate::meshsource::MeshSource::dial_chain(node, &addr, p).ok().map(|s| (format!("arcade {}", &id[..id.len().min(12)]), s)))
        .collect();
    if peers.is_empty() && mesh.is_empty() {
        set(run, format!("init · headers (round {round}): no {} peer or arcade reachable; retrying", p.name));
        rest(run, 20);
        return;
    }
    if node.chain.tip(c).ok().flatten().is_none() {
        if let Some((_, m)) = mesh.first_mut() {
            if let Err(e) = syncer.ensure_genesis(m) {
                set(run, format!("init · headers: genesis: {e}"));
                rest(run, 10);
                return;
            }
        }
    }
    *run.source.lock().unwrap() = format!("init: headers from {} P2P peers and {} arcades", peers.len(), mesh.len());
    set(run, format!("init · headers (round {round}): from {} sources, then asking each for its tip", peers.len() + mesh.len()));
    let mut v: Vec<(String, &mut dyn BlockSource)> = Vec::new();
    for pr in peers.iter_mut() {
        v.push((group(&pr.addr), pr as &mut dyn BlockSource));
    }
    for (k, m) in mesh.iter_mut() {
        v.push((k.clone(), m as &mut dyn BlockSource));
    }
    match node.chain.init_headers_round(syncer, &mut v, &run.stop) {
        Ok(a) if a.done() => {
            let missing = node.chain.init_missing(c).map(|m| m.len()).unwrap_or(1);
            let _ = node.chain.set_state_text(c, arcade_chaindb::initsync::INIT_ROUND, Some(&round.to_string()));
            let next = if missing == 0 { Phase::Build } else { Phase::Blocks };
            let _ = node.app.event("chains", &format!("{}: init headers round {round}: {} → {}{}", p.name, a.text(), next.as_str(),
                if missing > 0 { format!(" ({missing} blocks to fetch)") } else { String::new() }));
            let _ = node.chain.set_init_phase(c, next);
        }
        Ok(a) => {
            set(run, format!("init · headers (round {round}): waiting for agreement: {}", a.text()));
            rest(run, 30);
        }
        Err(e) => {
            set(run, format!("init · headers (round {round}): {e}"));
            rest(run, 10);
        }
    }
}

fn blocks(node: &Arc<Node>, run: &ChainRun) {
    let p = run.params;
    let c = p.chain_no;
    let round = node.chain.init_round(c).max(1);
    let missing = match node.chain.init_missing(c) {
        Ok(m) => m,
        Err(e) => {
            set(run, format!("init · blocks: {e}"));
            rest(run, 10);
            return;
        }
    };
    if missing.is_empty() {
        let _ = node.chain.set_init_phase(c, Phase::Headers);
        return;
    }
    let best = node.chain.best_header(c).ok().flatten().and_then(|h| node.chain.header(c, &h).ok().flatten()).map(|r| r.height).unwrap_or(0);
    // Sources near our best header (one far behind can't serve the newest blocks).
    let min_h = best.saturating_sub(10);
    let progress = |s: String| {
        *run.source.lock().unwrap() = format!("init, store only: {}", s.split(" · ").skip(2).collect::<Vec<_>>().join(" · "));
        set(run, format!("init · blocks (round {round}): {s}"));
    };
    let r = crate::psync::run_store(node, p, &missing, min_h, &progress, &run.stop);
    let left = node.chain.init_missing(c).map(|m| m.len()).unwrap_or(missing.len());
    match r {
        Ok(n) if left == 0 => {
            let _ = node.app.event("chains", &format!("{}: init blocks round {round}: {n} blocks stored; headers again", p.name));
            let _ = node.chain.set_init_phase(c, Phase::Headers);
        }
        Ok(_) => rest(run, 2),
        Err(e) => {
            let _ = node.app.event("chains", &format!("{}: init blocks round {round} stopped with {left} blocks left: {e}", p.name));
            set(run, format!("init · blocks (round {round}): {left} left · {e}; retrying"));
            rest(run, 10);
        }
    }
}

/// Bytes the coin derivation may hold in memory before it goes on on disk (ARCADE_INIT_COINS_MB,
/// default 4096). Bitcoin's coin set needs about 20 GB to stay in memory; on disk it needs
/// about 60 bytes per output and input of the whole history in the scratch folder instead.
fn coins_budget() -> usize {
    std::env::var("ARCADE_INIT_COINS_MB").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(4096) << 20
}

fn build(node: &Arc<Node>, run: &ChainRun) {
    let p = run.params;
    let c = p.chain_no;
    match node.chain.init_missing(c) {
        Ok(m) if !m.is_empty() && node.chain.init_built(c).is_empty() => {
            let _ = node.chain.set_init_phase(c, Phase::Blocks);
            return;
        }
        Err(e) => {
            set(run, format!("init · build: {e}"));
            rest(run, 10);
            return;
        }
        _ => {}
    }
    let tmp = node.home.join("initsync").join(p.key);
    let threads = arcade_chaindb::rules::script_threads();
    set(run, "init · build: starting".into());
    let t0 = std::time::Instant::now();
    let r = node.chain.init_build(p, TAIL, threads, coins_budget(), &tmp, &run.stop, &mut |s| set(run, format!("init · {s}")));
    match r {
        Ok(b) => {
            let _ = node.app.event("chains", &format!(
                "{}: init install built to block {} in {:.1} h: {} coins{}, {} transactions indexed, {} blocks' rules checked; the normal sync carries on",
                p.name, b.height, t0.elapsed().as_secs_f64() / 3600.0, b.coins, if b.on_disk { " (derived on disk)" } else { "" }, b.txs, b.rules_checked
            ));
        }
        Err(e) if run.stop.load(Ordering::Relaxed) => set(run, format!("init · build stopped ({e}); it carries on at the next start")),
        Err(e) => {
            let _ = node.app.event("chains", &format!("{}: init build failed: {e}", p.name));
            let _ = node.chain.set_state_text(c, arcade_chaindb::initsync::INIT_NOTE, Some(&format!("build failed: {e}")));
            set(run, format!("init · build failed: {e} (left as it is until the arcade restarts)"));
            FAILED.lock().unwrap().push(c);
        }
    }
}

/// Blocks stored of those on the best header chain above the tip (a walk down the headers:
/// cached for 30 s, status is asked often).
fn stored_of(node: &Node, c: u16) -> (u64, u64) {
    static CACHE: Mutex<Vec<(u16, std::time::Instant, u64, u64)>> = Mutex::new(Vec::new());
    let mut g = CACHE.lock().unwrap();
    if let Some(x) = g.iter().find(|x| x.0 == c && x.1.elapsed() < Duration::from_secs(30)) {
        return (x.2, x.3);
    }
    let path = node.chain.init_path(c).unwrap_or_default();
    let v = (path.iter().filter(|(_, b)| *b != 0).count() as u64, path.len() as u64);
    g.retain(|x| x.0 != c);
    g.push((c, std::time::Instant::now(), v.0, v.1));
    v
}

/// The init phases as status rows (chain status, the API): one per phase.
pub fn stages(node: &Node, c: u16, best: u64, peer: u64) -> Option<Vec<Value>> {
    let phase = node.chain.init_phase(c).ok().flatten()?;
    let order = |ph: Phase| match ph {
        Phase::Headers => 0,
        Phase::Blocks => 1,
        Phase::Build => 2,
        Phase::Done => 3,
    };
    let now = order(phase);
    let note = node.chain.state_text(c, arcade_chaindb::initsync::INIT_NOTE).ok().flatten().unwrap_or_default();
    let round = node.chain.init_round(c);
    let (stored, total) = if now == 1 { stored_of(node, c) } else { (0, 0) };
    let built = node.chain.init_built(c);
    let pct = |i: usize, live: f64| if now > i { 100.0 } else if now == i { live } else { 0.0 };
    let detail = |i: usize, live: String, done: &str, wait: &str| if now > i { done.to_string() } else if now == i { live } else { wait.to_string() };
    let hpct = if peer > 0 { (best as f64 * 100.0 / peer as f64).min(99.0) } else { 0.0 };
    let bpct = if total > 0 { stored as f64 * 100.0 / total as f64 } else { 0.0 };
    let steps = ["main", "txs", "coins", "rules"];
    let upct = built.iter().filter(|s| steps.contains(&s.as_str())).count() as f64 * 100.0 / steps.len() as f64;
    Some(vec![
        serde_json::json!({"name": "Init · headers", "detail": detail(0, format!("round {} · {}", round + 1, if note.is_empty() { "asking several sources".into() } else { note.clone() }), "agreed by at least 3 independent sources, enough work", "waiting"),
                           "pct": pct(0, hpct), "at": best, "of": peer}),
        serde_json::json!({"name": "Init · blocks", "detail": detail(1, format!("round {round} · stored only, any order, checked against their headers"), "every header's block stored", "waiting for the headers"),
                           "pct": pct(1, bpct), "at": stored, "of": total}),
        serde_json::json!({"name": "Init · build", "detail": detail(2, format!("{} · done: {}", note, if built.is_empty() { "nothing yet".into() } else { built.join(", ") }), "tx index, coin set and rules done; the normal sync carries on", "waiting for the blocks"),
                           "pct": pct(2, upct), "at": null}),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peers_count_once_per_network_group() {
        let a: std::net::SocketAddr = "203.0.113.7:8333".parse().unwrap();
        let b: std::net::SocketAddr = "203.0.9.9:8333".parse().unwrap();
        let x: std::net::SocketAddr = "198.51.100.7:8333".parse().unwrap();
        let v6: std::net::SocketAddr = "[2001:db8:1:2::5]:8333".parse().unwrap();
        assert_eq!(group(&a), group(&b));
        assert_ne!(group(&a), group(&x));
        assert_eq!(group(&v6), "p2p 2001:db8::/32");
        assert!(check("full").is_ok() && check("chain").is_ok());
        assert!(check("light").is_err() && check("shared").is_err());
    }
}
