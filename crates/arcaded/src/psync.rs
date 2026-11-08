//! Parallel block download: many sources at once for one chain, connected strictly in order.
//!
//! * A hash feeder asks one arcade for block hashes ahead of our tip (getblockhash, 500 a
//!   call) and cuts them into jobs of JOB blocks.
//! * One worker per source takes jobs from a shared queue and fetches those blocks: every
//!   arcade on the mesh that offers the chain (existing arcades' Core nodes over coreshare,
//!   V1.0 arcades over bk) plus a few of the coin's P2P peers. A fast source takes more jobs;
//!   a source that fails gives its job back and, after repeated failures, stops.
//! * The chain's own thread pulls finished jobs in height order through [`Prefetched`], a
//!   BlockSource the normal Syncer reads: so every header is checked (proof of work, links,
//!   AuxPoW) and every block connected (merkle, byte-identical, in-commit indexes) exactly as
//!   with one source. A hash that doesn't link (a source lying or on a fork) fails the check,
//!   and that job is fetched again from the start of the gap.

use crate::node::Node;
use arcade_chain::p2p::HeaderEntry;
use arcade_chain::params::ChainParams;
use arcade_chain::{Block, Hash256};
use arcade_chaindb::sync::{BlockSource, Syncer};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const JOB: u32 = 250;
/// P2P peers kept working at once (the coin's network is the other side of "mesh + P2P").
const P2P_WANT: usize = 6;
/// How far ahead of the connected tip hashes are fetched (bounds memory).
const AHEAD: u32 = 12_000;

struct Job {
    start: u32,
    hashes: Vec<Hash256>,
    tries: u32,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    /// Finished jobs by start height: (raw blocks, source name).
    ready: BTreeMap<u32, (Vec<Vec<u8>>, String)>,
    /// Next height the feeder will cut a job for.
    next: u32,
    /// The connector's next expected height.
    want: u32,
    /// Blocks done per source.
    stats: BTreeMap<String, u64>,
    /// Sources still working.
    workers: usize,
    /// P2P workers among them.
    p2p: usize,
    /// Jobs being fetched: start -> (hashes, since, already doubled).
    inflight: BTreeMap<u32, (Vec<Hash256>, Instant, bool)>,
}

pub struct Pool {
    s: Mutex<State>,
    cv: Condvar,
    stop: AtomicBool,
    top: u32,
}

impl Pool {
    fn put_back(&self, job: Job) {
        let mut st = self.s.lock().unwrap();
        st.queue.push_front(Job { tries: job.tries + 1, ..job });
        self.cv.notify_all();
    }

    /// "arcade 31ff… 41% · P2P 1.2.3.4 25% · …" for the status line.
    pub fn summary(&self) -> String {
        let st = self.s.lock().unwrap();
        let total: u64 = st.stats.values().sum();
        if total == 0 {
            return format!("{} sources starting", st.workers);
        }
        let mut v: Vec<(&String, &u64)> = st.stats.iter().collect();
        v.sort_by(|a, b| b.1.cmp(a.1));
        format!("{} sources ({} P2P) [queue {} · fetching {} · done {} · hashes to {}]: {}", st.workers, st.p2p, st.queue.len(), st.inflight.len(), st.ready.len(), st.next, v.iter().take(6).map(|(k, n)| format!("{k} {}%", **n * 100 / total)).collect::<Vec<_>>().join(" · "))
    }
}

/// The ordered side: what the Syncer reads. Headers come from the blocks themselves.
pub struct Prefetched<'p> {
    pool: &'p Pool,
    cache: HashMap<Hash256, Vec<u8>>,
    p: &'static ChainParams,
}

