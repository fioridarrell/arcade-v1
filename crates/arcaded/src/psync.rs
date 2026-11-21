//! Parallel block download: many sources at once for one chain, connected strictly in order.
//!
//! * A hash feeder asks one arcade for block hashes ahead of our tip (getblockhash, 500 a
//!   call) and cuts them into jobs of JOB blocks.
//! * One worker per source takes jobs from a shared queue and fetches those blocks: every
//!   arcade on the mesh that offers the chain (existing arcades' Core nodes over coreshare,
//!   V1.0 arcades over bk) plus a few of the coin's P2P peers. A fast source takes more jobs;
//!   a source that fails gives its job back and, after repeated failures, stops.
//! * Store-only mode ([`run_store`], init installs, docs/INITSYNC.md): the jobs are the blocks
//!   our checked headers name that we don't have, done in ANY order; each worker stores what it
//!   fetched itself (ChainDb::init_store: the block asked for, merkle root, witness commitment),
//!   so the checks run on every worker at once and nothing is connected or indexed. A block
//!   refused goes back in the queue for another source.
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
use arcade_chaindb::ChainDb;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

const JOB: u32 = 250;
/// P2P peers kept working at once (the coin's network is the other side of "mesh + P2P").
const P2P_WANT: usize = 24;
/// A P2P peer delivering under this (bytes/s) is replaced.
const SLOW_BPS: f64 = 150_000.0;
/// How far ahead of the connected tip hashes are fetched: at most this many blocks, and no
/// more than READY_BYTES of finished blocks waiting (old blocks are tiny, new ones are not).
const AHEAD: u32 = 100_000;
const READY_BYTES: usize = 384 << 20;
/// An idle source also fetches the oldest job still in flight once it is this old.
const STEAL_AFTER: Duration = Duration::from_millis(1500);
/// The job the connector waits on, still not delivered this long after its last copy went
/// out: one more source fetches it too (first copy wins), up to MAX_COPIES. Two slow or stuck
/// sources used to hold every later block back for many minutes (Dogecoin, 130 MB jobs).
const RECOPY_AFTER: Duration = Duration::from_secs(45);
/// Worst case that is ~3 extra downloads of the job (Dogecoin's ~130 MB jobs: ~390 MB).
const MAX_COPIES: u8 = 4;

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
    /// Bytes in `ready`.
    ready_bytes: usize,
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
    /// start -> (hashes, first sent, (copies out, last copy sent))
    inflight: BTreeMap<u32, (Vec<Hash256>, Instant, (u8, Instant))>,
    /// Recent average block size (bytes; 0 = not known yet), to size jobs by bytes.
    avg_block: usize,
    /// P2P peers (by IP) found too slow: the recruiter rests them.
    slow: std::collections::HashSet<String>,
    /// Store-only mode: jobs stored (a late copy of one is dropped), blocks stored and refused,
    /// the next id for a job fetched again, and an error that ends the round.
    taken: std::collections::HashSet<u32>,
    stored: u64,
    refused: u64,
    next_id: u32,
    failed: Option<String>,
    /// Store-only mode: sources that already failed a job (refused it, left blocks out, or
    /// failed to send): the job goes to another source; one that failed every job left steps
    /// aside.
    tried: HashMap<u32, std::collections::HashSet<String>>,
    /// Store-only mode: bytes of the blocks stored.
    bytes: u64,
}

/// What a store-only round did (run_store).
#[derive(Debug, Default)]
pub struct Round {
    /// Blocks stored, and their bytes.
    pub stored: u64,
    pub bytes: u64,
    /// Why it ended early (None: every block stored, or stopped).
    pub error: Option<String>,
}

/// Bytes one job should carry: one source must finish it well within the connector's wait,
/// whether blocks are 300 bytes or 1 MB (250 small blocks, or ~30 full Doginals-era ones).
const JOB_BYTES: usize = 48 << 20;

/// Raw block bytes a storing round takes at most.
const ROUND_BYTES: usize = 200 << 20;

/// Blocks per job for the recent block size (a small first job measures it).
fn job_len(avg_block: usize) -> u32 {
    if avg_block == 0 {
        return 100;
    }
    ((JOB_BYTES / avg_block.max(1)) as u32).clamp(10, JOB)
}

pub struct Pool {
    s: Mutex<State>,
    cv: Condvar,
    stop: AtomicBool,
    top: u32,
    /// Store-only mode: jobs in any order, each worker stores its own (run_store).
    any_order: bool,
}

