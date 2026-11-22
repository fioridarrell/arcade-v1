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
/// Chains whose init phases are running in their chain thread (they write the chain's store
/// directly: chains.remove waits for them to leave before it deletes the chain).
static RUNNING: Mutex<Vec<u16>> = Mutex::new(Vec::new());

/// Whether chain `c`'s init phases are still running (chains.remove waits on this).
pub fn running(c: u16) -> bool {
    RUNNING.lock().unwrap().contains(&c)
}

struct Running(u16);

impl Running {
    fn enter(c: u16) -> Running {
        RUNNING.lock().unwrap().push(c);
        Running(c)
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let mut v = RUNNING.lock().unwrap();
        if let Some(i) = v.iter().position(|x| *x == self.0) {
            v.swap_remove(i);
        }
    }
}

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

/// Can a chain be installed with init in `mode`? Every block is kept and built from (mode full
/// or chain). Not on a proof-of-stake chain: its stake proofs are checked only when a block is
/// connected, and an init install never connects the blocks it builds. Only on a chain with a
/// minimum chain work: the sources that agree on the headers can be few and could be one party,
/// so the headers must also carry real work no one can make up.
pub fn check(p: &arcade_chain::params::ChainParams, mode: &str) -> Result<(), String> {
    if !matches!(mode, "full" | "chain") {
        return Err("an init install keeps every block: mode full or chain".into());
    }
    if matches!(p.pow, arcade_chain::pow::PowRule::PowThenStake { .. }) || p.value_rules.is_some_and(|v| v.stake_reward.is_some()) {
        return Err(format!("{} has proof-of-stake blocks: an init install can't check stake (install it without --init)", p.name));
    }
    // Litecoin's MWEB: the extension block after a block's transactions is stored and served
    // as part of the block, but nothing here checks it. Its header could be checked against
    // the HogEx commitment, but not its body: the body's roots are MMR roots over every MWEB
    // output and kernel ever made, which takes the whole MWEB state. So no init install, which
    // takes blocks from sources it hasn't checked over, until it is checked.
    if p.format.mweb {
        return Err(format!(
            "{} carries MWEB extension blocks, which this release can't check yet (their roots need the whole MWEB state): install it without --init",
            p.name
        ));
    }
    // The trust floor: a release checkpoint taken from two independent sources. Where none could
    // be checked (no chain data of our own, or no second source), the chain stays refused.
    if p.init_floor().is_none() {
        return Err(format!(
            "{} has no init checkpoint in this release (a block hash checked against two independent sources): install it without --init",
            p.name
        ));
    }
    Ok(())
}

/// The init phases, until done (true) or the chain stops (false). Pausing waits here.
pub fn run(node: &Arc<Node>, run: &ChainRun, syncer: &Syncer) -> bool {
    let _running = Running::enter(run.params.chain_no);
    run_with(&node.chain, run.params, &mut NodeEnv { node, run, syncer })
}

/// What the phase loop needs from the world: the arcade (P2P peers, the mesh, psync, the chain's
/// status line and journal) or, in the tests, a few made-up sources.
pub(crate) trait Env {
    fn stopped(&self) -> bool;
    fn paused(&self) -> bool {
        false
    }
    /// The chain's status line.
    fn status(&self, s: String);
    /// A line in the journal (events).
    fn event(&self, s: String);
    /// Wait up to `secs` (waking for a stop).
    fn rest(&self, secs: u64);
    /// ARCADE_INIT_UPTO (testing only): store blocks up to this height, then hold; no build.
    /// An error: the value doesn't parse (the install stops, never runs uncapped).
    fn upto(&self) -> Result<Option<u32>, String>;
    /// Room on the chain's drive for the next batches (an error: hold, and why).
    fn room(&self) -> Result<(), String> {
        Ok(())
    }
    /// One headers round over the sources reachable now.
    fn headers_round(&mut self, round: u32) -> Result<arcade_chaindb::initsync::Agreement, String>;
    /// Store `missing` (store only, any order) from every source.
    fn store(&mut self, round: u32, missing: &[arcade_chain::Hash256]) -> crate::psync::Round;
    /// The build; Ok(None): not started (waiting, e.g. for scratch space).
    fn build(&mut self) -> arcade_store::Result<Option<arcade_chaindb::initsync::Built>>;
}

