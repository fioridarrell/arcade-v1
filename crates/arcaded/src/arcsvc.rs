//! The ARCADE chain inside the arcade (docs/ARCADECHAIN.md §14).
//!
//! **A local test chain only, for now.** It runs on the two stand-ins of crates/arcade-arc:
//! `StandIn` in place of the proof of time and `FixedHolders` in place of the storage
//! records. On such a chain anyone can write any block at once, so the chain is known only
//! to an arcade started with `--arc-local <settings.json>`, on a closed test network; an
//! arcade started without it does nothing here.
//!
//! **An optional install, as every coin but the home chain is.** The settings only say
//! which chain there is. It shows in Chains & Coins as not installed; `chains.add` installs
//! it (from then on it starts with the arcade) and `chains.remove` stops it and deletes its
//! blocks. Every frame names the chain's block 0, and an arcade answers only frames for the
//! chain it runs itself.
//!
//! What is here: the chain's rules (arcade-arc) decide every block; the block log keeps
//! them across a restart; the best branch is copied into the chain store for everything
//! else in the arcade to read; a writer waits out its draw and writes; and blocks travel
//! over mesh links as "ac" frames:
//!
//! ```text
//! tip  {g, id, h, w}        my best block: id, height, total difficulty
//! get  {g, id: n, loc}      send me your branch after the newest of these blocks you have
//! a    {g, id: n, seq, end, more} + blob     the blocks, oldest first (as "bk" answers)
//! inv  {g} + blob           ids (32 bytes each) of transactions waiting here, new to you
//! gtx  {g, id: n} + blob    send me these transactions (ids)
//! txs  {g, id: n, seq, end, left} + blob     the ones still waiting here, in the order asked;
//!                           `left`: how many ids at the end of the ask weren't reached
//! ```
//!
//! Transactions wait in the pool (arcade-arc `pool.rs`); the writer fills its block from it
//! and takes the fees.

use crate::node::Node;
use arcade_arc::block::{Block, Payout, TimeProof, TX_FORMAT};
use arcade_arc::chain::{Accepted, Chain, ChainError, Entry, FixedHolders, Genesis, Storage, Ticket};
use arcade_arc::light::Headers;
use arcade_arc::log::{self, BlockLog};
use arcade_arc::coins::TxError;
use arcade_arc::params::{self, Net};
use arcade_arc::pool::{Pool, PoolError};
use arcade_arc::time::{StandIn, TimeProofs};
use arcade_arc::{coins, script, store};
use arcade_chain::hash::Hash256;
use arcade_chain::params::ChainParams;
use arcade_chain::{OutPoint, Tx};
use arcade_chaindb::ChainDb;
use arcade_mesh::bk::{join_blocks, split_blocks, CHUNK};
use arcade_mesh::link::{Frame, Link, LinkError};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

/// Blocks in one answer, and its most bytes.
const ANSWER_BLOCKS: usize = 50;
const ANSWER_BYTES: usize = 4 << 20;
/// A peer is asked again for a branch it named after this long without it.
const ASK_AGAIN: Duration = Duration::from_secs(10);
/// A peer's asks are answered this far apart at the closest; one that comes sooner waits.
const SERVE_EVERY: Duration = Duration::from_millis(500);
/// Transaction ids in one `inv`, and in one `gtx`.
const INV_IDS: usize = 1_000;
const ASK_TXS: usize = 100;
/// Ids a peer told us of that we haven't asked for yet: more than these are forgotten.
const WANTED_TXS: usize = 5_000;
/// Transactions refused since the last block, remembered so they aren't fetched again.
const REFUSED_TXS: usize = 20_000;
/// A peer counts as linked while its link ticked this recently.
const LINKED: Duration = Duration::from_secs(30);
/// With no other arcade heard from for this long after the start, this arcade's copy is
/// taken to be the chain (it may be the only one on it) and can be lent.
const ALONE_AFTER: Duration = Duration::from_secs(60);
/// A lender goes on answering its borrowers for this long after it was last caught up. Each
/// new block leaves a copy a moment behind (a peer has named it, it isn't fetched yet), and a
/// borrower checks every header itself and follows the branch with the most work, so an
/// answer from a copy a block behind does it no harm.
const ANSWERS_AFTER: Duration = Duration::from_secs(30);
/// The most headers in one shared-core answer.
pub const LENT_HEADERS: usize = 500;

type LocalChain = Chain<StandIn, FixedHolders>;

/// What `--arc-local` reads: block 0, who holds how much, and how fast this arcade's
/// stand-in "calculates" (steps of proof of time per second).
pub struct Settings {
    pub genesis: Genesis,
    /// (arcade id, bytes held, where it is paid).
    pub holders: Vec<([u8; 32], u64, Payout)>,
    pub steps_per_second: u64,
    /// Bytes of blocks in one catalog segment; the rule's own size when not given. A local
    /// chain follows small source chains, so it cuts small segments.
    pub segment_bytes: u64,
}

impl Settings {
    pub fn parse(net: &Net, text: &str) -> Result<Settings, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("settings: {e}"))?;
        if v["net"] != net.key {
            return Err(format!("settings: \"net\" must be \"{}\"", net.key));
        }
        let hash = |k: &str| -> Result<[u8; 32], String> {
            let h = v["genesis"][k].as_str().ok_or(format!("settings: genesis.{k} missing"))?;
            hex::decode(h).ok().and_then(|b| b.try_into().ok()).ok_or(format!("settings: genesis.{k} must be 64 hex characters"))
        };
        let genesis = Genesis {
            time: v["genesis"]["time"].as_u64().ok_or("settings: genesis.time missing")?,
            closed_list: hash("closed_list")?,
            final_list: hash("final_list")?,
            beacon_1: hash("beacon_1")?,
            beacon_2: hash("beacon_2")?,
            // A string: the number can be longer than JSON numbers hold.
            difficulty: v["genesis"]["difficulty"].as_str().and_then(|d| d.parse().ok()).filter(|d| *d >= 1).ok_or("settings: genesis.difficulty must be a number in quotes")?,
        };
        let mut holders = Vec::new();
        for h in v["holders"].as_array().ok_or("settings: holders missing")? {
            let key = h["key"].as_str().and_then(|k| hex::decode(k).ok()).and_then(|b| b.try_into().ok()).ok_or("settings: a holder's key must be an arcade id")?;
            let bytes = h["bytes"].as_u64().ok_or("settings: a holder's bytes missing")?;
            let (version, hash) = h["pay"].as_str().and_then(arcade_ledger::address::decode_base58check).ok_or("settings: a holder's pay address doesn't read")?;
            let mut pay = [version; 21];
            if hash.len() != 20 {
                return Err("settings: a holder's pay address doesn't read".into());
            }
            pay[1..].copy_from_slice(&hash);
            holders.push((key, bytes, pay));
        }
        let steps_per_second = v["steps_per_second"].as_u64().filter(|s| *s >= 1).ok_or("settings: steps_per_second missing")?;
        let segment_bytes = match &v["segment_bytes"] {
            Value::Null => params::SEGMENT_BYTES,
            n => n.as_u64().filter(|b| (1..=params::SEGMENT_BYTES).contains(b)).ok_or("settings: segment_bytes must be from 1 to the rule's segment size")?,
        };
        Ok(Settings { genesis, holders, steps_per_second, segment_bytes })
    }
}

struct State {
    chain: LocalChain,
    log: BlockLog,
    /// When the tip became the tip here: a writer's wait runs from then.
    since: Instant,
    pool: Pool,
    /// Transactions this arcade wouldn't take, until the next block.
    refused: HashSet<Hash256>,
}

