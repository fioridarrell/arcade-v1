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
        format!("{} sources: {}", st.workers, v.iter().take(6).map(|(k, n)| format!("{k} {}%", **n * 100 / total)).collect::<Vec<_>>().join(" · "))
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
            if st.workers == 0 {
                return Err("every source stopped".into());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(format!("no source delivered block {want} in time"));
            }
            st = self.pool.cv.wait_timeout(st, deadline - now).unwrap().0;
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
                    break st.queue.pop_front().unwrap();
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
                *st.stats.entry(name.clone()).or_insert(0) += raws.len() as u64;
                st.ready.insert(job.start, (raws, name.clone()));
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

/// Feed the queue with block hashes ahead of the connector, from one coreshare arcade.
fn feeder(pool: &Pool, mut hs: crate::crsource::CoreShareSource) {
    while !pool.stop.load(Ordering::Relaxed) {
        let (next, want) = {
            let st = pool.s.lock().unwrap();
            (st.next, st.want)
        };
        if next > pool.top {
            return;
        }
        if next >= want + AHEAD {
            std::thread::sleep(Duration::from_millis(300));
            continue;
        }
        let n = (pool.top - next + 1).min(500);
        match hs.hashes(next, n) {
            Ok(hashes) => {
                let mut st = pool.s.lock().unwrap();
                for (i, chunk) in hashes.chunks(JOB as usize).enumerate() {
                    st.queue.push_back(Job { start: next + i as u32 * JOB, hashes: chunk.to_vec(), tries: 0 });
                }
                st.next = next + hashes.len() as u32;
                pool.cv.notify_all();
            }
            Err(_) => {
                std::thread::sleep(Duration::from_secs(3));
                // Try again (the connector times out if nothing comes).
            }
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
    let feeder_src = cr.iter().find_map(|o| crate::crsource::CoreShareSource::dial(node, &o.1, p).ok()).ok_or("no arcade offers hashes for this chain")?;
    let pool = Pool { s: Mutex::new(State { next: tip + 1, want: tip + 1, ..Default::default() }), cv: Condvar::new(), stop: AtomicBool::new(false), top };
    let short = |id: &str| id[..id.len().min(8)].to_string();
    let mut started = 0u32;
    std::thread::scope(|sc| {
        sc.spawn(|| feeder(&pool, feeder_src));
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
                started += 1;
            }
        }
        for a in node.coin_candidates(p).iter().take(8) {
            if started >= 12 || pool.s.lock().unwrap().stats.len() > 50 {
                break;
            }
            if let Ok(peer) = arcade_chain::p2p::Peer::connect(p, a, Duration::from_secs(8)) {
                if (peer.version.start_height.max(0) as u32) < tip + 10 {
                    continue;
                }
                pool.s.lock().unwrap().workers += 1;
                let name = format!("P2P {}", peer.addr.ip());
                let pool = &pool;
                sc.spawn(move || worker(pool, name, Box::new(peer)));
                started += 1;
                if pool.s.lock().unwrap().workers >= 3 + cr.len() {
                    break;
                }
            }
        }
        // The ordered side, on this thread.
        let mut pre = Prefetched { pool: &pool, cache: HashMap::new(), p };
        let mut last_note = Instant::now();
        let mut stalls = 0;
        let res: Result<(), String> = loop {
            if stop.load(Ordering::Relaxed) {
                break Ok(());
            }
            let ours = node.chain.tip(c).ok().flatten().map(|t| t.height).unwrap_or(0);
            if ours + 10 >= top {
                break Ok(());
            }
            match syncer.sync_headers_once(&mut pre) {
                Err(e) => break Err(e.to_string()),
                // Headers that don't link to ours (a source on a fork, or wrong hashes): stop and
                // let the one-source path take over from our tip.
                Ok(0) if node.chain.tip(c).ok().flatten().map(|t| t.height).unwrap_or(0) == ours => {
                    stalls += 1;
                    if stalls > 3 {
                        break Err("blocks from the sources don't link to our chain".into());
                    }
                }
                Ok(_) => stalls = 0,
            }
            if let Err(e) = syncer.sync_blocks(&mut pre, 5000, 500) {
                break Err(e.to_string());
            }
            if last_note.elapsed() > Duration::from_secs(2) {
                on_progress(format!("block {} of {top} · {}", node.chain.tip(c).ok().flatten().map(|t| t.height).unwrap_or(0), pool.summary()));
                last_note = Instant::now();
            }
        };
        pool.stop.store(true, Ordering::Relaxed);
        pool.cv.notify_all();
        if let Err(e) = &res {
            on_progress(format!("parallel download stopped: {e}"));
        }
    });
    let now = node.chain.tip(c).map_err(|e| e.to_string())?.map(|t| t.height).unwrap_or(0);
    Ok(now.saturating_sub(tip))
}