/// [`run`] over any [`Env`]: the phase loop itself (STATE "init" says where it is).
pub(crate) fn run_with(db: &arcade_chaindb::ChainDb, p: &'static arcade_chain::params::ChainParams, env: &mut dyn Env) -> bool {
    let c = p.chain_no;
    let upto = match env.upto() {
        Ok(u) => u,
        Err(e) => {
            env.event(format!("{}: init stopped: {e}", p.name));
            loop {
                env.status(format!("init · stopped: {e}"));
                if env.stopped() {
                    return false;
                }
                env.rest(30);
            }
        }
    };
    if let Some(u) = upto {
        env.event(format!("{}: init: storing up to {u} (ARCADE_INIT_UPTO, testing only: no build)", p.name));
        env.status(format!("init: storing up to {u} (ARCADE_INIT_UPTO, testing only)"));
    }
    loop {
        if env.stopped() {
            return false;
        }
        if env.paused() || FAILED.lock().unwrap().contains(&c) {
            env.rest(2);
            continue;
        }
        let phase = db.init_phase(c);
        // A smoke test's stopping height reached: every block up to it stored; nothing built.
        if let (Some(u), Ok(Some(Phase::Headers | Phase::Blocks))) = (upto, &phase) {
            if db.init_round(c) >= 1 && db.init_missing_upto(c, Some(u)).is_ok_and(|m| m.is_empty()) {
                env.status(format!("init · stopped at block {u} (ARCADE_INIT_UPTO, testing only): every block up to it stored; no build. Timing: {}",
                    db.init_timing(c).last().cloned().unwrap_or_default()));
                env.rest(60);
                continue;
            }
        }
        match phase {
            Ok(Some(Phase::Headers)) => headers_step(db, p, env, upto),
            Ok(Some(Phase::Blocks)) => blocks_step(db, p, env, upto),
            Ok(Some(Phase::Build)) => build_step(db, p, env),
            Ok(Some(Phase::Done)) | Ok(None) => return true,
            Err(e) => {
                env.status(format!("init: {e}"));
                env.rest(10);
            }
        }
    }
}

/// A timing line: kept with the chain (STATE, shown in its status) and in the journal.
fn timing(db: &arcade_chaindb::ChainDb, p: &arcade_chain::params::ChainParams, env: &dyn Env, line: String) {
    let _ = db.init_add_timing(p.chain_no, &line);
    env.event(format!("{}: init timing: {line}", p.name));
}

fn headers_step(db: &arcade_chaindb::ChainDb, p: &'static arcade_chain::params::ChainParams, env: &mut dyn Env, upto: Option<u32>) {
    use arcade_chaindb::initsync::{per_s, took, INIT_ROUND};
    let c = p.chain_no;
    let round = db.init_round(c) + 1;
    let t0 = std::time::Instant::now();
    match env.headers_round(round) {
        Ok(a) if a.done() => {
            let missing = db.init_missing_upto(c, upto).map(|m| m.len()).unwrap_or(1);
            let _ = db.set_state_text(c, INIT_ROUND, Some(&round.to_string()));
            timing(db, p, env, format!("headers round {round}: {} new headers to {} in {} · {} headers/s", a.new_headers, a.height, took(t0.elapsed()), per_s(a.new_headers as u64, t0.elapsed())));
            if missing == 0 && upto.is_some() {
                // ARCADE_INIT_UPTO: held at the top of the loop, never built.
                return;
            }
            let next = if missing == 0 { Phase::Build } else { Phase::Blocks };
            env.event(format!("{}: init headers round {round}: {} → {}{}", p.name, a.text(), next.as_str(),
                if missing > 0 { format!(" ({missing} blocks to fetch)") } else { String::new() }));
            let _ = db.set_init_phase(c, next);
        }
        Ok(a) => {
            env.status(format!("init · headers (round {round}): waiting for agreement: {}", a.text()));
            env.rest(30);
        }
        Err(e) => {
            env.status(format!("init · headers (round {round}): {e}"));
            env.rest(10);
        }
    }
}

fn blocks_step(db: &arcade_chaindb::ChainDb, p: &'static arcade_chain::params::ChainParams, env: &mut dyn Env, upto: Option<u32>) {
    use arcade_chaindb::initsync::{per_s, took};
    let c = p.chain_no;
    let round = db.init_round(c).max(1);
    let missing = match db.init_missing_upto(c, upto) {
        Ok(m) => m,
        Err(e) => {
            env.status(format!("init · blocks: {e}"));
            env.rest(10);
            return;
        }
    };
    if missing.is_empty() {
        let _ = db.set_init_phase(c, Phase::Headers);
        return;
    }
    // Room on the chain's drive first: held (and said so) below the floor.
    if let Err(e) = env.room() {
        env.status(e);
        env.rest(60);
        return;
    }
    let t0 = std::time::Instant::now();
    let r = env.store(round, &missing);
    let d = t0.elapsed();
    if r.stored > 0 {
        timing(db, p, env, format!("blocks round {round}: {} blocks, {:.1} MB stored in {} · {} blocks/s · {:.1} MB/s", r.stored, r.bytes as f64 / 1e6, took(d), per_s(r.stored, d),
            r.bytes as f64 / 1e6 / d.as_secs_f64().max(0.001)));
    }
    let left = db.init_missing_upto(c, upto).map(|m| m.len()).unwrap_or(missing.len());
    match r.error {
        None if left == 0 => {
            env.event(format!("{}: init blocks round {round}: {} blocks stored; headers again", p.name, r.stored));
            let _ = db.set_init_phase(c, Phase::Headers);
        }
        None => env.rest(2),
        Some(e) => {
            env.event(format!("{}: init blocks round {round} stopped with {left} blocks left: {e}", p.name));
            env.status(format!("init · blocks (round {round}): {left} left · {e}; retrying"));
            env.rest(10);
        }
    }
}