#[derive(Default)]
struct Peer {
    /// The tip we last told it.
    said: Option<Hash256>,
    /// We have told it a tip since this chain started here. The first one asks for its tip
    /// back: what it said before the chain was installed here was not heard.
    greeted: bool,
    /// The tip it last told us, while we don't have that block.
    wants: Option<Hash256>,
    /// It has told us a tip since this chain started here.
    heard: bool,
    /// When its link last ticked.
    seen: Option<Instant>,
    asked: Option<Instant>,
    ask_id: u64,
    /// The answer being put together: next frame number, bytes so far.
    parts: Option<(u64, Vec<u8>)>,
    served: Option<Instant>,
    /// Its ask that came too soon after the last answer, answered when its turn comes.
    waiting: Option<Value>,

    /// The newest waiting transaction it has been told of (the pool's numbering).
    tx_told: Option<u64>,
    /// Transactions it named that we don't have and haven't asked for yet.
    tx_wants: VecDeque<Hash256>,
    /// The ids of the ask that is out, and its answer so far.
    tx_asked: Option<(Instant, Vec<Hash256>)>,
    tx_ask_id: u64,
    tx_parts: Option<(u64, Vec<u8>)>,
    tx_served: Option<Instant>,
    tx_waiting: Option<(u64, Vec<u8>)>,
}

pub struct Svc {
    net: Net,
    pub params: &'static ChainParams,
    genesis_hex: String,
    steps_per_second: u64,
    /// Where this arcade's own blocks pay their fees.
    pay: Option<Vec<u8>>,
    st: Mutex<State>,
    peers: Mutex<HashMap<String, Peer>>,
    /// Set (under `st`) when the chain is removed: nothing is taken in or written after.
    stop: AtomicBool,
    started: Instant,
    /// When this copy was last seen caught up.
    last_up: Mutex<Option<Instant>>,
}

impl Svc {
    /// Whether this copy is as far as the arcades it is linked to: none of them has named a
    /// block it lacks, and one has spoken (or none did for a good while: it may be alone).
    fn caught_up(&self) -> bool {
        let peers = self.peers.lock().unwrap();
        let behind = peers.values().any(|p| p.wants.is_some() && p.seen.is_some_and(|t| t.elapsed() < LINKED));
        let up = !behind && (peers.values().any(|p| p.heard) || self.started.elapsed() > ALONE_AFTER);
        drop(peers);
        if up {
            *self.last_up.lock().unwrap() = Some(Instant::now());
        }
        up
    }

    /// Whether borrowers are answered: caught up, or was a moment ago ([`ANSWERS_AFTER`]).
    fn answers(&self) -> bool {
        self.caught_up() || still_answers(*self.last_up.lock().unwrap(), Instant::now())
    }
}

/// Whether a copy last caught up at `last_up` still answers its borrowers at `now`.
fn still_answers(last_up: Option<Instant>, now: Instant) -> bool {
    last_up.is_some_and(|t| now.saturating_duration_since(t) < ANSWERS_AFTER)
}

/// The chain this arcade can install: what its settings say.
struct Def {
    net: Net,
    settings: Settings,
    genesis: Hash256,
    params: &'static ChainParams,
}

static DEF: OnceLock<Def> = OnceLock::new();
/// The chain while it is installed.
static SVC: RwLock<Option<Arc<Svc>>> = RwLock::new(None);
/// One install or removal at a time.
static CHANGING: Mutex<()> = Mutex::new(());
/// While a removed chain's blocks are still being deleted.
static REMOVING: AtomicBool = AtomicBool::new(false);

fn svc() -> Option<Arc<Svc>> {
    SVC.read().unwrap().clone()
}

// ---------------------------------------------------------------------------------------
// The copy in the chain store

fn db_err(e: impl std::fmt::Display) -> String {
    format!("chain store: {e}")
}