impl Prefetched<'_> {
    /// The next finished job in height order (waits up to `wait`): its headers, in order.
    fn pull(&mut self, wait: Duration) -> Result<Vec<HeaderEntry>, String> {
        let deadline = Instant::now() + wait;
        let mut st = self.pool.s.lock().unwrap();
        loop {
            let want = st.want;
            if let Some((raws, _src)) = st.ready.remove(&want) {
                st.want = want + raws.len() as u32;
                self.pool.cv.notify_all();
                drop(st);
                let mut heads = Vec::with_capacity(raws.len());
                for raw in raws {
                    let b = Block::parse_with(&raw, &self.p.format).map_err(|e| e.to_string())?;
                    heads.push(HeaderEntry { header: b.header, auxpow: b.auxpow.clone() });
                    self.cache.insert(b.id, raw);
                }
                return Ok(heads);
            }
            if st.workers == 0 && Instant::now() + Duration::from_secs(150) > deadline {
                // Nobody working for a while (the recruiter had 30 s to bring someone).
                return Err("every source stopped".into());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(format!("no source delivered block {want} in time"));
            }
            // The next job in line is slow: let another source fetch it too (first copy wins).
            if let Some((hashes, since, doubled)) = st.inflight.get_mut(&want) {
                if !*doubled && since.elapsed() > Duration::from_secs(5) {
                    *doubled = true;
                    let hashes = hashes.clone();
                    st.queue.push_front(Job { start: want, hashes, tries: 0 });
                    self.pool.cv.notify_all();
                }
            }
            st = self.pool.cv.wait_timeout(st, (deadline - now).min(Duration::from_secs(1))).unwrap().0;
        }
    }
}

impl BlockSource for Prefetched<'_> {
    fn get_headers(&mut self, _locator: &[Hash256]) -> Result<Vec<HeaderEntry>, String> {
        self.pull(Duration::from_secs(180))
    }

    fn get_blocks(&mut self, hashes: &[Hash256]) -> Result<Vec<Vec<u8>>, String> {
        let mut guard = 0;
        while hashes.iter().any(|h| !self.cache.contains_key(h)) {
            self.pull(Duration::from_secs(180))?;
            guard += 1;
            if guard > 200 {
                return Err("blocks asked for are not coming from any source".into());
            }
        }
        Ok(hashes.iter().map(|h| self.cache.remove(h).unwrap()).collect())
    }

    fn describe(&self) -> String {
        self.pool.summary()
    }
}