fn build_step(db: &arcade_chaindb::ChainDb, p: &'static arcade_chain::params::ChainParams, env: &mut dyn Env) {
    use arcade_chaindb::initsync::took;
    let c = p.chain_no;
    match db.init_missing(c) {
        Ok(m) if !m.is_empty() && db.init_built(c).is_empty() => {
            let _ = db.set_init_phase(c, Phase::Blocks);
            return;
        }
        Err(e) => {
            env.status(format!("init · build: {e}"));
            env.rest(10);
            return;
        }
        _ => {}
    }
    let t0 = std::time::Instant::now();
    match env.build() {
        Ok(None) => {}
        Ok(Some(b)) => {
            for line in &b.timing {
                env.event(format!("{}: init timing: {line}", p.name));
            }
            if b.refetch > 0 {
                env.event(format!("{}: init build: the rules refused {} blocks; dropped and fetched again (a source may have sent damaged copies)", p.name, b.refetch));
            } else {
                let line = format!("build: to block {} in {}: {} coins{}, {} transactions indexed, {} blocks' rules checked", b.height, took(t0.elapsed()), b.coins,
                    if b.on_disk { " (derived on disk)" } else { "" }, b.txs, b.rules_checked);
                timing(db, p, env, line.clone());
                env.event(format!("{}: init install built ({line}); the normal sync carries on", p.name));
            }
        }
        Err(e) if env.stopped() => env.status(format!("init · build stopped ({e}); it carries on at the next start")),
        // The chain breaks the rules (even fetched again) or spends coins it never made: no retry fixes that.
        Err(e @ arcade_store::StoreError::InvalidBlock { .. }) => {
            env.event(format!("{}: init build failed: {e}", p.name));
            let _ = db.set_state_text(c, arcade_chaindb::initsync::INIT_NOTE, Some(&format!("build failed: {e}")));
            env.status(format!("init · build failed: {e} (left as it is until the arcade restarts)"));
            FAILED.lock().unwrap().push(c);
        }
        // Anything else (the disk, a read) may pass on a later try.
        Err(e) => {
            env.event(format!("{}: init build stopped: {e}; trying again in 10 minutes", p.name));
            let _ = db.set_state_text(c, arcade_chaindb::initsync::INIT_NOTE, Some(&format!("build stopped: {e}; trying again")));
            env.status(format!("init · build stopped: {e}; trying again in 10 minutes"));
            env.rest(600);
        }
    }
}

/// ARCADE_INIT_UPTO's value: unset = no cap; anything but a block height over 0 is an error.
fn parse_upto(v: Option<&str>) -> Result<Option<u32>, String> {
    match v {
        None => Ok(None),
        Some(s) => match s.trim().parse::<u32>() {
            Ok(h) if h > 0 => Ok(Some(h)),
            _ => Err(format!("ARCADE_INIT_UPTO={s:?} is not a block height (a whole number over 0): the init install stops here; unset it or give a height")),
        },
    }
}

/// Free space kept on the chain's drive while blocks are stored (ARCADE_INIT_FREE_GB, default 50).
const FREE_GB: u64 = 50;
/// Room the next batches take on top: 24 P2P peers and the arcades, 250 blocks a job, ~2 MB a block.
const NEXT_BATCHES: u64 = 12 << 30;

fn parse_free_gb(v: Option<&str>) -> Result<u64, String> {
    match v {
        None => Ok(FREE_GB << 30),
        Some(s) => s.trim().parse::<u64>().map(|g| g << 30).map_err(|_| format!("init · blocks held: ARCADE_INIT_FREE_GB={s:?} is not a number of GB")),
    }
}

/// Whether the chain's drive (`free` bytes) still has the floor plus the next batches' room.
fn room_left(free: Option<u64>, floor: u64, dir: &std::path::Path) -> Result<(), String> {
    match free {
        Some(f) if f < floor + NEXT_BATCHES => Err(format!(
            "init · blocks held: {} GB free on the chain's drive ({}), under the floor of {} GB plus {} GB for the next batches (ARCADE_INIT_FREE_GB); free some space, it carries on by itself",
            f >> 30, dir.display(), floor >> 30, NEXT_BATCHES >> 30)),
        _ => Ok(()),
    }
}

/// The arcade's side of [`Env`].
struct NodeEnv<'a, 'b> {
    node: &'a Arc<Node>,
    run: &'a ChainRun,
    syncer: &'a Syncer<'b>,
}