/// Bring the store's copy of the chain to the branch the rules are on: blocks the chain
/// left are taken back, blocks it joined are added. Safe to run at any time, and again
/// after a stop part way.
pub fn reconcile<T: TimeProofs, S: Storage>(db: &ChainDb, p: &ChainParams, chain: &Chain<T, S>) -> Result<(), String> {
    let c = p.chain_no;
    let mut tip = db.tip(c).map_err(db_err)?;
    while let Some(t) = tip {
        if chain.id_at(t.height as u64) == Some(t.hash) {
            break;
        }
        // Not on the branch. Block 0 can't be taken back, and the store keeps what it needs
        // to take back only its newest blocks: past either, the copy is made again whole.
        tip = if t.height == 0 { None } else { db.disconnect_tip(c).ok() };
        if tip.is_none() {
            while db.remove_chain_batch(c, 50_000).map_err(db_err)? > 0 {}
        }
    }
    let top = chain.tip().height;
    let mut next = tip.map_or(0, |t| t.height as u64 + 1);
    while next <= top {
        let mut batch = Vec::new();
        while next <= top && batch.len() < 200 {
            let id = chain.id_at(next).expect("a height of the branch");
            batch.push(match chain.block(&id) {
                Some(b) => store::stored_block(b),
                None => store::stored_block_0(id, chain.entry(&id).expect("block 0").time),
            });
            next += 1;
        }
        db.connect_blocks_checked(p, &batch, false).map_err(db_err)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Taking blocks in

/// Hand a block to the chain; what the chain keeps goes to the log, and a new tip to the store.
fn take(node: &Node, svc: &Svc, block: Block) -> Result<Accepted, ChainError> {
    let mut st = svc.st.lock().unwrap();
    if svc.stop.load(Ordering::Relaxed) {
        return Ok(Accepted::Known);
    }
    let got = st.chain.accept(block.clone(), arcade_app::now())?;
    if got == Accepted::Known {
        return Ok(got);
    }
    if let Err(e) = st.log.append(&block) {
        // Still on the chain in memory; after a restart it comes from other arcades again.
        eprintln!("arc: block log: {e}");
    }
    if let Accepted::Tip { undone, .. } = &got {
        st.since = Instant::now();
        // The transactions of blocks the chain left wait again, oldest first.
        let st = &mut *st;
        let back: Vec<Tx> = undone.iter().rev().filter_map(|id| st.chain.block(id)).flat_map(|b| b.txs.iter().skip(1).cloned()).collect();
        st.pool.retip(st.chain.coins(), st.chain.tip().height + 1, back);
        st.refused.clear();
        if let Err(e) = reconcile(&node.chain, svc.params, &st.chain) {
            eprintln!("arc: {e}");
        }
    }
    Ok(got)
}

/// A transaction from this arcade or from a peer: into the pool if it may wait there. Its fee.
fn take_tx(svc: &Svc, raw: &[u8]) -> Result<(Hash256, u64), PoolError> {
    if raw.len() > coins::MAX_ONE_TX_BYTES {
        return Err(PoolError::Tx(TxError::TooBig));
    }
    let tx = Tx::parse_with(raw, &TX_FORMAT).map_err(|_| PoolError::Tx(TxError::Format))?;
    let id = tx.txid;
    let mut st = svc.st.lock().unwrap();
    let st = &mut *st;
    if st.refused.contains(&id) {
        return Err(PoolError::Refused);
    }
    match st.pool.add(tx, st.chain.coins(), st.chain.tip().height + 1) {
        Ok(fee) => Ok((id, fee)),
        Err(e) => {
            // A coin that isn't there may yet come (its transaction is on the way, or a
            // block is): anything else stays refused until the next block.
            if !matches!(e, PoolError::Known | PoolError::Tx(TxError::MissingCoin)) && st.refused.len() < REFUSED_TXS {
                st.refused.insert(id);
            }
            Err(e)
        }
    }
}

/// The proof of time behind the draw seed of the epoch the block after the tip starts, where
/// it starts one.
fn seed_proof(chain: &LocalChain) -> Option<TimeProof> {
    let tip = chain.tip();
    if (tip.height + 1) % params::EPOCH_BLOCKS != 0 {
        return None;
    }
    let (challenge, steps) = chain.draw_seed_task(&tip.id)?;
    Some(StandIn.prove(&challenge, steps))
}

/// This arcade's block on the tip, as its ticket allows: the coinbase pays what is owed,
/// then the fees of the waiting transactions the block takes to `pay`.
fn write_block(chain: &LocalChain, pool: &Pool, pay: Option<&[u8]>, secret: &[u8; 32], ticket: &Ticket, seed: Option<TimeProof>, now: u64) -> Block {
    let time = now.max(chain.tip().time + 1);
    let mut outputs = chain.payouts();
    // Room for the transactions: the block's, less the coinbase with a fee output.
    let fee_out = pay.map(|script| (u64::MAX, script.to_vec()));
    let coinbase_bytes = coins::coinbase(ticket.height, &ticket.prev, b"", &[outputs.as_slice(), fee_out.as_slice()].concat()).raw.len();
    let (mut txs, fees) = pool.pick(params::MAX_TX_BYTES.saturating_sub(coinbase_bytes));
    // Fees under the dust amount can't be paid out, and with nowhere to pay they are burned.
    if let Some(script) = pay.filter(|_| fees >= params::DUST) {
        outputs.push((fees, script.to_vec()));
    }
    txs.insert(0, coins::coinbase(ticket.height, &ticket.prev, b"", &outputs));
    ticket.seal(chain.net(), secret, time, txs, Vec::new(), StandIn.prove(&ticket.challenge, ticket.steps), seed)
}

/// The writer: draw on each new tip, wait as long as the draw's proof of time would take,
/// write. One block per tip: a block the chain doesn't take yet is offered again, never
/// written a second time.
fn run_writer(node: Arc<Node>, svc: Arc<Svc>) {
    let secret = node.identity.to_bytes();
    // The tip drawn on, and what came of it.
    let mut on: Option<Hash256> = None;
    let mut ticket: Option<(Ticket, Option<TimeProof>)> = None;
    let mut written: Option<Block> = None;
    loop {
        std::thread::sleep(Duration::from_millis(100));
        if svc.stop.load(Ordering::Relaxed) {
            return;
        }
        if written.is_none() {
            let st = svc.st.lock().unwrap();
            let tip = st.chain.tip().id;
            if on != Some(tip) {
                on = Some(tip);
                let seed = seed_proof(&st.chain);
                ticket = st.chain.ticket(&secret, seed.as_ref()).map(|t| (t, seed));
            }
            let Some((t, seed)) = &ticket else { continue };
            if st.since.elapsed().as_secs_f64() < t.steps as f64 / svc.steps_per_second as f64 {
                continue;
            }
            written = Some(write_block(&st.chain, &st.pool, svc.pay.as_deref(), &secret, t, seed.clone(), arcade_app::now()));
            ticket = None;
        }
        let Some(b) = written.clone() else { continue };
        match take(&node, &svc, b) {
            // Its time is ahead of the clock (blocks came faster than seconds pass): later.
            Err(ChainError::AheadOfClock) => std::thread::sleep(Duration::from_secs(1)),
            Err(e) => {
                // The pool and the rules disagree about a transaction, which must never
                // happen: rather an empty pool than a writer that loses every block.
                eprintln!("arc: our block wasn't taken: {e}");
                svc.st.lock().unwrap().pool.clear();
                written = None;
            }
            Ok(_) => written = None,
        }
    }
}

// ---------------------------------------------------------------------------------------
// Start

fn installed_key(net: &Net) -> Vec<u8> {
    format!("arc:{}", net.key).into_bytes()
}

/// Read the settings of the chain this arcade can install, and start it if it is installed.
pub fn define(node: &Arc<Node>, settings: &std::path::Path) -> Result<(), String> {
    let net = params::TESTNET;
    let text = std::fs::read_to_string(settings).map_err(|e| format!("{}: {e}", settings.display()))?;
    let s = Settings::parse(&net, &text)?;
    FixedHolders::new(&net, &s.holders).ok_or("settings: the holders list doesn't hold (empty, a key twice, nothing held, or an address of another network)")?;
    let genesis = s.genesis.id(&net);
    let def = Def { net, params: store::chain_params(&net, &genesis), genesis, settings: s };
    let def = DEF.get_or_init(|| def);
    let installed = node.app.store.read(|r| r.get(arcade_app::META, &installed_key(&net))).map_err(db_err)?.is_some();
    if installed {
        let _changing = CHANGING.lock().unwrap();
        run(node, def)?;
    } else {
        eprintln!("arc: local test chain {} (block 0 {genesis}) can be installed in Chains & Coins", net.key);
    }
    Ok(())
}

/// Whether `key` names the chain this arcade can install ("arctest"; any capitalisation).
pub fn defines(key: &str) -> bool {
    DEF.get().is_some_and(|d| d.params.key.eq_ignore_ascii_case(key))
}

/// `chains.add` for the ARCADE chain: start it, and from now on with the arcade.
pub fn install(node: &Arc<Node>) -> Result<(), String> {
    let def = DEF.get().ok_or("this arcade knows no ARCADE chain")?;
    let _changing = CHANGING.lock().unwrap();
    if svc().is_some() {
        return Err(format!("{} is already installed", def.params.name));
    }
    if REMOVING.load(Ordering::Relaxed) {
        return Err(format!("{} is still being removed; try again in a moment", def.params.name));
    }
    run(node, def)?;
    node.app.store.write(|w| w.put(arcade_app::META, &installed_key(&def.net), b"1")).map_err(db_err)?;
    let _ = node.app.event("chains", &format!("installed {}", def.params.name));
    Ok(())
}

/// `chains.remove` for the ARCADE chain: stop it and delete its blocks (in the background).
/// Coins on it are not lost: they are on the chain, and the wallet's keys stay.
pub fn remove(node: &Arc<Node>) -> Result<(), String> {
    let def = DEF.get().ok_or("this arcade knows no ARCADE chain")?;
    let _changing = CHANGING.lock().unwrap();
    let svc = SVC.write().unwrap().take().ok_or_else(|| format!("{} is not installed", def.params.name))?;
    // Under the chain's lock: a block being taken in right now is in before, or never.
    {
        let _st = svc.st.lock().unwrap();
        svc.stop.store(true, Ordering::Relaxed);
    }
    node.app.store.write(|w| w.del(arcade_app::META, &installed_key(&def.net)).map(|_| ())).map_err(db_err)?;
    REMOVING.store(true, Ordering::Relaxed);
    let (n, c, name, dir) = (node.clone(), def.params.chain_no, def.params.name, node.home.join("arc").join(def.net.key));
    std::thread::spawn(move || {
        drop(svc);
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("arc: {}: {e}", dir.display());
            }
        }
        while n.chain.remove_chain_batch(c, 20_000).map(|x| x > 0).unwrap_or(false) {}
        REMOVING.store(false, Ordering::Relaxed);
        let _ = n.app.event("chains", &format!("removed {name} and its data"));
    });
    Ok(())
}