fn worker(pool: &Pool, name: String, mut src: Box<dyn BlockSource + Send + '_>) {
    let mut fails = 0;
    while !pool.stop.load(Ordering::Relaxed) {
        let job = {
            let mut st = pool.s.lock().unwrap();
            loop {
                if pool.stop.load(Ordering::Relaxed) {
                    st.workers -= 1;
                    pool.cv.notify_all();
                    return;
                }
                // Don't run too far ahead of the connector (memory).
                let ahead_ok = st.queue.front().is_some_and(|j| j.start < st.want + AHEAD);
                if ahead_ok {
                    let j = st.queue.pop_front().unwrap();
                    // A doubled job that another source already delivered: skip it.
                    if j.start < st.want || st.ready.contains_key(&j.start) {
                        continue;
                    }
                    st.inflight.entry(j.start).or_insert((j.hashes.clone(), Instant::now(), false));
                    break j;
                }
                if st.want > pool.top {
                    st.workers -= 1;
                    pool.cv.notify_all();
                    return;
                }
                st = pool.cv.wait_timeout(st, Duration::from_millis(500)).unwrap().0;
            }
        };
        match src.get_blocks(&job.hashes) {
            Ok(raws) if raws.len() == job.hashes.len() => {
                fails = 0;
                let mut st = pool.s.lock().unwrap();
                st.inflight.remove(&job.start);
                // First copy wins; a late duplicate is dropped.
                if job.start >= st.want && !st.ready.contains_key(&job.start) {
                    *st.stats.entry(name.clone()).or_insert(0) += raws.len() as u64;
                    st.ready.insert(job.start, (raws, name.clone()));
                }
                pool.cv.notify_all();
            }
            _ => {
                pool.put_back(job);
                fails += 1;
                if fails >= 3 {
                    break;
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
    let mut st = pool.s.lock().unwrap();
    st.workers -= 1;
    pool.cv.notify_all();
}

/// Where block hashes ahead come from: a P2P peer (2,000 headers a message, fast) or an
/// existing arcade's Core node (getblockhash, 500 a call, a few seconds each).
pub enum HashSource<'a> {
    P2p(arcade_chain::p2p::Peer, &'static ChainParams),
    Core(crate::crsource::CoreShareSource<'a>),
}

/// Feed the queue with block hashes ahead of the connector.
fn feeder(pool: &Pool, mut hs: HashSource, mut last: Hash256) {
    while !pool.stop.load(Ordering::Relaxed) {
        let (next, want) = {
            let st = pool.s.lock().unwrap();
            (st.next, st.want)
        };
        if next > pool.top {
            return;
        }
        if next >= want + AHEAD {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        let got: Result<Vec<Hash256>, String> = match &mut hs {
            HashSource::P2p(peer, p) => peer
                .get_headers(&[last])
                .map(|es| es.iter().map(|e| p.block_id(&e.header)).collect())
                .map_err(|e| e.to_string()),
            HashSource::Core(c) => c.hashes(next, (pool.top - next + 1).min(500)),
        };
        match got {
            Ok(hashes) if !hashes.is_empty() => {
                let hashes: Vec<Hash256> = hashes.into_iter().take((pool.top - next + 1) as usize).collect();
                let mut st = pool.s.lock().unwrap();
                for (i, chunk) in hashes.chunks(JOB as usize).enumerate() {
                    st.queue.push_back(Job { start: next + i as u32 * JOB, hashes: chunk.to_vec(), tries: 0 });
                }
                st.next = next + hashes.len() as u32;
                last = *hashes.last().unwrap();
                pool.cv.notify_all();
            }
            _ => std::thread::sleep(Duration::from_secs(3)),
        }
    }
}

/// Bring a chain up to `top` from every source at once. Returns blocks connected.
pub fn run(node: &Node, p: &'static ChainParams, syncer: &Syncer, top: u32, on_progress: &dyn Fn(String), stop: &AtomicBool) -> Result<u32, String> {
    let c = p.chain_no;
    let tip = node.chain.tip(c).map_err(|e| e.to_string())?.map(|t| t.height).ok_or("no genesis yet")?;
    if top <= tip + 10 {
        return Ok(0);
    }
    let offers = node.offers_for(p.key);
    let cr: Vec<&(String, String, &'static str, u64)> = offers.iter().filter(|o| o.2 == "cr").collect();
    // Hashes ahead: a P2P peer if one answers (fast), else an arcade's Core node.
    let tip_hash = node.chain.tip(c).map_err(|e| e.to_string())?.map(|t| t.hash).ok_or("no tip")?;
    let feeder_src = node
        .coin_candidates(p)
        .iter()
        .take(6)
        .find_map(|a| arcade_chain::p2p::Peer::connect(p, a, Duration::from_secs(6)).ok().filter(|pr| (pr.version.start_height.max(0) as u32) >= top.min(tip + 2000)))
        .map(|pr| HashSource::P2p(pr, p))
        .or_else(|| cr.iter().find_map(|o| crate::crsource::CoreShareSource::dial(node, &o.1, p).ok()).map(HashSource::Core))
        .ok_or("no source for block hashes")?;
    let pool = Pool { s: Mutex::new(State { next: tip + 1, want: tip + 1, ..Default::default() }), cv: Condvar::new(), stop: AtomicBool::new(false), top };
    let short = |id: &str| id[..id.len().min(8)].to_string();
    std::thread::scope(|sc| -> Result<(), String> {
        sc.spawn(|| feeder(&pool, feeder_src, tip_hash));
        // Every arcade offering the chain, then a few P2P peers.
        for (id, addr, kind, their) in offers.iter() {
            if (*their as u32) < tip + 10 {
                continue;
            }
            let src: Option<Box<dyn BlockSource + Send>> = match *kind {
                "bk" => crate::meshsource::MeshSource::dial_chain(node, addr, p).ok().map(|s| Box::new(s) as Box<dyn BlockSource + Send>),
                _ => crate::crsource::CoreShareSource::dial(node, addr, p).ok().map(|s| Box::new(s) as Box<dyn BlockSource + Send>),
            };
            if let Some(src) = src {
                pool.s.lock().unwrap().workers += 1;
                let name = format!("arcade {}", short(id));
                let pool = &pool;
                sc.spawn(move || worker(pool, name, src));
            }
        }
        // P2P peers: keep P2P_WANT of them working; when one drops out, recruit a fresh one.
        // A peer that failed rests for 10 minutes before it is tried again.
        let pool_r = &pool;
        sc.spawn(move || {
            let mut resting: HashMap<String, Instant> = HashMap::new();
            let mut busy: std::collections::HashSet<String> = Default::default();
            while !pool_r.stop.load(Ordering::Relaxed) {
                let p2p_now = pool_r.s.lock().unwrap().p2p;
                if p2p_now < P2P_WANT {
                    resting.retain(|_, t| t.elapsed() < Duration::from_secs(600));
                    let cand = node.coin_candidates(p).into_iter().find(|a| !resting.contains_key(a) && !busy.contains(a));
                    match cand {
                        Some(a) => match arcade_chain::p2p::Peer::connect(p, &a, Duration::from_secs(8)) {
                            Ok(peer) if (peer.version.start_height.max(0) as u32) >= tip + 10 => {
                                node.note_coin_peer(p.net, &a);
                                {
                                    let mut st = pool_r.s.lock().unwrap();
                                    st.workers += 1;
                                    st.p2p += 1;
                                }
                                busy.insert(a.clone());
                                let name = format!("P2P {}", peer.addr.ip());
                                sc.spawn(move || {
                                    worker(pool_r, name, Box::new(peer));
                                    pool_r.s.lock().unwrap().p2p -= 1;
                                });
                            }
                            _ => {
                                resting.insert(a, Instant::now());
                            }
                        },
                        None => {
                            // Everyone is busy or resting: free the busy list of peers that left.
                            busy.clear();
                            std::thread::sleep(Duration::from_secs(5));
                        }
                    }
                } else {
                    std::thread::sleep(Duration::from_secs(3));
                }
            }
        });
        // The ordered side, on this thread.
        let mut pre = Prefetched { pool: &pool, cache: HashMap::new(), p };
        let mut last_note = Instant::now();
        let mut stalls = 0;
        let (mut waited, mut stored) = (Duration::ZERO, Duration::ZERO);
        let res: Result<(), String> = loop {
            if stop.load(Ordering::Relaxed) {
                break Ok(());
            }
            let ours = node.chain.tip(c).ok().flatten().map(|t| t.height).unwrap_or(0);
            if ours + 10 >= top {
                break Ok(());
            }
            let t0 = Instant::now();
            if let Err(e) = syncer.sync_headers_once(&mut pre) {
                break Err(e.to_string());
            }
            let t1 = Instant::now();
            if let Err(e) = syncer.sync_blocks(&mut pre, 5000, 500) {
                break Err(e.to_string());
            }
            waited += t1 - t0;
            stored += t1.elapsed();
            // No progress for many rounds in a row: the sources' blocks don't link to ours.
            let now_tip = node.chain.tip(c).ok().flatten().map(|t| t.height).unwrap_or(0);
            if now_tip == ours {
                stalls += 1;
                if stalls > 20 {
                    break Err("blocks from the sources don't link to our chain".into());
                }
            } else {
                stalls = 0;
            }
            if last_note.elapsed() > Duration::from_secs(2) {
                let tot = (waited + stored).as_secs_f64().max(0.001);
                on_progress(format!("block {} of {top} · {} · waiting {:.0}% storing {:.0}%", node.chain.tip(c).ok().flatten().map(|t| t.height).unwrap_or(0), pool.summary(),
                    waited.as_secs_f64() * 100.0 / tot, stored.as_secs_f64() * 100.0 / tot));
                last_note = Instant::now();
            }
        };
        pool.stop.store(true, Ordering::Relaxed);
        pool.cv.notify_all();
        if let Err(e) = &res {
            on_progress(format!("parallel download stopped: {e}"));
        }
        res
    })?;
    let now = node.chain.tip(c).map_err(|e| e.to_string())?.map(|t| t.height).unwrap_or(0);
    Ok(now.saturating_sub(tip))
}