impl Pool {
    fn put_back(&self, job: Job) {
        let mut st = self.s.lock().unwrap();
        // The job goes back to the front and the next worker keeps its in-flight entry (and copy
        // count): a failed copy is replaced one for one, so the count stays right.
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
        let head = st.inflight.iter().find(|(s, _)| **s >= st.want).map(|(s, (_, since, d))| format!(" · next job +{} for {:.1}s{}", s - st.want, since.elapsed().as_secs_f32(), if d.0 > 1 { format!(" ({} copies)", d.0) } else { String::new() })).unwrap_or_default();
        format!("{} sources ({} P2P) [queue {} · fetching {} · done {} · {} MB ready · hashes to {}{head}]: {}", st.workers, st.p2p, st.queue.len(), st.inflight.len(), st.ready.len(), st.ready_bytes >> 20, st.next, v.iter().take(6).map(|(k, n)| format!("{k} {}%", **n * 100 / total)).collect::<Vec<_>>().join(" · "))
    }
}

/// The ordered side: what the Syncer reads. Headers come from the blocks themselves.
pub struct Prefetched<'p> {
    pool: &'p Pool,
    cache: HashMap<Hash256, Vec<u8>>,
    p: &'static ChainParams,
    /// Headers of jobs pulled but not yet handed to the syncer, in order. A job pulled early
    /// (because a block batch needed it) must still give its headers, or nothing after links.
    heads: VecDeque<HeaderEntry>,
    last_diag: Instant,
    /// The chain's stop: a wait for blocks ends with it (the chain is being restarted, e.g.
    /// for an index added, and a new thread takes over).
    stop: &'p AtomicBool,
}