/// Bring the chain up from its block log and start its writer. The caller holds `CHANGING`.
fn run(node: &Arc<Node>, def: &Def) -> Result<(), String> {
    let (net, s, genesis, p) = (def.net, &def.settings, def.genesis, def.params);
    let holders = FixedHolders::new(&net, &s.holders).ok_or("settings: the holders list doesn't hold")?.with_segment_bytes(s.segment_bytes);
    let mut chain = Chain::new(net, &s.genesis, StandIn, holders, &[]);

    let dir = node.home.join("arc").join(net.key);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let (log, blocks) = BlockLog::open(dir.join("blocks.log"), &genesis).map_err(|e| format!("block log: {e}"))?;
    let (held, t0) = (blocks.len(), Instant::now());
    let refused = log::replay(&mut chain, blocks);
    reconcile(&node.chain, p, &chain)?;

    let me = node.identity.verifying_key().to_bytes();
    let weight = chain.storage().weight(&me);
    let pay = s.holders.iter().find(|(key, ..)| *key == me).and_then(|(_, _, pay)| script::pay_to(&net, pay));
    eprintln!(
        "arc: LOCAL TEST CHAIN {} (stand-ins for the proof of time and the storage records; never for a public network). Block 0 {genesis}, height {} from {held} logged blocks in {:.1}s{}; this arcade {}",
        net.key,
        chain.tip().height,
        t0.elapsed().as_secs_f64(),
        if refused > 0 { format!(" ({refused} no longer taken)") } else { String::new() },
        if weight > 0 { format!("writes with {weight} bytes") } else { "holds nothing and only follows".into() },
    );
    let svc = Arc::new(Svc {
        net,
        params: p,
        genesis_hex: genesis.to_hex(),
        steps_per_second: s.steps_per_second,
        pay,
        st: Mutex::new(State { chain, log, since: Instant::now(), pool: Pool::default(), refused: HashSet::new() }),
        peers: Default::default(),
        stop: AtomicBool::new(false),
        started: Instant::now(),
        last_up: Mutex::new(None),
    });
    *SVC.write().unwrap() = Some(svc.clone());
    if weight > 0 {
        let n = node.clone();
        std::thread::Builder::new().name("arc writer".into()).spawn(move || run_writer(n, svc)).map_err(|e| e.to_string())?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------
// The mesh

fn frame(svc: &Svc, op: &str, mut head: Value) -> Frame {
    head["t"] = json!("ac");
    head["op"] = json!(op);
    head["g"] = json!(svc.genesis_hex);
    Frame::new(head)
}

fn id_of(v: &Value) -> Option<Hash256> {
    Hash256::from_hex(v.as_str()?).ok()
}

/// Ask `peer` for the branch it named: our newest blocks, thinning out further back, tell
/// it where the two chains part.
fn ask(svc: &Svc, link: &mut Link) -> Result<(), LinkError> {
    let loc: Vec<String> = {
        let st = svc.st.lock().unwrap();
        let top = st.chain.tip().height;
        let (mut back, mut step, mut loc) = (0u64, 1u64, Vec::new());
        while back <= top && loc.len() < 32 {
            loc.push(st.chain.id_at(top - back).expect("a height of the branch").to_hex());
            if loc.len() >= 3 {
                step *= 2;
            }
            back += step;
        }
        loc
    };
    let id = {
        let mut peers = svc.peers.lock().unwrap();
        let p = peers.entry(link.peer_id.clone()).or_default();
        p.ask_id += 1;
        p.asked = Some(Instant::now());
        p.parts = Some((0, Vec::new()));
        p.ask_id
    };
    link.send(&frame(svc, "get", json!({"id": id, "loc": loc})))
}

/// Answer a `get`: our branch after the newest block of `loc` that is on it.
fn serve(svc: &Svc, link: &mut Link, head: &Value) -> Result<(), LinkError> {
    {
        let mut peers = svc.peers.lock().unwrap();
        let p = peers.entry(link.peer_id.clone()).or_default();
        if p.served.is_some_and(|t| t.elapsed() < SERVE_EVERY) {
            // Too soon after the last answer: it waits its turn (`tick`). A newer ask
            // takes an older one's place.
            p.waiting = Some(head.clone());
            return Ok(());
        }
        p.served = Some(Instant::now());
        p.waiting = None;
    }
    let Some(id) = head["id"].as_u64() else { return Ok(()) };
    let (blocks, more) = {
        let st = svc.st.lock().unwrap();
        let chain = &st.chain;
        let known = head["loc"].as_array().into_iter().flatten().take(32).filter_map(id_of).find_map(|l| {
            let h = chain.entry(&l)?.height;
            (chain.id_at(h) == Some(l)).then_some(h)
        });
        let top = chain.tip().height;
        let mut next = known.unwrap_or(0) + 1;
        let (mut blocks, mut bytes) = (Vec::new(), 0usize);
        while next <= top && blocks.len() < ANSWER_BLOCKS {
            let b = chain.block(&chain.id_at(next).expect("a height of the branch")).expect("a block of the branch").encode();
            if !blocks.is_empty() && bytes + b.len() > ANSWER_BYTES {
                break;
            }
            bytes += b.len();
            blocks.push(b);
            next += 1;
        }
        (blocks, next <= top)
    };
    let data = join_blocks(&blocks);
    let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(CHUNK).collect() };
    let n = chunks.len();
    for (i, c) in chunks.into_iter().enumerate() {
        let mut f = frame(svc, "a", json!({"id": id, "seq": i, "end": i + 1 == n, "more": more}));
        f.blob = c.to_vec();
        link.send(&f)?;
    }
    Ok(())
}

/// A frame of an answer to our `get`. With the last one the blocks go to the chain.
fn on_answer(node: &Node, svc: &Svc, link: &mut Link, f: &Frame) -> Result<(), LinkError> {
    let data = {
        let mut peers = svc.peers.lock().unwrap();
        let Some(p) = peers.get_mut(&link.peer_id) else { return Ok(()) };
        if f.head["id"].as_u64() != Some(p.ask_id) {
            return Ok(());
        }
        let Some((next, bytes)) = p.parts.as_mut() else { return Ok(()) };
        if f.head["seq"].as_u64() != Some(*next) || bytes.len() + f.blob.len() > ANSWER_BYTES + (ANSWER_BLOCKS * 4) {
            // Out of order or too long: not an answer we can use. Asked again later.
            p.parts = None;
            return Ok(());
        }
        *next += 1;
        bytes.extend_from_slice(&f.blob);
        if f.head["end"] != true {
            return Ok(());
        }
        p.parts.take().expect("just used").1
    };
    let mut gained = false;
    for raw in split_blocks(&data).unwrap_or_default() {
        let Ok(block) = Block::decode(&raw) else { break };
        match take(node, svc, block) {
            Ok(Accepted::Known) => {}
            Ok(_) => gained = true,
            Err(e) => {
                eprintln!("arc: a block from {} wasn't taken: {e}", &link.peer_id[..link.peer_id.len().min(12)]);
                break;
            }
        }
    }
    let again = {
        let st = svc.st.lock().unwrap();
        let mut peers = svc.peers.lock().unwrap();
        let p = peers.entry(link.peer_id.clone()).or_default();
        if p.wants.is_some_and(|w| st.chain.entry(&w).is_some()) {
            p.wants = None;
        }
        // An answer that brought something: the next ask needn't wait. So too one that
        // brought nothing new because another arcade was quicker with the same blocks. Only
        // a peer that named a block and didn't bring us nearer it is asked again after a while.
        if gained || p.wants.is_none() {
            p.asked = None;
        }
        gained && (f.head["more"] == true || p.wants.is_some())
    };
    if again {
        ask(svc, link)?;
    }
    Ok(())
}

pub fn on_frame(node: &Node, link: &mut Link, f: &Frame) -> Result<(), LinkError> {
    let Some(svc) = svc() else { return Ok(()) };
    let svc = &*svc;
    if f.head["g"] != svc.genesis_hex.as_str() {
        return Ok(());
    }
    match f.head["op"].as_str().unwrap_or("") {
        "tip" => {
            let Some(id) = id_of(&f.head["id"]) else { return Ok(()) };
            let have = svc.st.lock().unwrap().chain.entry(&id).is_some();
            let ask_now = {
                let mut peers = svc.peers.lock().unwrap();
                let p = peers.entry(link.peer_id.clone()).or_default();
                if f.head["new"] == true {
                    p.said = None;
                }
                p.heard = true;
                p.wants = (!have).then_some(id);
                // While an ask is out, its answer decides whether to ask again.
                !have && p.asked.is_none_or(|t| t.elapsed() > ASK_AGAIN)
            };
            if ask_now {
                ask(svc, link)?;
            }
        }
        "get" => serve(svc, link, &f.head)?,
        "a" => on_answer(node, svc, link, f)?,
        "inv" => on_inv(svc, link, &f.blob)?,
        "gtx" => {
            if let Some(id) = f.head["id"].as_u64() {
                serve_txs(svc, link, id, &f.blob)?;
            }
        }
        "txs" => on_txs(svc, link, f)?,
        _ => {}
    }
    Ok(())
}

// Transactions.

fn ids_of(blob: &[u8], most: usize) -> Vec<Hash256> {
    blob.chunks_exact(32).take(most).map(|c| Hash256(c.try_into().expect("32 bytes"))).collect()
}

fn blob_of(ids: &[Hash256]) -> Vec<u8> {
    ids.iter().flat_map(|id| id.0).collect()
}

/// A peer named transactions waiting there: the ones new to us are asked for.
fn on_inv(svc: &Svc, link: &mut Link, blob: &[u8]) -> Result<(), LinkError> {
    let new: Vec<Hash256> = {
        let st = svc.st.lock().unwrap();
        ids_of(blob, INV_IDS).into_iter().filter(|id| !st.pool.contains(id) && !st.refused.contains(id)).collect()
    };
    {
        let mut peers = svc.peers.lock().unwrap();
        let p = peers.entry(link.peer_id.clone()).or_default();
        for id in new {
            if p.tx_wants.len() >= WANTED_TXS {
                break;
            }
            p.tx_wants.push_back(id);
        }
    }
    ask_txs(svc, link)
}

/// Ask `peer` for the next transactions it named, unless an ask is out.
fn ask_txs(svc: &Svc, link: &mut Link) -> Result<(), LinkError> {
    let (id, ids) = {
        let st = svc.st.lock().unwrap();
        let mut peers = svc.peers.lock().unwrap();
        let p = peers.entry(link.peer_id.clone()).or_default();
        if p.tx_asked.as_ref().is_some_and(|(at, _)| at.elapsed() <= ASK_AGAIN) {
            return Ok(());
        }
        let mut ids = Vec::new();
        while ids.len() < ASK_TXS {
            let Some(id) = p.tx_wants.pop_front() else { break };
            if !st.pool.contains(&id) && !st.refused.contains(&id) && !ids.contains(&id) {
                ids.push(id);
            }
        }
        if ids.is_empty() {
            p.tx_asked = None;
            return Ok(());
        }
        p.tx_ask_id += 1;
        p.tx_asked = Some((Instant::now(), ids.clone()));
        p.tx_parts = Some((0, Vec::new()));
        (p.tx_ask_id, ids)
    };
    let mut f = frame(svc, "gtx", json!({"id": id}));
    f.blob = blob_of(&ids);
    link.send(&f)
}

/// Answer a `gtx`: the transactions of the ask that still wait here.
fn serve_txs(svc: &Svc, link: &mut Link, id: u64, blob: &[u8]) -> Result<(), LinkError> {
    {
        let mut peers = svc.peers.lock().unwrap();
        let p = peers.entry(link.peer_id.clone()).or_default();
        if p.tx_served.is_some_and(|t| t.elapsed() < SERVE_EVERY) {
            p.tx_waiting = Some((id, blob.to_vec()));
            return Ok(());
        }
        p.tx_served = Some(Instant::now());
        p.tx_waiting = None;
    }
    let ids = ids_of(blob, ASK_TXS);
    let (txs, left) = {
        let st = svc.st.lock().unwrap();
        let (mut txs, mut bytes, mut reached) = (Vec::new(), 0usize, 0usize);
        for id in &ids {
            if let Some(w) = st.pool.get(id) {
                if !txs.is_empty() && bytes + w.tx.raw.len() > ANSWER_BYTES {
                    break;
                }
                bytes += w.tx.raw.len();
                txs.push(w.tx.raw.clone());
            }
            reached += 1;
        }
        (txs, ids.len() - reached)
    };
    let data = join_blocks(&txs);
    let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(CHUNK).collect() };
    let n = chunks.len();
    for (i, c) in chunks.into_iter().enumerate() {
        let mut f = frame(svc, "txs", json!({"id": id, "seq": i, "end": i + 1 == n, "left": left}));
        f.blob = c.to_vec();
        link.send(&f)?;
    }
    Ok(())
}