impl Env for NodeEnv<'_, '_> {
    fn stopped(&self) -> bool {
        self.run.stop.load(Ordering::Relaxed)
    }
    fn paused(&self) -> bool {
        self.run.paused.load(Ordering::Relaxed)
    }
    fn status(&self, s: String) {
        set(self.run, s);
    }
    fn event(&self, s: String) {
        let _ = self.node.app.event("chains", &s);
    }
    fn rest(&self, secs: u64) {
        rest(self.run, secs);
    }
    fn upto(&self) -> Result<Option<u32>, String> {
        parse_upto(std::env::var("ARCADE_INIT_UPTO").ok().as_deref())
    }
    fn room(&self) -> Result<(), String> {
        let dir = self.node.chain.own_dir(self.run.params.chain_no);
        let floor = parse_free_gb(std::env::var("ARCADE_INIT_FREE_GB").ok().as_deref())?;
        room_left(crate::chaindrive::free_bytes(&dir).map(|f| f.0), floor, &dir)
    }
    fn headers_round(&mut self, round: u32) -> Result<arcade_chaindb::initsync::Agreement, String> {
        headers_round(self.node, self.run, self.syncer, round)
    }
    fn store(&mut self, round: u32, missing: &[arcade_chain::Hash256]) -> crate::psync::Round {
        let (node, run, p) = (self.node, self.run, self.run.params);
        let best = node.chain.best_header(p.chain_no).ok().flatten().and_then(|h| node.chain.header(p.chain_no, &h).ok().flatten()).map(|r| r.height).unwrap_or(0);
        // Sources near our best header (one far behind can't serve the newest blocks); with
        // ARCADE_INIT_UPTO, near that height.
        let min_h = self.upto().ok().flatten().unwrap_or(best).min(best).saturating_sub(10);
        let progress = |s: String| {
            *run.source.lock().unwrap() = format!("init, store only: {}", s.split(" · ").skip(2).collect::<Vec<_>>().join(" · "));
            set(run, format!("init · blocks (round {round}): {s}"));
        };
        crate::psync::run_store(node, p, missing, min_h, &progress, &|| self.room(), &run.stop)
    }
    fn build(&mut self) -> arcade_store::Result<Option<arcade_chaindb::initsync::Built>> {
        build_here(self.node, self.run)
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

/// The independence key of a source: its network group (/16 for IPv4, /32 for IPv6), the same
/// for a P2P peer and an arcade, so a handful of addresses in one range, or one host offering
/// the chain both over P2P and as an arcade (under any number of arcade ids), count once.
pub fn group(addr: &std::net::SocketAddr) -> String {
    match addr.ip() {
        std::net::IpAddr::V4(a) => {
            let o = a.octets();
            format!("net {}.{}.0.0/16", o[0], o[1])
        }
        std::net::IpAddr::V6(a) => match a.to_ipv4_mapped() {
            Some(v4) => group(&std::net::SocketAddr::new(v4.into(), addr.port())),
            None => {
                let s = a.segments();
                format!("net {:x}:{:x}::/32", s[0], s[1])
            }
        },
    }
}

/// [`group`] of an arcade's dial address ("host:port", or a ws(s):// URL): its host resolved.
/// One that doesn't resolve is keyed by its name.
pub fn addr_group(addr: &str) -> String {
    let rest = addr.split_once("://").map(|x| x.1).unwrap_or(addr);
    let hostport = rest.split('/').next().unwrap_or(rest);
    if let Ok(sa) = hostport.parse::<std::net::SocketAddr>() {
        return group(&sa);
    }
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) if p.parse::<u16>().is_ok() => (h.trim_matches(|c| c == '[' || c == ']'), p.parse::<u16>().unwrap_or(443)),
        _ => (hostport, 443),
    };
    use std::net::ToSocketAddrs;
    match (host, port).to_socket_addrs().ok().and_then(|mut a| a.next()) {
        Some(sa) => group(&sa),
        None => format!("host {}", host.to_ascii_lowercase()),
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

/// One headers round from the P2P peers and arcades reachable now (headers_step decides).
fn headers_round(node: &Arc<Node>, run: &ChainRun, syncer: &Syncer, round: u32) -> Result<arcade_chaindb::initsync::Agreement, String> {
    let p = run.params;
    let c = p.chain_no;
    set(run, format!("init · headers (round {round}): dialing sources"));
    let mut peers = dial_peers(node, p, HEADER_PEERS);
    for pr in &peers {
        run.peer_tip.fetch_max(pr.version.start_height.max(0) as u32, Ordering::Relaxed);
    }
    if let Some(first) = peers.first_mut() {
        syncer.ensure_genesis(first).map_err(|e| format!("genesis: {e}"))?;
    }
    // Arcades offering the chain over bk answer headers from their own checked chain. (Those
    // answering through a Core node send headers only with their blocks: no use here.)
    let mut mesh: Vec<(String, crate::meshsource::MeshSource)> = node
        .offers_for(p.key)
        .into_iter()
        .filter(|o| o.2 == "bk")
        .take(HEADER_PEERS)
        .filter_map(|(_, addr, _, _)| crate::meshsource::MeshSource::dial_chain(node, &addr, p).ok().map(|s| (addr_group(&addr), s)))
        .collect();
    if peers.is_empty() && mesh.is_empty() {
        return Err(format!("no {} peer or arcade reachable; retrying", p.name));
    }
    if node.chain.tip(c).ok().flatten().is_none() {
        if let Some((_, m)) = mesh.first_mut() {
            syncer.ensure_genesis(m).map_err(|e| format!("genesis: {e}"))?;
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
    node.chain.init_headers_round(syncer, &mut v, &run.stop).map_err(|e| e.to_string())
}

/// Bytes the coin derivation may hold in memory before it goes on on disk (ARCADE_INIT_COINS_MB,
/// default 4096). Bitcoin's coin set needs about 20 GB to stay in memory; on disk it needs
/// about 60 bytes per output and input of the whole history in the scratch folder instead.
fn coins_budget() -> usize {
    coins_budget_of(std::env::var("ARCADE_INIT_COINS_MB").ok().as_deref(), mem_total())
}

/// The coin budget for ARCADE_INIT_COINS_MB=`v` on a machine with `mem` bytes of memory (None:
/// not known): at most three quarters of the memory and never past COINS_MB_MAX, in bytes
/// without wrapping (a huge value used to wrap around in the shift, to a tiny or random budget).
fn coins_budget_of(v: Option<&str>, mem: Option<u64>) -> usize {
    let mb = v.and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(4096).min(COINS_MB_MAX);
    let mut bytes = mb.saturating_mul(1 << 20);
    if let Some(m) = mem {
        bytes = bytes.min(m / 4 * 3);
    }
    usize::try_from(bytes).unwrap_or(usize::MAX)
}

/// The most ARCADE_INIT_COINS_MB is taken as: 1 TB.
const COINS_MB_MAX: u64 = 1 << 20;

/// The machine's memory in bytes (/proc/meminfo MemTotal; None elsewhere).
fn mem_total() -> Option<u64> {
    let s = std::fs::read_to_string("/proc/meminfo").ok()?;
    let kb: u64 = s.lines().find(|l| l.starts_with("MemTotal:"))?.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb.saturating_mul(1024))
}

/// The build's scratch folder: beside the chain's own folder, on the drive the chain was put on
/// (Bitcoin's coin set derived on disk takes ~350 GB there). Removed with the chain.
pub fn scratch(node: &Node, c: u16) -> std::path::PathBuf {
    let own = node.chain.own_dir(c);
    let name = own.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| format!("chain-{c}"));
    own.with_file_name(format!("{name}.initsync"))
}

/// Scratch space the build's next steps may take, from the chain file's size (its blocks): the
/// tx index sort ~6% of it, the coin set derived on disk ~55% (when it won't fit in memory:
/// about 1/32 of the blocks' bytes).
fn scratch_need(chain_bytes: u64, built: &[String], budget: usize) -> u64 {
    let mut need = 0u64;
    if !built.iter().any(|s| s == "txs") {
        need += chain_bytes / 16;
    }
    if !built.iter().any(|s| s == "coins") && (budget as u64) < chain_bytes / 32 {
        need += chain_bytes / 100 * 55;
    }
    need + (2 << 30)
}

/// The build here: its scratch beside the chain, room for it first (Ok(None) while waiting).
fn build_here(node: &Arc<Node>, run: &ChainRun) -> arcade_store::Result<Option<arcade_chaindb::initsync::Built>> {
    let p = run.params;
    let c = p.chain_no;
    let tmp = scratch(node, c);
    let _ = std::fs::create_dir_all(&tmp);
    let budget = coins_budget();
    // Enough room for the scratch first: a full drive would stop the chain's own writes (and
    // every other file on it) half way.
    let chain_bytes = std::fs::metadata(node.chain.own_dir(c).join("chain.mdbx")).map(|m| m.len()).unwrap_or(0);
    let need = scratch_need(chain_bytes, &node.chain.init_built(c), budget);
    if let Some((free, _)) = crate::chaindrive::free_bytes(&tmp) {
        // The scratch of an earlier run is removed when the build starts: it counts as free.
        let old = dir_bytes(&tmp);
        if free + old < need {
            set(run, format!("init · build: needs about {} GB of scratch space in {}, {} GB free there; waiting (free some space, or set ARCADE_INIT_COINS_MB higher to derive the coins in memory)",
                need >> 30, tmp.display(), (free + old) >> 30));
            rest(run, 60);
            return Ok(None);
        }
    }
    let threads = arcade_chaindb::rules::script_threads();
    set(run, "init · build: starting".into());
    let r = node.chain.init_build(p, TAIL, threads, budget, &tmp, &run.stop, &mut |s| set(run, format!("init · {s}")));
    if r.is_err() && !run.stop.load(Ordering::Relaxed) {
        let _ = std::fs::remove_dir_all(&tmp);
    }
    r.map(Some)
}

/// Bytes in the files of a folder (and its folders).
fn dir_bytes(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .map(|e| match e.file_type() {
                    Ok(t) if t.is_dir() => dir_bytes(&e.path()),
                    _ => e.metadata().map(|m| m.len()).unwrap_or(0),
                })
                .sum()
        })
        .unwrap_or(0)
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
    // Each phase's timing lines (blocks/s, rows/s ...), the newest last.
    let lines = node.chain.init_timing(c);
    let timing = |w: &str| lines.iter().filter(|l| l.starts_with(w)).cloned().collect::<Vec<_>>();
    Some(vec![
        serde_json::json!({"name": "Init · headers", "detail": detail(0, format!("round {} · {}", round + 1, if note.is_empty() { "asking several sources".into() } else { note.clone() }), "agreed by at least 3 independent sources, enough work", "waiting"),
                           "pct": pct(0, hpct), "at": best, "of": peer, "timing": timing("headers")}),
        serde_json::json!({"name": "Init · blocks", "detail": detail(1, format!("round {round} · stored only, any order, checked against their headers{}", timing("blocks").last().map(|l| format!(" · last {l}")).unwrap_or_default()), "every header's block stored", "waiting for the headers"),
                           "pct": pct(1, bpct), "at": stored, "of": total, "timing": timing("blocks")}),
        serde_json::json!({"name": "Init · build", "detail": detail(2, format!("{} · done: {}", note, if built.is_empty() { "nothing yet".into() } else { built.join(", ") }), "tx index, coin set and rules done; the normal sync carries on", "waiting for the blocks"),
                           "pct": pct(2, upct), "at": null, "timing": timing("build")}),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::psync::tests::{test_chain, Src};
    use arcade_chain::{Block, Hash256};
    use arcade_chaindb::ChainDb;
    use std::cell::RefCell;
    use std::sync::atomic::AtomicBool;

    /// A source that sets `stop` once it was asked `after` times (None: never).
    struct Stopper {
        inner: Src,
        after: Option<usize>,
        asked: usize,
        stop: Arc<AtomicBool>,
    }

    impl Stopper {
        fn new(blocks: &[Block], after: Option<usize>, stop: &Arc<AtomicBool>) -> Stopper {
            Stopper { inner: Src { blocks: blocks.to_vec(), kind: 0, asked: Default::default() }, after, asked: 0, stop: stop.clone() }
        }
        /// Counts a question; true when this one stops the run.
        fn tick(&mut self) -> bool {
            self.asked += 1;
            let now = self.after.is_some_and(|a| self.asked > a);
            if now {
                self.stop.store(true, Ordering::Relaxed);
            }
            now
        }
    }

    impl BlockSource for Stopper {
        fn get_headers(&mut self, locator: &[Hash256]) -> Result<Vec<arcade_chain::p2p::HeaderEntry>, String> {
            let _ = self.tick();
            self.inner.get_headers(locator)
        }
        fn get_blocks(&mut self, hashes: &[Hash256]) -> Result<Vec<Vec<u8>>, String> {
            if self.tick() {
                return Err("stopped".into());
            }
            self.inner.get_blocks(hashes)
        }
        fn describe(&self) -> String {
            "test".into()
        }
    }

    /// The smallest world the phase loop runs in: three sources (three networks), store_round
    /// over one of them, the build in a temp folder. `stop_in` stops it part way through that
    /// phase, as an arcade stopped (or restarted) there.
    struct FakeEnv<'a> {
        db: &'a ChainDb,
        p: &'static arcade_chain::params::ChainParams,
        blocks: Vec<Block>,
        stop: Arc<AtomicBool>,
        stop_in: Option<&'static str>,
        upto: Option<u32>,
        upto_err: Option<String>,
        full: bool,
        tmp: std::path::PathBuf,
        events: RefCell<Vec<String>>,
        statuses: RefCell<Vec<String>>,
    }

    impl<'a> FakeEnv<'a> {
        fn new(db: &'a ChainDb, p: &'static arcade_chain::params::ChainParams, blocks: &[Block], tmp: &std::path::Path, stop_in: Option<&'static str>) -> FakeEnv<'a> {
            FakeEnv { db, p, blocks: blocks.to_vec(), stop: Default::default(), stop_in, upto: None, upto_err: None, full: false, tmp: tmp.to_path_buf(), events: Default::default(), statuses: Default::default() }
        }
    }

    impl Env for FakeEnv<'_> {
        fn stopped(&self) -> bool {
            self.stop.load(Ordering::Relaxed)
        }
        fn status(&self, s: String) {
            // A smoke test held at its height: that's where this test ends.
            if s.contains("stopped at block") {
                self.stop.store(true, Ordering::Relaxed);
            }
            self.statuses.borrow_mut().push(s);
        }
        fn event(&self, s: String) {
            self.events.borrow_mut().push(s);
        }
        fn rest(&self, _secs: u64) {}
        fn upto(&self) -> Result<Option<u32>, String> {
            self.upto_err.clone().map_or(Ok(self.upto), Err)
        }
        fn room(&self) -> Result<(), String> {
            if self.full {
                self.stop.store(true, Ordering::Relaxed);
                return Err("init · blocks held: drive full".into());
            }
            Ok(())
        }
        fn headers_round(&mut self, _round: u32) -> Result<arcade_chaindb::initsync::Agreement, String> {
            let syncer = Syncer::new(self.db, self.p);
            let stop_at = |i: usize| (self.stop_in == Some("headers") && i == 1).then_some(0);
            let mut srcs: Vec<Stopper> = (0..3).map(|i| Stopper::new(&self.blocks, stop_at(i), &self.stop)).collect();
            if self.db.tip(self.p.chain_no).map_err(|e| e.to_string())?.is_none() {
                syncer.ensure_genesis(&mut srcs[0]).map_err(|e| e.to_string())?;
            }
            let mut v: Vec<(String, &mut dyn BlockSource)> = srcs.iter_mut().enumerate().map(|(i, s)| (format!("net 10.{i}.0.0/16"), s as &mut dyn BlockSource)).collect();
            self.db.init_headers_round(&syncer, &mut v, &self.stop).map_err(|e| e.to_string())
        }
        fn store(&mut self, _round: u32, missing: &[Hash256]) -> crate::psync::Round {
            let after = (self.stop_in == Some("blocks")).then_some(1);
            let src = Stopper::new(&self.blocks, after, &self.stop);
            crate::psync::store_round(self.db, self.p, missing, vec![("net 10.0.0.0/16".into(), Box::new(src) as Box<dyn BlockSource + Send>)], None, Duration::from_secs(2), &|_| {}, &|| self.room(), &self.stop)
        }
        fn build(&mut self) -> arcade_store::Result<Option<arcade_chaindb::initsync::Built>> {
            let (stop, stop_here) = (self.stop.clone(), self.stop_in == Some("build"));
            self.db
                .init_build(self.p, 5, 2, usize::MAX, &self.tmp, &self.stop, &mut |s| {
                    if stop_here && s.contains("coin set") {
                        stop.store(true, Ordering::Relaxed);
                    }
                })
                .map(Some)
        }
    }

    #[test]
    fn the_phase_loop_goes_through_every_phase_and_carries_on_after_a_restart_in_each() {
        let (p, blocks) = test_chain(600);
        let c = p.chain_no;
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("chain.mdbx");
        let tmp = d.path().join("scratch");
        {
            let db = ChainDb::open(&path).unwrap();
            db.init_start(c).unwrap();
        }
        // Stopped in each phase in turn, the chain's file closed and opened again ("restart").
        for (stop_in, phase_after) in [("headers", Phase::Headers), ("blocks", Phase::Blocks), ("build", Phase::Build)] {
            let db = ChainDb::open(&path).unwrap();
            let mut env = FakeEnv::new(&db, p, &blocks, &tmp, Some(stop_in));
            assert!(!run_with(&db, p, &mut env), "stopped in {stop_in}");
            assert_eq!(db.init_phase(c).unwrap(), Some(phase_after), "stopped in {stop_in}: {:?}", env.statuses.borrow());
            assert_eq!(db.tip(c).unwrap().unwrap().height, 0, "nothing usable before the build is done");
            if stop_in == "blocks" {
                let left = db.init_missing(c).unwrap().len();
                assert!(left > 0 && left < 600, "part of the blocks stored: {left} left");
            }
        }
        // The last start runs to the end.
        let db = ChainDb::open(&path).unwrap();
        let mut env = FakeEnv::new(&db, p, &blocks, &tmp, None);
        assert!(run_with(&db, p, &mut env));
        assert_eq!(db.init_phase(c).unwrap(), Some(Phase::Done));
        assert_eq!(db.tip(c).unwrap().unwrap().height, 600 - TAIL.min(5), "built to best - tail");
        assert!(db.init_missing(c).unwrap().is_empty());
        // Comparable timing lines for every phase, kept with the chain and in the journal.
        let t = db.init_timing(c);
        for w in ["headers round", "blocks round", "build main", "build txs", "build coins:", "build coins written", "build: to block"] {
            assert!(t.iter().any(|l| l.starts_with(w)), "no {w:?} line in {t:?}");
        }
        assert!(t.iter().any(|l| l.starts_with("blocks round") && l.contains("blocks/s") && l.contains("MB/s")), "{t:?}");
        assert!(t.iter().any(|l| l.starts_with("build txs") && l.contains("rows/s")), "{t:?}");
        assert!(env.events.borrow().iter().any(|e| e.contains("init timing: build coins written") && e.contains("coins/s")), "{:?}", env.events.borrow());
    }

    #[test]
    fn a_bad_cap_stops_the_install_and_a_full_drive_holds_the_blocks() {
        assert_eq!(parse_upto(None), Ok(None));
        assert_eq!(parse_upto(Some(" 700000 ")), Ok(Some(700_000)));
        for bad in ["", "0", "-5", "70k", "1e6"] {
            assert!(parse_upto(Some(bad)).unwrap_err().contains("not a block height"), "{bad}");
        }
        assert_eq!(parse_free_gb(None), Ok(50 << 30));
        assert!(parse_free_gb(Some("lots")).is_err());
        let dir = std::path::Path::new("/x");
        assert!(room_left(Some(61 << 30), 50 << 30, dir).is_err());
        assert!(room_left(Some(63 << 30), 50 << 30, dir).is_ok());
        assert!(room_left(None, 50 << 30, dir).is_ok());
        let (p, blocks) = test_chain(100);
        let c = p.chain_no;
        let d = tempfile::tempdir().unwrap();
        let db = ChainDb::open(d.path().join("chain.mdbx")).unwrap();
        db.init_start(c).unwrap();
        // A cap that doesn't parse: nothing runs, and the status says why.
        let mut env = FakeEnv::new(&db, p, &blocks, &d.path().join("scratch"), None);
        env.upto_err = parse_upto(Some("12x")).err();
        env.stop.store(true, Ordering::Relaxed);
        assert!(!run_with(&db, p, &mut env));
        assert!(env.statuses.borrow()[0].contains("ARCADE_INIT_UPTO=\"12x\""), "{:?}", env.statuses.borrow());
        assert_eq!(db.tip(c).unwrap(), None, "not even the headers round ran");
        // The drive under its floor: the blocks phase holds, nothing stored.
        let mut env = FakeEnv::new(&db, p, &blocks, &d.path().join("scratch"), None);
        env.full = true;
        assert!(!run_with(&db, p, &mut env));
        assert_eq!(db.init_phase(c).unwrap(), Some(Phase::Blocks));
        assert_eq!(db.init_missing(c).unwrap().len(), 100);
        assert!(env.statuses.borrow().last().unwrap().contains("drive full"));
    }

    #[test]
    fn a_smoke_test_stops_storing_at_its_height_and_builds_nothing() {
        let (p, blocks) = test_chain(300);
        let c = p.chain_no;
        let d = tempfile::tempdir().unwrap();
        let db = ChainDb::open(d.path().join("chain.mdbx")).unwrap();
        db.init_start(c).unwrap();
        let mut env = FakeEnv::new(&db, p, &blocks, &d.path().join("scratch"), None);
        env.upto = Some(120);
        assert!(!run_with(&db, p, &mut env));
        assert!(env.events.borrow().iter().any(|e| e.contains("init: storing up to 120")), "{:?}", env.events.borrow());
        assert!(env.statuses.borrow().last().unwrap().contains("stopped at block 120"), "{:?}", env.statuses.borrow());
        assert_eq!(db.init_missing(c).unwrap().len(), 180, "blocks 1..=120 stored, none above");
        assert!(db.init_missing_upto(c, Some(120)).unwrap().is_empty());
        assert!(matches!(db.init_phase(c).unwrap(), Some(Phase::Headers | Phase::Blocks)));
        assert!(db.init_built(c).is_empty());
        assert!(db.init_timing(c).iter().any(|l| l.starts_with("blocks round 1: 120 blocks")), "{:?}", db.init_timing(c));
        // Its coins aren't answered meanwhile.
        let op = arcade_chain::OutPoint { txid: blocks[3].txs[0].txid, vout: 0 };
        assert!(db.coin(p, &op).is_err());
        assert!(db.balance_for_script(c, &[0x51]).is_err());
    }

    #[test]
    fn peers_count_once_per_network_group() {
        let a: std::net::SocketAddr = "203.0.113.7:8333".parse().unwrap();
        let b: std::net::SocketAddr = "203.0.9.9:8333".parse().unwrap();
        let x: std::net::SocketAddr = "198.51.100.7:8333".parse().unwrap();
        let v6: std::net::SocketAddr = "[2001:db8:1:2::5]:8333".parse().unwrap();
        assert_eq!(group(&a), group(&b));
        assert_ne!(group(&a), group(&x));
        assert_eq!(group(&v6), "net 2001:db8::/32");
        // A P2P peer and an arcade on the same host (or range) count once.
        assert_eq!(addr_group("203.0.113.7:7530"), group(&a));
        assert_eq!(addr_group("wss://203.0.9.1:443/mesh"), group(&a));
        assert_eq!(group(&"[::ffff:203.0.1.1]:8333".parse().unwrap()), group(&a));
        assert_ne!(addr_group("198.51.100.9:7530"), group(&a));
    }

    #[test]
    fn init_only_where_it_can_be_checked() {
        use arcade_chain::params::{ALL_CHAINS, BITCOIN_MAIN, DIGIBYTE_MAIN, DIGIBYTE_TEST, KOTIA_MAIN};
        assert!(check(&BITCOIN_MAIN, "full").is_ok() && check(&BITCOIN_MAIN, "chain").is_ok());
        assert!(check(&BITCOIN_MAIN, "light").is_err() && check(&BITCOIN_MAIN, "shared").is_err());
        // Bitcoin's floor is set and parses (a typo would turn it off).
        assert!(!BITCOIN_MAIN.min_chain_work().is_zero());
        // Proof of stake: refused, whatever else.
        assert!(check(&KOTIA_MAIN, "full").unwrap_err().contains("proof-of-stake"));
        // MWEB: refused, whatever else.
        use arcade_chain::params::{JUNKCOIN_MAIN, LITECOIN_MAIN, LITECOIN_TEST};
        for m in [&LITECOIN_MAIN, &LITECOIN_TEST, &JUNKCOIN_MAIN] {
            assert!(check(m, "full").unwrap_err().contains("MWEB"), "{}", m.key);
        }
        // No checkpoint checked against two sources: refused.
        assert!(check(&DIGIBYTE_MAIN, "full").unwrap_err().contains("no init checkpoint"));
        assert!(check(&DIGIBYTE_TEST, "full").unwrap_err().contains("no init checkpoint"));
        // Every chain init allows has a checkpoint, and a minimum chain work that parses where set.
        for p in ALL_CHAINS.iter().filter(|p| check(p, "full").is_ok()) {
            assert!(p.init_floor().is_some(), "{}", p.key);
            assert!(p.min_chain_work.is_empty() || !p.min_chain_work().is_zero(), "{}", p.key);
        }
    }

    #[test]
    fn the_coin_budget_is_capped_and_never_wraps() {
        let gb = 1u64 << 30;
        assert_eq!(coins_budget_of(None, None), 4096 << 20);
        assert_eq!(coins_budget_of(Some("24000"), Some(128 * gb)), 24_000 << 20);
        // Three quarters of the memory at most.
        assert_eq!(coins_budget_of(Some("24000"), Some(16 * gb)), (12 * gb) as usize);
        // Values that used to wrap in `<< 20`: capped at 1 TB (or the memory), never small.
        assert_eq!(coins_budget_of(Some("18446744073709551615"), None), (1u64 << 40) as usize);
        assert_eq!(coins_budget_of(Some("17592186044416"), Some(64 * gb)), (48 * gb) as usize);
        assert_eq!(coins_budget_of(Some("junk"), None), 4096 << 20);
    }

    #[test]
    fn scratch_need_follows_the_steps_left() {
        let gb = 1u64 << 30;
        let all = scratch_need(720 * gb, &[], 4096 << 20);
        assert!(all > 400 * gb, "{}", all >> 30);
        // Coins in memory (a budget over 1/32 of the blocks): the tx sort only.
        assert!(scratch_need(720 * gb, &[], 24_000 << 20) < 50 * gb);
        assert!(scratch_need(720 * gb, &["txs".into(), "coins".into()], 0) < 3 * gb);
    }
}