impl Prefetched<'_> {
    fn cached_bytes(&self) -> usize {
        self.cache.values().map(|r| r.len()).sum()
    }

    /// Whether the next job in height order is already finished.
    fn ready_next(&self) -> bool {
        let st = self.pool.s.lock().unwrap();
        st.ready.contains_key(&st.want)
    }

    /// The next finished job in height order (waits up to `wait`): its headers, in order.
    fn pull(&mut self, wait: Duration) -> Result<usize, String> {
        let deadline = Instant::now() + wait;
        let mut st = self.pool.s.lock().unwrap();
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err("the chain is stopping".into());
            }
            let want = st.want;
            if let Some((raws, _src)) = st.ready.remove(&want) {
                st.ready_bytes -= raws.iter().map(|r| r.len()).sum::<usize>();
                st.want = want + raws.len() as u32;
                self.pool.cv.notify_all();
                drop(st);
                let n = raws.len();
                for raw in raws {
                    let b = Block::parse_with(&raw, &self.p.format).map_err(|e| e.to_string())?;
                    self.heads.push_back(HeaderEntry { header: b.header, auxpow: b.auxpow.clone() });
                    self.cache.insert(b.id, raw);
                }
                return Ok(n);
            }
            if std::env::var_os("ARCADE_SYNC_TIMES").is_some() && self.last_diag.elapsed() > Duration::from_secs(30) {
                self.last_diag = Instant::now();
                let first_ready = st.ready.keys().next().copied();
                let fl = st.inflight.get(&want).map(|(h, since, d)| format!("{} blocks for {:.0}s{}", h.len(), since.elapsed().as_secs_f64(), if d.0 > 1 { format!(" ({} copies)", d.0) } else { String::new() }));
                let queued = st.queue.iter().any(|j| j.start == want);
                eprintln!(
                    "  psync waiting for {want}: in flight {} · queued {queued} · ready {} jobs from {:?} ({} MB) · queue {} (front {:?}) · workers {} (p2p {})",
                    fl.unwrap_or_else(|| "no".into()), st.ready.len(), first_ready, st.ready_bytes >> 20, st.queue.len(), st.queue.front().map(|j| j.start), st.workers, st.p2p
                );
            }
            if st.workers == 0 && Instant::now() + Duration::from_secs(360) > deadline {
                // Nobody working for a while (the recruiter had 30 s to bring someone).
                return Err("every source stopped".into());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(format!("no source delivered block {want} in time"));
            }
            // The next job in line is slow: let another source fetch it too (first copy wins).
            // Still not here long after the last copy: one more, up to MAX_COPIES.
            if let Some((hashes, since, copies)) = st.inflight.get_mut(&want) {
                let again = if copies.0 == 1 { since.elapsed() > Duration::from_secs(5) } else { copies.1.elapsed() > RECOPY_AFTER };
                if again && copies.0 < MAX_COPIES {
                    *copies = (copies.0 + 1, Instant::now());
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
        // Every job up to the top pulled already: nothing more is coming (the workers have
        // stopped), and the blocks still in the cache are connected by sync_blocks. Pulling
        // here would end the round with "every source stopped" and drop those blocks.
        let exhausted = self.pool.s.lock().unwrap().want > self.pool.top;
        if exhausted {
            let n = self.heads.len().min(2000);
            return Ok(self.heads.drain(..n).collect());
        }
        if self.heads.is_empty() {
            self.pull(Duration::from_secs(420))?;
        }
        // Then every job already finished in order, so a round stores a big batch even when
        // jobs are small (one job a round made many tiny commits), up to ROUND_BYTES of blocks:
        // full blocks are ~750 KB, and a round's blocks are held raw and parsed twice over.
        while self.heads.len() < 2000 && self.cached_bytes() < ROUND_BYTES && self.ready_next() {
            self.pull(Duration::from_secs(1))?;
        }
        // Everything pulled so far, in order (the syncer takes up to 2,000 per round).
        let n = self.heads.len().min(2000);
        Ok(self.heads.drain(..n).collect())
    }

    fn get_blocks(&mut self, hashes: &[Hash256]) -> Result<Vec<Vec<u8>>, String> {
        let mut guard = 0;
        while hashes.iter().any(|h| !self.cache.contains_key(h)) {
            if self.pool.s.lock().unwrap().want > self.pool.top {
                return Err("blocks asked for are past what this round fetched".into());
            }
            self.pull(Duration::from_secs(420))?;
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

/// `store`: store-only mode (the node and chain to store into).
fn worker(pool: &Pool, name: String, mut src: Box<dyn BlockSource + Send + '_>, store: Option<(&ChainDb, &'static ChainParams)>) {
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
                // The memory bound never holds back the jobs the connector needs next (a job
                // put back after a failure goes to the front): those always go out.
                let ahead_ok = if pool.any_order {
                    !st.queue.is_empty()
                } else {
                    st.queue.front().is_some_and(|j| j.start < st.want + AHEAD && (st.ready_bytes < READY_BYTES || j.start < st.want + 8 * JOB))
                };
                // First help with the oldest jobs still in flight: the connector waits on them
                // while later ones pile up done (first copy wins).
                let want = st.want;
                // (In any order, only once nothing is left in the queue: the last jobs.)
                let steal_ok = !pool.any_order || st.queue.is_empty();
                let steal = st.inflight.iter_mut().filter(|_| steal_ok).filter(|(s, _)| **s >= want).take(2).find(|(_, (_, since, copies))| copies.0 == 1 && since.elapsed() > STEAL_AFTER).map(|(s, (h, _, d))| {
                    *d = (2, Instant::now());
                    Job { start: *s, hashes: h.clone(), tries: 0 }
                });
                if let Some(j) = steal {
                    break j;
                }
                if ahead_ok {
                    let mut j = if pool.any_order {
                        // Not a job this source failed already (another one fetches it); with
                        // only those left, this source steps aside.
                        match st.queue.iter().position(|j| !st.tried.get(&j.start).is_some_and(|t| t.contains(&name))) {
                            Some(i) => st.queue.remove(i).unwrap(),
                            None => {
                                st.workers -= 1;
                                pool.cv.notify_all();
                                return;
                            }
                        }
                    } else {
                        st.queue.pop_front().unwrap()
                    };
                    // A doubled job that another source already delivered: skip it.
                    if j.start < st.want || st.ready.contains_key(&j.start) || st.taken.contains(&j.start) {
                        continue;
                    }
                    // Jobs are cut ahead of time; blocks may have grown since: take a job's
                    // worth for today's block size and put the rest back at the front.
                    let len = job_len(st.avg_block) as usize;
                    if j.hashes.len() > len && !st.inflight.contains_key(&j.start) {
                        let rest = j.hashes.split_off(len);
                        st.queue.push_front(Job { start: j.start + len as u32, hashes: rest, tries: 0 });
                    }
                    st.inflight.entry(j.start).or_insert((j.hashes.clone(), Instant::now(), (1, Instant::now())));
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
        let t_job = Instant::now();
        if let Some((db, p)) = store {
            // Store-only: this worker checks and stores what it fetched (any order).
            let got = src.get_blocks(&job.hashes);
            {
                let st = pool.s.lock().unwrap();
                // The round is over (stopped, or the chain removed): nothing more is written.
                if pool.stop.load(Ordering::Relaxed) {
                    drop(st);
                    break;
                }
                if st.taken.contains(&job.start) {
                    continue;
                }
            }
            let res = match got {
                Ok(raws) => db.init_store(p, &job.hashes, &raws).map(|r| (r, raws.iter().map(|x| x.len()).sum::<usize>(), raws.len())).map_err(|e| Some(e.to_string())),
                Err(_) => Err(None),
            };
            let mut st = pool.s.lock().unwrap();
            match res {
                Ok((r, bytes, sent)) => {
                    st.inflight.remove(&job.start);
                    st.taken.insert(job.start);
                    st.stored += r.stored as u64;
                    st.bytes += bytes as u64 * r.stored as u64 / sent.max(1) as u64;
                    st.refused += r.refused.len() as u64;
                    if r.stored > 0 {
                        let per = bytes / r.stored.max(1);
                        st.avg_block = if st.avg_block == 0 { per } else { (st.avg_block * 4 + per) / 5 };
                        *st.stats.entry(name.clone()).or_insert(0) += r.stored as u64;
                    }
                    let mut tried = st.tried.remove(&job.start).unwrap_or_default();
                    if !r.again.is_empty() {
                        // Fetched again by another source (a new job id, with who failed it).
                        tried.insert(name.clone());
                        let id = st.next_id;
                        st.next_id += r.again.len() as u32;
                        st.tried.insert(id, tried);
                        st.queue.push_front(Job { start: id, hashes: r.again, tries: job.tries + 1 });
                        // A source that sends damaged or other blocks, or leaves some out, steps
                        // aside after a few such batches in a row.
                        fails += 1;
                    } else {
                        fails = 0;
                    }
                    pool.cv.notify_all();
                    if fails >= 3 {
                        break;
                    }
                }
                Err(Some(e)) => {
                    // The store itself failed (the disk): no other source helps.
                    st.failed = Some(e);
                    pool.cv.notify_all();
                    break;
                }
                Err(None) => {
                    st.tried.entry(job.start).or_default().insert(name.clone());
                    drop(st);
                    pool.put_back(job);
                    fails += 1;
                    if fails >= 3 {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
            continue;
        }
        match src.get_blocks(&job.hashes) {
            Ok(raws) if raws.len() == job.hashes.len() => {
                fails = 0;
                // A slow P2P peer holds back the blocks the connector waits on: after a sizeable
                // job delivered at under SLOW_BPS, this worker steps aside (its peer rests) so
                // the recruiter brings a faster one. Arcades on the mesh always stay.
                let bytes: usize = raws.iter().map(|r| r.len()).sum();
                let secs = t_job.elapsed().as_secs_f64().max(0.001);
                let slow = name.starts_with("P2P ") && bytes >= 4 << 20 && (bytes as f64 / secs) < SLOW_BPS;
                let mut st = pool.s.lock().unwrap();
                st.inflight.remove(&job.start);
                let per = raws.iter().map(|r| r.len()).sum::<usize>() / raws.len().max(1);
                st.avg_block = if st.avg_block == 0 { per } else { (st.avg_block * 4 + per) / 5 };
                // First copy wins; a late duplicate is dropped.
                if job.start >= st.want && !st.ready.contains_key(&job.start) {
                    *st.stats.entry(name.clone()).or_insert(0) += raws.len() as u64;
                    st.ready_bytes += raws.iter().map(|r| r.len()).sum::<usize>();
                    st.ready.insert(job.start, (raws, name.clone()));
                }
                pool.cv.notify_all();
                if slow {
                    st.slow.insert(name.trim_start_matches("P2P ").to_string());
                    drop(st);
                    break;
                }
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
fn feeder(node: &Node, pool: &Pool, mut hs: HashSource, mut last: Hash256) {
    let mut bad = 0;
    let mut tried: Vec<String> = Vec::new();
    while !pool.stop.load(Ordering::Relaxed) {
        let (next, want, full) = {
            let st = pool.s.lock().unwrap();
            (st.next, st.want, st.ready_bytes >= READY_BYTES)
        };
        if next > pool.top {
            return;
        }
        if next >= want + AHEAD || full {
            std::thread::sleep(Duration::from_millis(200));
            continue;
        }
        let got: Result<Vec<Hash256>, String> = match &mut hs {
            HashSource::P2p(peer, p) => peer.get_headers(&[last]).map_err(|e| e.to_string()).and_then(|es| {
                // The headers must follow our last hash, each building on the one before:
                // a peer answering from somewhere else would put blocks at the wrong heights.
                let mut prev = last;
                let mut out = Vec::with_capacity(es.len());
                for e in &es {
                    if e.header.prev != prev {
                        return Err("headers from this peer don't follow ours".into());
                    }
                    prev = p.block_id(&e.header);
                    out.push(prev);
                }
                Ok(out)
            }),
            HashSource::Core(c) => c.hashes(next, (pool.top - next + 1).min(500)),
        };
        match got {
            Ok(hashes) if !hashes.is_empty() => {
                bad = 0;
                let hashes: Vec<Hash256> = hashes.into_iter().take((pool.top - next + 1) as usize).collect();
                let mut st = pool.s.lock().unwrap();
                let len = job_len(st.avg_block) as usize;
                for (i, chunk) in hashes.chunks(len).enumerate() {
                    st.queue.push_back(Job { start: next + (i * len) as u32, hashes: chunk.to_vec(), tries: 0 });
                }
                st.next = next + hashes.len() as u32;
                last = *hashes.last().unwrap();
                pool.cv.notify_all();
            }
            _ => {
                // Try another source for the hashes ahead.
                bad += 1;
                if bad >= 2 {
                    if let Some(np) = recruit_peer(node, p_of(&hs), &tried) {
                        tried.push(np.addr.to_string());
                        hs = HashSource::P2p(np, p_of(&hs));
                        bad = 0;
                    }
                }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
}

fn p_of(hs: &HashSource) -> &'static ChainParams {
    match hs {
        HashSource::P2p(_, p) => p,
        HashSource::Core(c) => c.params(),
    }
}

/// A P2P peer not tried yet, for the hash feeder.
fn recruit_peer(node: &Node, p: &'static ChainParams, tried: &[String]) -> Option<arcade_chain::p2p::Peer> {
    node.coin_candidates(p).iter().filter(|a| !tried.contains(a)).take(6).find_map(|a| arcade_chain::p2p::Peer::connect(p, a, Duration::from_secs(6)).ok())
}

/// Every arcade offering the chain (at `min_h` or above), dialed: (worker name, source).
fn dial_offers<'a>(node: &'a Node, p: &'static ChainParams, offers: &[(String, String, &'static str, u64)], min_h: u32) -> Vec<(String, Box<dyn BlockSource + Send + 'a>)> {
    let short = |id: &str| id[..id.len().min(8)].to_string();
    let mut out = Vec::new();
    for (id, addr, kind, their) in offers.iter() {
        if (*their as u32) < min_h {
            continue;
        }
        let src: Option<Box<dyn BlockSource + Send>> = match *kind {
            "bk" => crate::meshsource::MeshSource::dial_chain(node, addr, p).ok().map(|s| Box::new(s) as Box<dyn BlockSource + Send>),
            _ => crate::crsource::CoreShareSource::dial(node, addr, p).ok().map(|s| Box::new(s) as Box<dyn BlockSource + Send>),
        };
        if let Some(src) = src {
            out.push((format!("arcade {}", short(id)), src));
        }
    }
    out
}

/// Every arcade offering the chain (at `min_h` or above), then P2P_WANT P2P peers kept working
/// (a peer that drops out is replaced; one that failed rests 10 minutes). `store`: the workers
/// store what they fetch (run_store) instead of handing it to the ordered side.
fn spawn_sources<'scope, 'env>(
    sc: &'scope std::thread::Scope<'scope, 'env>,
    node: &'env Node,
    p: &'static ChainParams,
    pool: &'env Pool,
    offers: &'env [(String, String, &'static str, u64)],
    min_h: u32,
    store: bool,
) {
    for (name, src) in dial_offers(node, p, offers, min_h) {
        pool.s.lock().unwrap().workers += 1;
        sc.spawn(move || worker(pool, name, src, store.then_some((&node.chain, p))));
    }
    spawn_recruiter(sc, node, p, pool, min_h, store);
}

/// P2P_WANT of the coin's P2P peers (at `min_h` or above) kept working for `pool`.
fn spawn_recruiter<'scope, 'env>(sc: &'scope std::thread::Scope<'scope, 'env>, node: &'env Node, p: &'static ChainParams, pool: &'env Pool, min_h: u32, store: bool) {
    // P2P peers: keep P2P_WANT of them working; when one drops out, recruit a fresh one.
    // A peer that failed rests for 10 minutes before it is tried again.
    let pool_r = pool;
    sc.spawn(move || {
        let mut resting: HashMap<String, Instant> = HashMap::new();
        let mut busy: std::collections::HashSet<String> = Default::default();
        while !pool_r.stop.load(Ordering::Relaxed) {
            let p2p_now = pool_r.s.lock().unwrap().p2p;
            if p2p_now < P2P_WANT {
                resting.retain(|_, t| t.elapsed() < Duration::from_secs(600));
                // Peers that stepped aside for being slow rest too (and leave the busy list).
                let slow: Vec<String> = pool_r.s.lock().unwrap().slow.drain().collect();
                let ip_of = |a: &str| a.rsplit_once(':').map(|x| x.0.trim_matches(|c| c == '[' || c == ']').to_string()).unwrap_or_default();
                for a in busy.iter().filter(|a| slow.contains(&ip_of(a))).cloned().collect::<Vec<_>>() {
                    busy.remove(&a);
                    resting.insert(a, Instant::now());
                }
                let cand = node.coin_candidates(p).into_iter().find(|a| !resting.contains_key(a) && !busy.contains(a));
                match cand {
                    Some(a) => match arcade_chain::p2p::Peer::connect(p, &a, Duration::from_secs(8)) {
                        Ok(peer) if (peer.version.start_height.max(0) as u32) >= min_h => {
                            node.note_coin_peer(p.net, &a);
                            {
                                let mut st = pool_r.s.lock().unwrap();
                                st.workers += 1;
                                st.p2p += 1;
                            }
                            busy.insert(a.clone());
                            let name = format!("P2P {}", peer.addr.ip());
                            sc.spawn(move || {
                                worker(pool_r, name, Box::new(peer), store.then_some((&node.chain, p)));
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
    syncer.known_top.set(syncer.known_top.get().max(top));
    let pool = Pool { s: Mutex::new(State { next: tip + 1, want: tip + 1, ..Default::default() }), cv: Condvar::new(), stop: AtomicBool::new(false), top, any_order: false };
    std::thread::scope(|sc| -> Result<(), String> {
        sc.spawn(|| feeder(node, &pool, feeder_src, tip_hash));
        spawn_sources(sc, node, p, &pool, &offers, tip + 10, false);
        // The ordered side, on this thread.
        let mut pre = Prefetched { pool: &pool, cache: HashMap::new(), p, heads: VecDeque::new(), last_diag: Instant::now(), stop };
        let mut last_note = Instant::now();
        let mut stalls = 0;
        let (mut waited, mut stored) = (Duration::ZERO, Duration::ZERO);
        // Show the sources at once (the first round can take minutes on full blocks).
        std::thread::sleep(Duration::from_secs(3));
        on_progress(format!("block {} of {top} · {} · starting", node.chain.tip(c).ok().flatten().map(|t| t.height).unwrap_or(0), pool.summary()));
        let res: Result<(), String> = loop {
            // A copy is being taken in (crate::coreboot): stand still.
            if stop.load(Ordering::Relaxed) || crate::coreboot::held(p.key) {
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
            // 250-block chunks: sync_blocks checks and warms the next chunk while it writes
            // this one. One 1000-block chunk did the checks and the writes one after the other,
            // and its warmed pages had gone cold by the write (Litecoin: 2.8 blk/s against 5–6).
            if let Err(e) = syncer.sync_blocks(&mut pre, 1000, 250) {
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
                // A rolling share (about the last 20 s), not the average since the start.
                waited = waited.mul_f64(0.9);
                stored = stored.mul_f64(0.9);
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

/// Store-only download (init installs, docs/INITSYNC.md): every block of `hashes` (our checked
/// headers' blocks not stored yet), from every source at once, in any order; each worker
/// checks and stores its own job (ChainDb::init_store) and a refused block goes back in the
/// queue for another source. Nothing is connected. `min_h`: the lowest height asked for (a
/// source must be at least there). Returns blocks stored; some may still be missing after an
/// error or a stop (the caller looks again: ChainDb::init_missing).
pub fn run_store(node: &Node, p: &'static ChainParams, hashes: &[Hash256], min_h: u32, on_progress: &dyn Fn(String), stop: &AtomicBool) -> Round {
    if hashes.is_empty() {
        return Round::default();
    }
    let offers = node.offers_for(p.key);
    let arcades = dial_offers(node, p, &offers, min_h);
    store_round(&node.chain, p, hashes, arcades, Some((node, min_h)), Duration::from_secs(360), on_progress, stop)
}

/// A store-only round over `sources` (and, with `recruit`, the coin's P2P peers kept working):
/// see [`run_store`]. Ends when every block was stored, at a stop, when the store fails, or
/// when no source worked for `idle`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn store_round<'a>(
    db: &'a ChainDb,
    p: &'static ChainParams,
    hashes: &[Hash256],
    sources: Vec<(String, Box<dyn BlockSource + Send + 'a>)>,
    recruit: Option<(&'a Node, u32)>,
    idle: Duration,
    on_progress: &dyn Fn(String),
    stop: &AtomicBool,
) -> Round {
    if hashes.is_empty() {
        return Round::default();
    }
    let n = hashes.len() as u32;
    let mut st = State { next_id: n, ..Default::default() };
    // Jobs by position in `hashes` (a job split later keeps its range); refetches get ids past n.
    for (i, chunk) in hashes.chunks(JOB as usize).enumerate() {
        st.queue.push_back(Job { start: i as u32 * JOB, hashes: chunk.to_vec(), tries: 0 });
    }
    let pool = Pool { s: Mutex::new(st), cv: Condvar::new(), stop: AtomicBool::new(false), top: n, any_order: true };
    let t0 = Instant::now();
    let res = std::thread::scope(|sc| -> Result<(), String> {
        let pool = &pool;
        for (name, src) in sources {
            pool.s.lock().unwrap().workers += 1;
            sc.spawn(move || worker(pool, name, src, Some((db, p))));
        }
        if let Some((node, min_h)) = recruit {
            spawn_recruiter(sc, node, p, pool, min_h, true);
        }
        let mut last_note = Instant::now() - Duration::from_secs(10);
        let mut idle_since: Option<Instant> = None;
        let r = loop {
            if stop.load(Ordering::Relaxed) {
                break Ok(());
            }
            {
                let st = pool.s.lock().unwrap();
                if let Some(e) = &st.failed {
                    break Err(e.clone());
                }
                if st.queue.is_empty() && st.inflight.is_empty() {
                    break Ok(());
                }
                // Nobody working for a while (the recruiter keeps trying meanwhile).
                if st.workers == 0 {
                    if idle_since.get_or_insert_with(Instant::now).elapsed() > idle {
                        break Err("every source stopped".into());
                    }
                } else {
                    idle_since = None;
                }
            }
            if last_note.elapsed() > Duration::from_secs(2) {
                let (stored, refused) = { let st = pool.s.lock().unwrap(); (st.stored, st.refused) };
                let mb_s = pool.s.lock().unwrap().avg_block as f64 * stored as f64 / t0.elapsed().as_secs_f64().max(1.0) / 1e6;
                on_progress(format!("stored {stored} of {n} blocks{} · ~{mb_s:.0} MB/s · {}", if refused > 0 { format!(" ({refused} refused and fetched again)") } else { String::new() }, pool.summary()));
                last_note = Instant::now();
            }
            let st = pool.s.lock().unwrap();
            drop(pool.cv.wait_timeout(st, Duration::from_millis(500)).unwrap());
        };
        pool.stop.store(true, Ordering::Relaxed);
        pool.cv.notify_all();
        r
    });
    let st = pool.s.lock().unwrap();
    Round { stored: st.stored, bytes: st.bytes, error: res.err() }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use arcade_chain::encode::write_varint;
    use arcade_chain::hash::merkle_root;
    use arcade_chain::pow::{meets_target, try_pow_hash, PowAlgo};
    use arcade_chain::{Header, Tx};

    const EASY: u32 = 0x207fffff;

    pub(crate) fn mine(prev: Hash256, tag: u32) -> Block {
        let mut o = 1i32.to_le_bytes().to_vec();
        write_varint(&mut o, 1);
        o.extend_from_slice(&[0; 32]);
        o.extend_from_slice(&u32::MAX.to_le_bytes());
        write_varint(&mut o, 4);
        o.extend_from_slice(&tag.to_le_bytes());
        o.extend_from_slice(&u32::MAX.to_le_bytes());
        write_varint(&mut o, 1);
        o.extend_from_slice(&50u64.to_le_bytes());
        write_varint(&mut o, 1);
        o.push(0x51);
        o.extend_from_slice(&0u32.to_le_bytes());
        let cb = Tx::parse(&o).unwrap();
        let mut header = Header { version: 1, prev, merkle_root: merkle_root(&[cb.txid]), time: 1_600_000_000 + 60 * tag, bits: EASY, nonce: 0 };
        while try_pow_hash(PowAlgo::Sha256d, &header.encode()).is_some_and(|h| !meets_target(&h, EASY)) {
            header.nonce += 1;
        }
        let mut raw = header.encode().to_vec();
        write_varint(&mut raw, 1);
        raw.extend_from_slice(&cb.raw);
        Block::parse(&raw).unwrap()
    }

    /// A source: honest, mute (sends no blocks) or one that damages every block.
    #[derive(Clone)]
    pub(crate) struct Src {
        pub(crate) blocks: Vec<Block>,
        pub(crate) kind: u8,
        pub(crate) asked: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl BlockSource for Src {
        fn get_headers(&mut self, locator: &[Hash256]) -> Result<Vec<HeaderEntry>, String> {
            let from = locator.iter().find_map(|l| self.blocks.iter().position(|b| b.hash() == *l)).map(|i| i + 1).unwrap_or(0);
            Ok(self.blocks[from..].iter().map(|b| HeaderEntry { header: b.header, auxpow: None }).collect())
        }
        fn get_blocks(&mut self, hashes: &[Hash256]) -> Result<Vec<Vec<u8>>, String> {
            self.asked.fetch_add(hashes.len(), Ordering::Relaxed);
            match self.kind {
                1 => Ok(vec![]),
                _ => Ok(hashes
                    .iter()
                    .filter_map(|h| self.blocks.iter().find(|b| b.hash() == *h))
                    .map(|b| {
                        let mut raw = b.encode();
                        if self.kind == 2 {
                            let n = raw.len();
                            raw[n - 5] ^= 1;
                        }
                        raw
                    })
                    .collect()),
            }
        }
        fn describe(&self) -> String {
            "test".into()
        }
    }

    /// A made-up chain of `n` blocks on genesis, and params for it (no rules, easy work).
    pub(crate) fn test_chain(n: u32) -> (&'static ChainParams, Vec<Block>) {
        let mut blocks = vec![mine(Hash256::ZERO, 0)];
        for h in 1..=n {
            let prev = blocks.last().unwrap().hash();
            blocks.push(mine(prev, h));
        }
        let mut p = arcade_chain::params::BITCOIN_MAIN.clone();
        p.value_rules = None;
        p.block_rules = None;
        p.genesis = Box::leak(blocks[0].hash().to_hex().into_boxed_str());
        p.genesis_header = "";
        p.pow_limit_bits = EASY;
        p.checkpoints = &[];
        p.min_chain_work = "";
        (Box::leak(Box::new(p)), blocks)
    }

    fn setup(n: u32) -> (tempfile::TempDir, ChainDb, &'static ChainParams, Vec<Block>) {
        let (p, blocks) = test_chain(n);
        let d = tempfile::tempdir().unwrap();
        let db = ChainDb::open(d.path().join("chain.mdbx")).unwrap();
        let mut src = Src { blocks: blocks.clone(), kind: 0, asked: Default::default() };
        let s = Syncer::new(&db, p);
        s.ensure_genesis(&mut src).unwrap();
        while s.sync_headers_once(&mut src).unwrap() > 0 {}
        (d, db, p, blocks)
    }

    pub(crate) fn src(blocks: &[Block], kind: u8) -> (Src, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let s = Src { blocks: blocks.to_vec(), kind, asked: Default::default() };
        let a = s.asked.clone();
        (s, a)
    }

    #[test]
    fn a_store_round_gets_past_mute_and_damaging_sources() {
        let (_d, db, p, blocks) = setup(1200);
        let missing = db.init_missing(p.chain_no).unwrap();
        assert_eq!(missing.len(), 1200);
        let (good, _) = src(&blocks, 0);
        let (mute, mute_asked) = src(&blocks, 1);
        let (bad, bad_asked) = src(&blocks, 2);
        let sources: Vec<(String, Box<dyn BlockSource + Send>)> = vec![("mute".into(), Box::new(mute)), ("bad".into(), Box::new(bad)), ("good".into(), Box::new(good))];
        let r = store_round(&db, p, &missing, sources, None, Duration::from_secs(5), &|_| {}, &AtomicBool::new(false));
        assert!(r.error.is_none(), "{:?}", r.error);
        assert_eq!(r.stored, 1200);
        assert!(r.bytes >= 1200 * 80, "{}", r.bytes);
        assert!(db.init_missing(p.chain_no).unwrap().is_empty());
        for b in &blocks[1..] {
            assert_eq!(db.block_raw(p.chain_no, &b.hash()).unwrap(), Some(b.encode()));
        }
        // Each bad source stepped aside after a few batches (it never stalls the round).
        assert!(mute_asked.load(Ordering::Relaxed) <= 3 * JOB as usize, "{}", mute_asked.load(Ordering::Relaxed));
        assert!(bad_asked.load(Ordering::Relaxed) <= 3 * JOB as usize, "{}", bad_asked.load(Ordering::Relaxed));
    }

    #[test]
    fn a_store_round_of_mute_sources_ends_instead_of_spinning() {
        let (_d, db, p, blocks) = setup(300);
        let missing = db.init_missing(p.chain_no).unwrap();
        let sources: Vec<(String, Box<dyn BlockSource + Send>)> = (0..3).map(|i| (format!("mute{i}"), Box::new(src(&blocks, 1).0) as Box<dyn BlockSource + Send>)).collect();
        let t0 = Instant::now();
        let r = store_round(&db, p, &missing, sources, None, Duration::from_secs(1), &|_| {}, &AtomicBool::new(false));
        assert_eq!(r.error.as_deref(), Some("every source stopped"));
        assert!(t0.elapsed() < Duration::from_secs(60));
        assert_eq!(db.init_missing(p.chain_no).unwrap().len(), 300);
        // A stop ends a round at once, with nothing written after it.
        let (good, _) = src(&blocks, 0);
        let stop = AtomicBool::new(true);
        let r = store_round(&db, p, &missing, vec![("good".into(), Box::new(good) as Box<dyn BlockSource + Send>)], None, Duration::from_secs(1), &|_| {}, &stop);
        assert!(r.error.is_none());
        assert_eq!(r.stored, db.init_missing(p.chain_no).map(|m| 300 - m.len() as u64).unwrap());
    }
}