/// A frame of an answer to our `gtx`. With the last one the transactions go to the pool.
fn on_txs(svc: &Svc, link: &mut Link, f: &Frame) -> Result<(), LinkError> {
    let data = {
        let mut peers = svc.peers.lock().unwrap();
        let Some(p) = peers.get_mut(&link.peer_id) else { return Ok(()) };
        if f.head["id"].as_u64() != Some(p.tx_ask_id) {
            return Ok(());
        }
        let Some((next, bytes)) = p.tx_parts.as_mut() else { return Ok(()) };
        if f.head["seq"].as_u64() != Some(*next) || bytes.len() + f.blob.len() > ANSWER_BYTES + coins::MAX_ONE_TX_BYTES + (ASK_TXS * 4) {
            p.tx_parts = None;
            return Ok(());
        }
        *next += 1;
        bytes.extend_from_slice(&f.blob);
        if f.head["end"] != true {
            return Ok(());
        }
        let data = p.tx_parts.take().expect("just used").1;
        // The ids at the end of the ask that the answer didn't reach are asked for again.
        let (_, asked) = p.tx_asked.take().unwrap_or((Instant::now(), Vec::new()));
        let left = (f.head["left"].as_u64().unwrap_or(0) as usize).min(asked.len());
        for id in asked[asked.len() - left..].iter().rev() {
            p.tx_wants.push_front(*id);
        }
        data
    };
    for raw in split_blocks(&data).unwrap_or_default().into_iter().take(ASK_TXS) {
        let _ = take_tx(svc, &raw);
    }
    ask_txs(svc, link)
}

/// Per link, often: our tip when this peer hasn't heard it, and an ask that went unanswered.
pub fn tick(_node: &Node, link: &mut Link) -> Result<(), LinkError> {
    let Some(svc) = svc() else { return Ok(()) };
    let svc = &*svc;
    let (id, height, total) = {
        let st = svc.st.lock().unwrap();
        let t = st.chain.tip();
        (t.id, t.height, t.total_difficulty.to_string())
    };
    let (say, first, ask_again) = {
        let mut peers = svc.peers.lock().unwrap();
        let p = peers.entry(link.peer_id.clone()).or_default();
        p.seen = Some(Instant::now());
        let say = p.said != Some(id);
        p.said = Some(id);
        let first = say && !p.greeted;
        p.greeted |= say;
        (say, first, p.wants.is_some() && p.asked.is_none_or(|t| t.elapsed() > ASK_AGAIN))
    };
    if say {
        link.send(&frame(svc, "tip", json!({"id": id.to_hex(), "h": height, "w": total, "new": first})))?;
    }
    if ask_again {
        ask(svc, link)?;
    }

    // Asks of this peer that had to wait their turn.
    let (due, tx_due) = {
        let mut peers = svc.peers.lock().unwrap();
        let p = peers.entry(link.peer_id.clone()).or_default();
        let turn = |last: Option<Instant>| last.is_none_or(|t| t.elapsed() >= SERVE_EVERY);
        (if turn(p.served) { p.waiting.take() } else { None }, if turn(p.tx_served) { p.tx_waiting.take() } else { None })
    };
    if let Some(head) = due {
        serve(svc, link, &head)?;
    }
    if let Some((id, blob)) = tx_due {
        serve_txs(svc, link, id, &blob)?;
    }

    // Waiting transactions this peer hasn't been told of, and ones it named that we lack.
    let news: Vec<Hash256> = {
        let st = svc.st.lock().unwrap();
        let mut peers = svc.peers.lock().unwrap();
        let p = peers.entry(link.peer_id.clone()).or_default();
        let news = st.pool.after(p.tx_told, INV_IDS);
        if let Some((seq, _)) = news.last() {
            p.tx_told = Some(*seq);
        }
        news.into_iter().map(|(_, id)| id).collect()
    };
    if !news.is_empty() {
        let mut f = frame(svc, "inv", json!({}));
        f.blob = blob_of(&news);
        link.send(&f)?;
    }
    ask_txs(svc, link)
}

/// A new link to `peer`: it hears our tip again, whatever an earlier link was told.
pub fn new_link(peer: &str) {
    if let Some(svc) = svc() {
        if let Some(p) = svc.peers.lock().unwrap().get_mut(peer) {
            p.said = None;
            p.tx_told = None;
        }
    }
}

// ---------------------------------------------------------------------------------------
// The local API

/// `arc.status`: where the chain is.
pub fn status(node: &Node) -> Result<Value, String> {
    let Some(svc) = svc() else { return Ok(json!({"on": false})) };
    let st = svc.st.lock().unwrap();
    let t = st.chain.tip();
    let me = node.identity.verifying_key().to_bytes();
    Ok(json!({
        "on": true,
        "local": true,
        "net": svc.net.key,
        "genesis": svc.genesis_hex,
        "height": t.height,
        "tip": t.id.to_hex(),
        "time": t.time,
        "difficulty": t.difficulty.to_string(),
        "total_difficulty": t.total_difficulty.to_string(),
        "weight": st.chain.storage().weight(&me),
        "pot": st.chain.storage().pot(),
        "coins": st.chain.coins().len(),
        "waiting": st.pool.len(),
        "waiting_bytes": st.pool.bytes(),
        "stored": node.chain.tip(svc.params.chain_no).map_err(db_err)?.map(|s| s.height),
    }))
}

// For the wallet, the Explorer and Chains & Coins. The chain isn't in the list of coins
// (`ALL_CHAINS`): these are its own entries. It is known by its key whenever this arcade
// can install it: installed it is read from this arcade's own copy, not installed it can be
// borrowed from arcades that have it (`arcborrow`), as any coin.

/// The ARCADE chain this arcade knows (installed or not), by its key ("arctest"; any
/// capitalisation).
pub fn params_for(key: &str) -> Option<&'static ChainParams> {
    defined().filter(|p| p.key.eq_ignore_ascii_case(key))
}

/// The ARCADE chain this arcade knows: the one it can install.
pub fn defined() -> Option<&'static ChainParams> {
    DEF.get().map(|d| d.params)
}

/// Whether `p` is the ARCADE chain installed here.
pub fn runs(p: &ChainParams) -> bool {
    svc().is_some_and(|s| s.params.chain_no == p.chain_no)
}

/// The waiting transaction that spends a coin, so a wallet doesn't pick it again.
pub fn spender(at: &OutPoint) -> Option<Hash256> {
    svc()?.st.lock().unwrap().pool.spender(at)
}

/// A wallet's signed transaction: into the pool, from where it goes to the other arcades.
pub fn send_raw(raw: &[u8]) -> Result<(Hash256, u64), String> {
    let svc = svc().ok_or("the ARCADE chain isn't running here")?;
    take_tx(&svc, raw).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------------------
// Lending the chain (docs/ARCADECHAIN.md §14.14)
//
// An arcade with the chain installed lends it as it lends any coin's core (`coreshare`):
// the same offer, the same calls. Most calls are answered from the copy in the chain store.
// The ones below are the chain's own: a borrower checks headers by this chain's rules, so
// blocks and headers are handed over in the chain's own form.

/// The chain, once this arcade's copy has caught up: what it offers to lend.
pub fn lends() -> Option<&'static ChainParams> {
    let svc = svc()?;
    svc.caught_up().then_some(svc.params)
}

/// The chain, while this arcade answers the borrowers it has: caught up, or was a moment ago.
/// Longer than [`lends`] says, so a question that arrives just as a new block is told isn't
/// turned away.
pub fn answers() -> Option<&'static ChainParams> {
    let svc = svc()?;
    svc.answers().then_some(svc.params)
}

/// A block this arcade holds (on any branch), as the chain writes it. Block 0 has none.
pub fn raw_block(id: &Hash256) -> Option<Vec<u8>> {
    svc()?.st.lock().unwrap().chain.block(id).map(Block::encode)
}

/// The header of a block this arcade holds, as the chain writes it.
pub fn raw_header(id: &Hash256) -> Option<Vec<u8>> {
    let svc = svc()?;
    let st = svc.st.lock().unwrap();
    let mut out = Vec::new();
    st.chain.block(id)?.header.encode(&mut out);
    Some(out)
}

/// The headers of the branch this arcade is on, from height `from` (1 or later), at most
/// `count` and never more than [`LENT_HEADERS`].
pub fn raw_headers(from: u64, count: usize) -> Vec<Vec<u8>> {
    let Some(svc) = svc() else { return Vec::new() };
    let st = svc.st.lock().unwrap();
    let top = st.chain.tip().height;
    (from.max(1)..=top)
        .take(count.min(LENT_HEADERS))
        .filter_map(|h| {
            let mut out = Vec::new();
            st.chain.block(&st.chain.id_at(h)?)?.header.encode(&mut out);
            Some(out)
        })
        .collect()
}

/// A borrower's signed transaction. One that already waits here is no error to the borrower:
/// it asks every lender, and they tell one another.
pub fn send_lent(raw: &[u8]) -> Result<Hash256, String> {
    let svc = svc().ok_or("the ARCADE chain isn't running here")?;
    match take_tx(&svc, raw) {
        Ok((id, _)) => Ok(id),
        Err(PoolError::Known) => Tx::parse_with(raw, &TX_FORMAT).map(|t| t.txid).map_err(|_| TxError::Format.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

// For the borrowed copy (`arcborrow`): what the settings say of the chain. With the stand-in
// storage records every arcade knows each writer's draw weight from its settings; with the
// real ones a borrower will have to be shown it (§14.14, open).

/// The headers-only chain, at block 0.
pub fn light() -> Option<Headers<StandIn>> {
    let d = DEF.get()?;
    Some(Headers::new(d.net, &d.settings.genesis, StandIn))
}

/// The headers-only chain, picked up from the newest headers this arcade had checked.
pub fn light_from(entries: Vec<Entry>) -> Option<Headers<StandIn>> {
    let d = DEF.get()?;
    Headers::resume(d.net, &d.settings.genesis, StandIn, entries)
}

/// The draw weight of a writer.
pub fn writer_weight(key: &[u8; 32]) -> u64 {
    DEF.get().and_then(|d| d.settings.holders.iter().find(|(k, ..)| k == key)).map_or(0, |(_, bytes, _)| *bytes)
}

/// The defined chain's network, block 0's id and block 0's time.
pub fn defined_net() -> Option<(Net, Hash256, u64)> {
    DEF.get().map(|d| (d.net, d.genesis, d.settings.genesis.time))
}

/// The chain's row in `chains.list`, shaped as a coin's: installed, borrowed or neither, as
/// any coin. `arcade` marks it: its blocks come over the arcade mesh, so there is no storage
/// mode or index to choose.
pub fn row(node: &Node) -> Option<Value> {
    let key = defined()?.key;
    let st = crate::chains::status(node, key).ok()?;
    let mut row = json!({
        "ord": false, "tokens": Value::Null, "dunes": false, "runes": false, "sizes": Value::Null,
        "offers": node.offers_for(key).len(), "borrowed": st["borrowed"] == true, "queued": Value::Null,
    });
    for k in ["key", "name", "installed", "home", "arcade", "local", "mode", "indexes", "overall", "state", "tip", "waiting", "lenders", "floor", "mismatch"] {
        row[k] = st[k].clone();
    }
    Some(row)
}

/// `chains.status` for the chain this arcade can install (None: it knows none).
pub fn chain_status(node: &Node) -> Option<Value> {
    let def = DEF.get()?;
    let p = def.params;
    let Some(svc) = svc() else {
        let state = if REMOVING.load(Ordering::Relaxed) { "being removed" } else { "" };
        return Some(json!({"key": p.key, "name": p.name, "installed": false, "home": false, "arcade": true, "local": true, "state": state}));
    };
    let (height, waiting) = {
        let st = svc.st.lock().unwrap();
        (st.chain.tip().height, st.pool.len())
    };
    let addresses = node.chain.addresses_on(p.chain_no).unwrap_or(false);
    let links = svc.peers.lock().unwrap().len();
    Some(json!({
        "key": p.key, "name": p.name, "installed": true, "home": false, "arcade": true, "local": true,
        "mode": "full", "indexes": if addresses { vec!["addresses"] } else { vec![] },
        "overall": 100.0, "state": format!("block {height}"), "tip": height, "waiting": waiting,
        "stages": [{
            "name": "Blocks", "pct": 100.0, "info": true,
            "detail": format!("block {height} · {waiting} transaction{} waiting · heard from {links} arcade{}", if waiting == 1 { "" } else { "s" }, if links == 1 { "" } else { "s" }),
        }],
    }))
}

/// `arc.block`: a block of the branch this arcade is on, by height, or any block it holds, by id.
pub fn block(req: &Value) -> Result<Value, String> {
    let svc = svc().ok_or("the ARCADE chain isn't running here")?;
    let st = svc.st.lock().unwrap();
    let id = match req["height"].as_u64() {
        Some(h) => st.chain.id_at(h).ok_or("no block at that height")?,
        None => id_of(&req["id"]).ok_or("height or id required")?,
    };
    let e = st.chain.entry(&id).ok_or("no such block")?;
    let b = st.chain.block(&id);
    Ok(json!({
        "id": e.id.to_hex(),
        "prev": e.prev.to_hex(),
        "height": e.height,
        "time": e.time,
        "difficulty": e.difficulty.to_string(),
        "writer": hex::encode(e.writer),
        "score": e.score.to_string(),
        "steps": e.steps,
        "best": st.chain.id_at(e.height) == Some(id),
        "txs": b.map(|b| b.txs.iter().map(|t| t.txid.to_hex()).collect::<Vec<_>>()).unwrap_or_default(),
        // With "raw": true, the transactions themselves, in the block's order.
        "raw": if req["raw"] == true { json!(b.map(|b| b.txs.iter().map(|t| hex::encode(&t.raw)).collect::<Vec<_>>()).unwrap_or_default()) } else { Value::Null },
        "bytes": b.map(|b| b.encode().len()).unwrap_or(0),
    }))
}

/// `arc.submit`: a signed transaction (`hex`) into the pool, from where it goes to the
/// other arcades and into a block.
pub fn submit(req: &Value) -> Result<Value, String> {
    let svc = svc().ok_or("the ARCADE chain isn't running here")?;
    let raw = req["hex"].as_str().and_then(|h| hex::decode(h).ok()).ok_or("hex required")?;
    let (id, fee) = take_tx(&svc, &raw).map_err(|e| e.to_string())?;
    Ok(json!({"id": id.to_hex(), "fee": fee}))
}

/// `arc.pool`: the transactions waiting here, in order of arrival.
/// The waiting transactions as they are, in the order they came (at most `max`).
pub fn waiting(max: usize) -> Vec<Vec<u8>> {
    svc().map(|s| s.st.lock().unwrap().pool.iter().take(max).map(|w| w.tx.raw.clone()).collect()).unwrap_or_default()
}

pub fn pool(_req: &Value) -> Result<Value, String> {
    let svc = svc().ok_or("the ARCADE chain isn't running here")?;
    let st = svc.st.lock().unwrap();
    let txs: Vec<Value> = st.pool.iter().take(1_000).map(|w| json!({"id": w.tx.txid.to_hex(), "fee": w.fee, "bytes": w.tx.raw.len()})).collect();
    Ok(json!({"count": st.pool.len(), "bytes": st.pool.bytes(), "txs": txs}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcade_arc::vrf;
    use arcade_chain::hash::hash160;
    use arcade_chain::{OutPoint, TxIn, TxOut};
    use arcade_ledger::address::base58check;
    use secp256k1::{PublicKey, Secp256k1, SecretKey};

    const NET: Net = params::TESTNET;
    const START: u64 = 1_797_000_000;
    const LATE: u64 = START + 10_000_000;

    /// A lender answers its borrowers through a moment's lag, and no longer.
    #[test]
    fn a_lender_a_moment_behind_still_answers() {
        let now = Instant::now();
        assert!(!still_answers(None, now), "never caught up: a copy still being made");
        assert!(still_answers(Some(now), now));
        assert!(still_answers(Some(now), now + ANSWERS_AFTER - Duration::from_secs(1)));
        assert!(!still_answers(Some(now), now + ANSWERS_AFTER));
    }

    fn secret(who: u8) -> [u8; 32] {
        [who; 32]
    }

    /// The coin key `who` is paid to: the secret key, the public key and its hash.
    fn coin_key(who: u8) -> (SecretKey, Vec<u8>, [u8; 20]) {
        let sk = SecretKey::from_slice(&[who; 32]).unwrap();
        let pk = PublicKey::from_secret_key(&Secp256k1::new(), &sk).serialize().to_vec();
        let hash = hash160(&pk);
        (sk, pk, hash)
    }

    fn pay_script(who: u8) -> Vec<u8> {
        let mut address = [NET.p2pkh_version; 21];
        address[1..].copy_from_slice(&coin_key(who).2);
        script::pay_to(&NET, &address).unwrap()
    }

    fn settings() -> String {
        let holders: Vec<Value> =
            [1u8, 2].iter().map(|&w| json!({"key": hex::encode(vrf::public_key(&secret(w))), "bytes": (w as u64) << 30, "pay": base58check(NET.p2pkh_version, &coin_key(w).2)})).collect();
        json!({
            "net": "arctest",
            "genesis": {"time": START, "closed_list": "01".repeat(32), "final_list": "02".repeat(32), "beacon_1": "03".repeat(32), "beacon_2": "04".repeat(32), "difficulty": "1099511627776"},
            "holders": holders,
            "steps_per_second": 1_000_000,
        })
        .to_string()
    }

    fn chain() -> LocalChain {
        let s = Settings::parse(&NET, &settings()).unwrap();
        Chain::new(NET, &s.genesis, StandIn, FixedHolders::new(&NET, &s.holders).unwrap(), &[])
    }

    /// `who`'s block on the tip, 150 seconds after it, with what waits in `pool`.
    fn block_with(c: &LocalChain, who: u8, pool: &Pool, pay: Option<&[u8]>) -> Block {
        let seed = seed_proof(c);
        let t = c.ticket(&secret(who), seed.as_ref()).unwrap();
        write_block(c, pool, pay, &secret(who), &t, seed, c.tip().time + params::BLOCK_SECONDS)
    }

    fn block_by(c: &LocalChain, who: u8) -> Block {
        block_with(c, who, &Pool::default(), Some(&pay_script(who)))
    }

    /// `who` spends its coin `from` (worth `value`): `amount` to `to`, the rest less `fee` back.
    fn payment(who: u8, from: OutPoint, value: u64, to: u8, amount: u64, fee: u64) -> Tx {
        let (sk, pk, _) = coin_key(who);
        let mut tx = Tx {
            version: 1,
            time: None,
            inputs: vec![TxIn { prevout: from, script_sig: Vec::new(), sequence: u32::MAX, witness: Vec::new() }],
            outputs: vec![TxOut { value: amount, script_pubkey: pay_script(to) }, TxOut { value: value - amount - fee, script_pubkey: pay_script(who) }],
            lock_time: 0,
            txid: Hash256::ZERO,
            raw: Vec::new(),
            hogex: false,
        };
        let digest = script::sighash(&tx, 0, &pay_script(who), script::SIGHASH_ALL).unwrap();
        let mut unlock = Vec::new();
        script::push(&mut unlock, &arcade_wallet::legacy::sign(&sk, &digest, script::SIGHASH_ALL as u32));
        script::push(&mut unlock, &pk);
        tx.inputs[0].script_sig = unlock;
        Tx::parse_with(&tx.encode_legacy(), &TX_FORMAT).unwrap()
    }

    fn grow(c: &mut LocalChain, who: u8, blocks: u64) {
        for _ in 0..blocks {
            let b = block_by(c, who);
            assert!(matches!(c.accept(b, LATE), Ok(Accepted::Tip { .. })));
        }
    }

    fn stored_matches(db: &ChainDb, p: &ChainParams, c: &LocalChain) {
        let tip = db.tip(p.chain_no).unwrap().unwrap();
        assert_eq!((tip.height as u64, tip.hash), (c.tip().height, c.tip().id));
        for h in 0..=c.tip().height {
            assert_eq!(db.hash_at(p.chain_no, h as u32).unwrap(), c.id_at(h));
        }
    }

    #[test]
    fn settings_read_or_say_what_is_wrong() {
        let s = Settings::parse(&NET, &settings()).unwrap();
        assert_eq!(s.holders.len(), 2);
        assert_eq!(s.holders[1].1, 2 << 30);
        assert_eq!(s.holders[0].2[0], NET.p2pkh_version);
        assert_eq!(s.genesis.difficulty, 1 << 40);
        assert!(Settings::parse(&params::MAINNET, &settings()).is_err());
        assert!(Settings::parse(&NET, &settings().replace("\"1099511627776\"", "\"0\"")).is_err());
        assert!(Settings::parse(&NET, &settings().replace("closed_list", "closed")).is_err());
        assert!(Settings::parse(&NET, "{").is_err());
    }

    #[test]
    fn the_store_follows_the_chain_through_growth_and_a_change_of_branch() {
        let dir = tempfile::tempdir().unwrap();
        let db = ChainDb::open(dir.path().join("chain.mdbx")).unwrap();
        let mut c = chain();
        let p = store::chain_params(&NET, &c.id_at(0).unwrap());

        // Block 0 alone, then 105 blocks (past the first pot).
        reconcile(&db, p, &c).unwrap();
        stored_matches(&db, p, &c);
        grow(&mut c, 1, 105);
        reconcile(&db, p, &c).unwrap();
        stored_matches(&db, p, &c);
        // Again changes nothing.
        reconcile(&db, p, &c).unwrap();
        stored_matches(&db, p, &c);

        // Another arcade went its own way from block 100: eight blocks by the other writer
        // against this one's last five, so this one changes branch and the store with it.
        let mut other = chain();
        for h in 1..=100 {
            other.accept(c.block(&c.id_at(h).unwrap()).unwrap().clone(), LATE).unwrap();
        }
        grow(&mut other, 2, 8);
        let before = c.tip().id;
        for h in 101..=108 {
            c.accept(other.block(&other.id_at(h).unwrap()).unwrap().clone(), LATE).unwrap();
        }
        assert_ne!(c.tip().id, before);
        assert_eq!(c.tip().id, other.tip().id);
        reconcile(&db, p, &c).unwrap();
        stored_matches(&db, p, &c);

        // The coins the store counts are the coins the rules count.
        let paid = base58check(NET.p2pkh_version, &coin_key(1).2);
        let script = arcade_ledger::address::script_for_address(p, &paid).unwrap();
        let by_rules: u64 = c.coins().values().filter(|coin| coin.script == script).map(|coin| coin.value).sum();
        assert!(by_rules > 0);
        let stored: u64 = c.coins().iter().filter(|(_, coin)| coin.script == script).map(|(o, _)| db.coin(p, o).unwrap().expect("a coin of the rules is in the store").value).sum();
        assert_eq!(stored, by_rules);
    }

    #[test]
    fn a_block_takes_what_waits_and_its_writer_the_fees() {
        let mut c = chain();
        // The first pot is paid in block 100 and can be spent 100 blocks on.
        grow(&mut c, 2, 199);
        let mine = pay_script(1);
        let (from, coin) = c.coins().iter().find(|(_, coin)| coin.script == mine).map(|(at, coin)| (*at, coin.clone())).expect("holder 1 was paid");
        assert!(coin.coinbase && coin.height == 100);

        let mut pool = Pool::default();
        let next = c.tip().height + 1;
        let first = payment(1, from, coin.value, 2, 7 * params::COIN, params::COIN / 100);
        let change = OutPoint { txid: first.txid, vout: 1 };
        let second = payment(1, change, coin.value - 7 * params::COIN - params::COIN / 100, 2, params::COIN, params::COIN / 50);
        assert_eq!(pool.add(first.clone(), c.coins(), next), Ok(params::COIN / 100));
        assert_eq!(pool.add(second.clone(), c.coins(), next), Ok(params::COIN / 50));
        // A block earlier the reward is too young, and the rules say so too.
        assert!(Pool::default().add(first.clone(), c.coins(), next - 1).is_err());

        // Writer 2's block carries both, and its coinbase ends with their fees.
        let fees = params::COIN / 100 + params::COIN / 50;
        let b = block_with(&c, 2, &pool, Some(&pay_script(2)));
        assert_eq!(b.txs.iter().map(|t| t.txid).collect::<Vec<_>>()[1..], [first.txid, second.txid]);
        let last = b.txs[0].outputs.last().unwrap();
        assert_eq!((last.value, &last.script_pubkey), (fees, &pay_script(2)));
        // A writer with nowhere to be paid leaves the fees: the block holds all the same.
        let burnt = block_with(&c, 1, &pool, None);
        assert_eq!(burnt.txs.len(), 3);
        assert_eq!(burnt.txs[0].outputs.len(), c.payouts().len());
        assert_eq!(b.txs[0].outputs.len(), c.payouts().len() + 1);
        let mut other = chain();
        for h in 1..=c.tip().height {
            other.accept(c.block(&c.id_at(h).unwrap()).unwrap().clone(), LATE).unwrap();
        }
        assert!(matches!(other.accept(burnt, LATE), Ok(Accepted::Tip { .. })));

        assert!(matches!(c.accept(b, LATE), Ok(Accepted::Tip { .. })));
        let theirs: u64 = c.coins().values().filter(|coin| coin.script == pay_script(2) && !coin.coinbase).map(|coin| coin.value).sum();
        assert_eq!(theirs, 8 * params::COIN);
        assert!(c.coins().values().any(|coin| coin.coinbase && coin.height == 200 && coin.value == fees));
        pool.retip(c.coins(), c.tip().height + 1, Vec::new());
        assert!(pool.is_empty());
    }

    #[test]
    fn a_copy_that_went_wrong_is_made_again() {
        let dir = tempfile::tempdir().unwrap();
        let db = ChainDb::open(dir.path().join("chain.mdbx")).unwrap();
        let mut c = chain();
        let p = store::chain_params(&NET, &c.id_at(0).unwrap());
        grow(&mut c, 1, 12);
        reconcile(&db, p, &c).unwrap();

        // The arcade comes back with a shorter chain (its log lost its end): the store
        // steps back to it.
        let mut short = chain();
        for h in 1..=7 {
            short.accept(c.block(&c.id_at(h).unwrap()).unwrap().clone(), LATE).unwrap();
        }
        reconcile(&db, p, &short).unwrap();
        stored_matches(&db, p, &short);

        // And with another chain's settings in the same home: nothing of the old copy stays.
        let text = settings().replace(&"04".repeat(32), &"05".repeat(32));
        let s = Settings::parse(&NET, &text).unwrap();
        let mut new = Chain::new(NET, &s.genesis, StandIn, FixedHolders::new(&NET, &s.holders).unwrap(), &[]);
        grow(&mut new, 2, 3);
        let p2 = store::chain_params(&NET, &new.id_at(0).unwrap());
        reconcile(&db, p2, &new).unwrap();
        stored_matches(&db, p2, &new);
    }
}
