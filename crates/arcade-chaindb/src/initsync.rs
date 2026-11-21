//! Init installs (`arcade chain add <key> --init`, docs/INITSYNC.md): a big chain stored first
//! and built afterwards, in bulk passes, instead of connected block by block.
//!
//! STATE "init" holds the phase; each phase picks up where it was after a restart:
//!
//! 1. **headers**: headers from several sources (checked as any sync checks them: links, proof
//!    of work, difficulty, time). Done once at least [`MIN_AGREE`] independent sources agree on
//!    our best header (they have it as their tip, or extend it) and its total work is at least
//!    the chain's `min_chain_work`.
//! 2. **blocks**: every block on the best header chain that we don't have yet, fetched in any
//!    order from any source and STORED ONLY ([`ChainDb::init_store`]): it must be the block we
//!    asked for, rebuild byte-identically, match its header's merkle root and, where SegWit is
//!    on, its witness commitment. No coins, no indexes. Then headers again: the two phases
//!    repeat until a headers round finds nothing new and no header lacks its block.
//! 3. **build** ([`ChainDb::init_build`]): the stored blocks join the main chain, the tx index
//!    is written in one sorted pass, the coin set is derived from the blocks (the re-check's
//!    Replay / DiskReplay) while the chain's rules are checked over them on every core
//!    (rules::replay), and the tip moves to the built height. The last `tail` blocks are left
//!    to the normal sync, which connects them with undo records (reorgs at the tip).
//! 4. **done**: the normal sync takes over; addresses and the protocol indexes are built from the
//!    stored blocks by their usual backfill paths.
//!
//! Until build is done the chain's tip stays where it was (genesis): it has no coins, no
//! balances, and is offered to no one.

use crate::coreimport::{check_stored_block, DiskReplay, Note, Replay, Sorted, Sorter, TRUSTED, VERIFIED};
use crate::db::*;
use crate::schema::*;
use crate::sync::{BlockSource, Syncer};
use arcade_chain::p2p::HeaderEntry;
use arcade_chain::params::ChainParams;
use arcade_chain::{Block, Hash256};
use arcade_store::key::be_u32;
use arcade_store::{Result, StoreError};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// STATE: the init phase (text: headers, blocks, build, done).
pub const INIT: &str = "init";
/// STATE: headers/blocks rounds so far (text).
pub const INIT_ROUND: &str = "init_round";
/// STATE: what the last headers round found, or the build's last step (text, for status).
pub const INIT_NOTE: &str = "init_note";
/// STATE: build steps finished (comma list: main, txs, coins, rules).
pub const INIT_BUILT: &str = "init_built";
/// STATE: the height and hash the build brings the tip to ("height:hash"), fixed when it starts.
pub const INIT_TO: &str = "init_to";
/// STATE: the rules check has passed every block up to this height (resume point).
pub const INIT_RULES_TO: &str = "init_rules_to";
/// STATE: blocks the rules refused once, dropped and fetched again (comma list of hashes): one
/// refused again is the chain's own.
pub const INIT_REFETCHED: &str = "init_refetched";
/// STATE: timing lines of the phases so far (newline separated, the last 40), comparable
/// between runs: blocks/s stored, tx index rows/s, coin derivation and rules blocks/s.
pub const INIT_TIMING: &str = "init_timing";
/// STATE: the block number under height 1's (be u32): a block at height h on the best header
/// chain is stored as block number base + h.
pub const INIT_B_BASE: &str = "init_b_base";

/// The block number an init install stores a block of height `h` (>= 1) at: base + h, so the
/// main chain's block numbers run in height order whatever order the blocks arrive in. The
/// base is fixed at the first block stored (the next free number, under it); the counter of
/// block numbers moves past every number handed out, so nothing else takes one. A number
/// already held by a block of the same height on another branch (the best header chain moved
/// between rounds) is freed: that block is off the chain an init install builds, its body is
/// dropped (its header stays; a normal sync fetches it again should it be needed).
pub(crate) fn init_slot(w: &arcade_store::WriteTxn, params: &ChainParams, h: u32) -> Result<u32> {
    let c = params.chain_no;
    if h == 0 {
        return Err(err("an init install doesn't store the genesis block"));
    }
    let counter = w.get(STATE, &k_state(c, "next_b"))?.map(|v| be_u32(&v, 0)).unwrap_or(1);
    let base = match w.get(STATE, &k_state(c, INIT_B_BASE))? {
        Some(v) => be_u32(&v, 0),
        None => {
            let base = counter.saturating_sub(1);
            w.put(STATE, &k_state(c, INIT_B_BASE), &base.to_be_bytes())?;
            base
        }
    };
    let b = base.checked_add(h).ok_or_else(|| err("block numbers run out"))?;
    if let Some(meta) = w.get(BLOCK_META, &k_block(c, b))? {
        let h80: [u8; 80] = meta.get(..80).and_then(|x| x.try_into().ok()).ok_or_else(|| err("short block meta"))?;
        let other = params.block_id(&arcade_chain::Header::parse(&h80));
        match header_rec(w, c, &other)? {
            Some(r) if r.height == h && r.b == b => drop_body_in(w, params, &other)?,
            _ => return Err(err(format!("block number {b} (height {h}) is held by a block not at that height; the init install can't number its blocks by height"))),
        }
    }
    if counter <= b {
        w.put(STATE, &k_state(c, "next_b"), &(b + 1).to_be_bytes())?;
    }
    Ok(b)
}

/// Forget a stored block's body (its rows, its tx index rows, its height row); its header stays.
fn drop_body_in(w: &arcade_store::WriteTxn, params: &ChainParams, hash: &Hash256) -> Result<()> {
    let c = params.chain_no;
    let Some(mut rec) = header_rec(w, c, hash)? else { return Ok(()) };
    if rec.b == 0 {
        return Ok(());
    }
    let b = rec.b;
    for (k, raw) in w.scan_prefix(BLOCK_TX, &k_block(c, b))? {
        let p = be_u32(&k, k.len() - 4);
        if let Ok(txid) = row_txid(&raw, params) {
            let mut bp = b.to_be_bytes().to_vec();
            bp.extend_from_slice(&p.to_be_bytes());
            w.del_dup(TXS, &k_txs(c, &txid.t12()), &bp)?;
        }
        w.del(BLOCK_TX, &k)?;
    }
    w.del(BLOCK_META, &k_block(c, b))?;
    if w.get(HEIGHT, &k_height(c, rec.height))?.is_some_and(|v| be_u32(&v, 32) == b) {
        w.del(HEIGHT, &k_height(c, rec.height))?;
    }
    rec.b = 0;
    w.put(HEADERS, &k_header(c, &hash.0), &rec.index_value())?;
    Ok(())
}

/// Independent sources that must agree on the best header.
pub const MIN_AGREE: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Headers,
    Blocks,
    Build,
    Done,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Headers => "headers",
            Phase::Blocks => "blocks",
            Phase::Build => "build",
            Phase::Done => "done",
        }
    }

    pub fn parse(s: &str) -> Option<Phase> {
        Some(match s {
            "headers" => Phase::Headers,
            "blocks" => Phase::Blocks,
            "build" => Phase::Build,
            "done" => Phase::Done,
            _ => return None,
        })
    }
}

fn err(e: impl std::fmt::Display) -> StoreError {
    StoreError::Other(e.to_string())
}

/// What a headers round found.
#[derive(Debug, Clone, Default)]
pub struct Agreement {
    pub best: Hash256,
    pub height: u32,
    /// New headers stored this round.
    pub new_headers: u32,
    /// Sources (by independence key) that have our best header as their tip or extend it.
    pub agree: Vec<String>,
    /// Sources that answered from another chain, and why the others didn't count.
    pub differ: Vec<String>,
    /// The best header's total work reaches the chain's `min_chain_work`.
    pub work_ok: bool,
}

impl Agreement {
    /// Enough sources agree and the work is there: the headers phase is done.
    pub fn done(&self) -> bool {
        self.agree.len() >= MIN_AGREE && self.work_ok
    }

    pub fn text(&self) -> String {
        format!(
            "best header {} at {} · {} of {} sources agree{} · work {}{}",
            short(&self.best), self.height, self.agree.len(), MIN_AGREE,
            if self.differ.is_empty() { String::new() } else { format!(" ({} don't: {})", self.differ.len(), self.differ.iter().take(4).cloned().collect::<Vec<_>>().join("; ")) },
            if self.work_ok { "enough" } else { "below the chain's minimum" },
            if self.new_headers > 0 { format!(" · {} new headers", self.new_headers) } else { String::new() }
        )
    }
}

fn short(h: &Hash256) -> String {
    let x = h.to_hex();
    x[..x.len().min(12)].to_string()
}

/// A source's answer to "headers after `best`'s parent": agreement means the first header it
/// sends is `best` itself (it has `best` on its chain, as its tip or under it). A source on
/// another chain sends another header there, or answers from its fork point or genesis; one
/// behind sends nothing. An empty answer never agrees: it proves nothing.
pub fn agrees(params: &ChainParams, best: &Hash256, reply: &[HeaderEntry]) -> bool {
    reply.first().is_some_and(|e| params.block_id(&e.header) == *best)
}

/// A source asked for headers with our own locator along the best header chain (the Syncer's
/// runs along the connected chain, which during an init is genesis alone: a source on our chain
/// past a made-up fork could then only answer from genesis, all known, and never be heard).
struct Located<'a, 'b> {
    inner: &'a mut (dyn BlockSource + 'b),
    db: &'a ChainDb,
    c: u16,
    back: &'a [Hash256],
}

impl BlockSource for Located<'_, '_> {
    fn get_headers(&mut self, _locator: &[Hash256]) -> std::result::Result<Vec<HeaderEntry>, String> {
        let best = self.db.best_header(self.c).map_err(|e| e.to_string())?;
        let mut loc: Vec<Hash256> = best.into_iter().collect();
        loc.extend(self.back.iter().filter(|h| best.as_ref() != Some(h)));
        self.inner.get_headers(&loc)
    }
    fn get_blocks(&mut self, hashes: &[Hash256]) -> std::result::Result<Vec<Vec<u8>>, String> {
        self.inner.get_blocks(hashes)
    }
    fn describe(&self) -> String {
        self.inner.describe()
    }
}

/// What a store-only batch did.
#[derive(Debug, Default)]
pub struct Stored {
    pub stored: usize,
    /// Asked for and not stored (not sent, or refused): fetch again, from another source.
    pub again: Vec<Hash256>,
    /// Blocks refused, and why.
    pub refused: Vec<(Hash256, String)>,
}

/// What the build did.
#[derive(Debug, Default, Clone)]
pub struct Built {
    pub height: u32,
    pub txs: u64,
    pub coins: u64,
    pub rules_checked: u64,
    pub on_disk: bool,
    /// Blocks the rules refused, dropped to be fetched again: the phase is back at blocks.
    pub refetch: usize,
    /// Timing lines of this build's steps (also kept in STATE INIT_TIMING).
    pub timing: Vec<String>,
}

/// "12.3" per second of `n` over `d`.
pub fn per_s(n: u64, d: std::time::Duration) -> String {
    format!("{:.1}", n as f64 / d.as_secs_f64().max(0.001))
}

/// A duration as "1h 02m 03s" / "4m 05s" / "6.2s".
pub fn took(d: std::time::Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h {:02}m {:02}s", s / 3600, s / 60 % 60, s % 60)
    } else if s >= 60 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

impl ChainDb {
    pub fn init_phase(&self, c: u16) -> Result<Option<Phase>> {
        Ok(self.state_text(c, INIT)?.and_then(|s| Phase::parse(&s)))
    }

    /// An init install is under way (not done): the chain is not to be used or offered.
    pub fn init_active(&self, c: u16) -> bool {
        matches!(self.init_phase(c), Ok(Some(p)) if p != Phase::Done)
    }

    pub fn set_init_phase(&self, c: u16, p: Phase) -> Result<()> {
        self.set_state_text(c, INIT, Some(p.as_str()))
    }

    /// Start an init install on a chain with nothing connected past genesis.
    pub fn init_start(&self, c: u16) -> Result<()> {
        if let Some(t) = self.tip(c)? {
            if t.height > 0 {
                return Err(err(format!("an init install starts from nothing; this chain is at block {} already", t.height)));
            }
        }
        for k in [INIT_ROUND, INIT_NOTE, INIT_BUILT, INIT_TO, INIT_RULES_TO, INIT_REFETCHED, INIT_TIMING, INIT_B_BASE] {
            self.set_state_text(c, k, None)?;
        }
        self.set_init_phase(c, Phase::Headers)
    }

    pub fn init_round(&self, c: u16) -> u32 {
        self.state_text(c, INIT_ROUND).ok().flatten().and_then(|s| s.parse().ok()).unwrap_or(0)
    }

    fn built(&self, c: u16) -> Result<HashSet<String>> {
        Ok(self.state_text(c, INIT_BUILT)?.map(|s| s.split(',').filter(|x| !x.is_empty()).map(String::from).collect()).unwrap_or_default())
    }

    fn mark_built(&self, c: u16, step: &str) -> Result<()> {
        let mut b: Vec<String> = self.built(c)?.into_iter().collect();
        b.push(step.into());
        b.sort();
        b.dedup();
        self.set_state_text(c, INIT_BUILT, Some(&b.join(",")))
    }

    /// Add a timing line (STATE INIT_TIMING keeps the last 40).
    pub fn init_add_timing(&self, c: u16, line: &str) -> Result<()> {
        let mut v = self.init_timing(c);
        v.push(line.replace('\n', " "));
        let skip = v.len().saturating_sub(40);
        self.set_state_text(c, INIT_TIMING, Some(&v[skip..].join("\n")))
    }

    /// Set the in-progress line of a running round (starts with `prefix`): it replaces the one
    /// before, so a long round keeps one up-to-date line, not hundreds.
    pub fn init_set_progress(&self, c: u16, prefix: &str, line: &str) -> Result<()> {
        let mut v: Vec<String> = self.init_timing(c).into_iter().filter(|l| !l.starts_with(prefix)).collect();
        v.push(line.replace('\n', " "));
        let skip = v.len().saturating_sub(40);
        self.set_state_text(c, INIT_TIMING, Some(&v[skip..].join("\n")))
    }

    /// The timing lines so far, oldest first.
    pub fn init_timing(&self, c: u16) -> Vec<String> {
        self.state_text(c, INIT_TIMING).ok().flatten().map(|s| s.lines().map(String::from).collect()).unwrap_or_default()
    }

    /// Build steps finished so far (for status).
    pub fn init_built(&self, c: u16) -> Vec<String> {
        let mut v: Vec<String> = self.built(c).unwrap_or_default().into_iter().collect();
        v.sort();
        v
    }

    /// A locator along the best header chain: the best header, the 10 under it, then
    /// doubling steps back to genesis (one walk down the chain).
    pub fn init_locator(&self, c: u16) -> Result<Vec<Hash256>> {
        let Some(best) = self.best_header(c)? else { return Ok(vec![]) };
        self.store(c).read(|r| {
            let mut out = Vec::new();
            let (mut h, mut step, mut next) = (best, 1u32, None::<u32>);
            loop {
                let rec = header_rec(r, c, &h)?.ok_or_else(|| err("a header's parent is missing"))?;
                if next.is_none_or(|n| rec.height <= n) {
                    out.push(h);
                    if out.len() > 10 {
                        step = step.saturating_mul(2);
                    }
                    next = Some(rec.height.saturating_sub(step));
                }
                if rec.height == 0 {
                    break;
                }
                h = rec.header.prev;
            }
            if out.last() != Some(&h) {
                out.push(h);
            }
            Ok(out)
        })
    }

    /// The best header chain above the tip: (hash, block number; 0 = not stored), lowest first.
    pub fn init_path(&self, c: u16) -> Result<Vec<(Hash256, u32)>> {
        let tip = self.tip(c)?.ok_or_else(|| err("connect genesis first"))?;
        let Some(best) = self.best_header(c)? else { return Ok(vec![]) };
        self.store(c).read(|r| {
            let mut out = Vec::new();
            let mut h = best;
            let mut rec = header_rec(r, c, &h)?.ok_or_else(|| err("best header missing"))?;
            while rec.height > tip.height {
                out.push((h, rec.b));
                h = rec.header.prev;
                rec = header_rec(r, c, &h)?.ok_or_else(|| err("a header's parent is missing"))?;
            }
            if h != tip.hash {
                return Err(err("the best header chain doesn't build on our tip"));
            }
            out.reverse();
            Ok(out)
        })
    }

    /// Hashes at heights `from..=to` on the header chain ending at `target` (height `to`).
    fn path_down(&self, c: u16, from: u32, to: u32, target: &Hash256) -> Result<Vec<Hash256>> {
        self.store(c).read(|r| {
            let mut out = Vec::with_capacity((to + 1).saturating_sub(from) as usize);
            let mut h = *target;
            let mut rec = header_rec(r, c, &h)?.ok_or_else(|| err("the build's target header is missing"))?;
            if rec.height != to {
                return Err(err(format!("the build's target is at {}, not {to}", rec.height)));
            }
            while rec.height >= from {
                out.push(h);
                if rec.height == 0 {
                    break;
                }
                h = rec.header.prev;
                rec = header_rec(r, c, &h)?.ok_or_else(|| err("a header's parent is missing"))?;
            }
            out.reverse();
            Ok(out)
        })
    }

    /// Blocks on the best header chain we don't have yet, lowest first.
    pub fn init_missing(&self, c: u16) -> Result<Vec<Hash256>> {
        self.init_missing_upto(c, None)
    }

    /// [`ChainDb::init_missing`] at heights up to `upto` (None: all).
    pub fn init_missing_upto(&self, c: u16, upto: Option<u32>) -> Result<Vec<Hash256>> {
        let tip = self.tip(c)?.map(|t| t.height).unwrap_or(0);
        Ok(self
            .init_path(c)?
            .into_iter()
            .enumerate()
            .filter(|(i, (_, b))| *b == 0 && upto.is_none_or(|u| tip + 1 + *i as u32 <= u))
            .map(|(_, (h, _))| h)
            .collect())
    }

    /// One headers round: headers from every source until each has nothing new (a source that
    /// sends a bad header or fails is left out of the count), then each is asked what follows
    /// our best header. `sources`: (independence key, source); two sources with the same key
    /// count once (the caller keys every source by its network, so one host, P2P or arcade, counts once).
    pub fn init_headers_round(&self, syncer: &Syncer, sources: &mut [(String, &mut dyn BlockSource)], stop: &AtomicBool) -> Result<Agreement> {
        let p = syncer.params;
        let c = p.chain_no;
        let mut a = Agreement::default();
        let back = self.init_locator(c)?;
        let mut failed: HashMap<String, String> = HashMap::new();
        for (key, src) in sources.iter_mut() {
            let mut loc = Located { inner: &mut **src, db: self, c, back: &back };
            loop {
                if stop.load(Ordering::Relaxed) {
                    return Err(err("stopped"));
                }
                match syncer.sync_headers_once(&mut loc) {
                    Ok(0) => break,
                    Ok(n) => a.new_headers += n,
                    Err(e) => {
                        failed.insert(key.clone(), e.to_string());
                        break;
                    }
                }
            }
        }
        let best = self.best_header(c)?.ok_or_else(|| err("no headers yet"))?;
        let rec = self.header(c, &best)?.ok_or_else(|| err("best header missing"))?;
        a.best = best;
        a.height = rec.height;
        a.work_ok = rec.work >= p.min_chain_work();
        // Asked from best's parent, an agreeing source must send best itself.
        let ask = if rec.height > 0 { rec.header.prev } else { best };
        let mut agree: Vec<String> = Vec::new();
        for (key, src) in sources.iter_mut() {
            if let Some(why) = failed.get(key) {
                a.differ.push(format!("{key}: {why}"));
                continue;
            }
            match src.get_headers(&[ask]) {
                Ok(reply) if rec.height > 0 && agrees(p, &best, &reply) => {
                    if !agree.contains(key) {
                        agree.push(key.clone());
                    }
                }
                Ok(reply) if reply.is_empty() => a.differ.push(format!("{key}: sent nothing after our best header's parent")),
                Ok(_) => a.differ.push(format!("{key}: on another chain")),
                Err(e) => a.differ.push(format!("{key}: {e}")),
            }
        }
        a.agree = agree;
        self.set_state_text(c, INIT_NOTE, Some(&a.text()))?;
        Ok(a)
    }

    /// Store a batch a source sent for `wanted` (store-only, any order): each block must be one
    /// asked for and pass check_stored_block (rebuilds byte-identically, merkle root, witness
    /// commitment where SegWit is on). Good blocks are stored, the rest are to fetch again.
    pub fn init_store(&self, params: &ChainParams, wanted: &[Hash256], raws: &[Vec<u8>]) -> Result<Stored> {
        let want: HashSet<Hash256> = wanted.iter().copied().collect();
        let mut out = Stored::default();
        let mut good: Vec<Block> = Vec::with_capacity(raws.len());
        let mut got: HashSet<Hash256> = HashSet::new();
        for raw in raws {
            let id = match Block::parse_with(raw, &params.format) {
                Ok(b) => b.hash(),
                Err(e) => {
                    out.refused.push((Hash256::ZERO, e.to_string()));
                    continue;
                }
            };
            if !want.contains(&id) || got.contains(&id) {
                out.refused.push((id, "not a block asked for".into()));
                continue;
            }
            match check_stored_block(self, params, raw) {
                Ok(b) => {
                    got.insert(id);
                    good.push(b);
                }
                Err(e) => out.refused.push((id, e.to_string())),
            }
        }
        if !good.is_empty() {
            self.write_stored_as(params, &good, true)?;
        }
        out.stored = good.len();
        out.again = wanted.iter().filter(|h| !got.contains(h)).copied().collect();
        Ok(out)
    }

    /// The blocks phase from plain sources, one batch at a time (arcaded fetches from many
    /// sources at once, in psync, and stores with init_store just the same): every missing
    /// block, a refused batch fetched again from the next source. Returns blocks stored.
    pub fn init_fetch_blocks(&self, params: &ChainParams, sources: &mut [&mut dyn BlockSource], batch: usize, stop: &AtomicBool) -> Result<u64> {
        let c = params.chain_no;
        let mut n = 0u64;
        let mut todo = self.init_missing(c)?;
        let mut turn = 0usize;
        let mut fails = 0usize;
        while !todo.is_empty() {
            if stop.load(Ordering::Relaxed) {
                return Err(err("stopped"));
            }
            if sources.is_empty() || fails > 3 * sources.len() {
                return Err(err(format!("{} blocks no source sent as asked", todo.len())));
            }
            let k = batch.max(1).min(todo.len());
            let chunk: Vec<Hash256> = todo[..k].to_vec();
            let src = &mut sources[turn % sources.len()];
            turn += 1;
            let r = match src.get_blocks(&chunk) {
                Ok(raws) => self.init_store(params, &chunk, &raws)?,
                Err(_) => Stored { again: chunk.clone(), ..Default::default() },
            };
            n += r.stored as u64;
            fails = if r.again.is_empty() { 0 } else { fails + 1 };
            let again: HashSet<Hash256> = r.again.iter().copied().collect();
            todo = todo[k..].to_vec();
            // The ones to fetch again go first (to the next source).
            let mut v: Vec<Hash256> = chunk.into_iter().filter(|h| again.contains(h)).collect();
            v.extend(todo);
            todo = v;
        }
        Ok(n)
    }

    /// The build: from the stored blocks to a chain at height best - `tail`, in bulk passes.
    /// Each step is recorded once done (INIT_BUILT), so a restart carries on from the next.
    /// `threads`: for the rules check; `budget`: bytes the coin derivation may hold in memory
    /// before it goes on on disk; `tmp`: its scratch space (and the tx index sort's).
    ///
    /// The coin set is derived alongside the rules check but written only once the rules passed.
    /// Blocks the rules refuse are dropped and fetched again once (`Built::refetch`, the phase back
    /// at blocks): a stored body could be a damaged copy, not the chain's block. A chain that
    /// breaks the rules even so, or spends coins it never made, is a [`StoreError::InvalidBlock`]
    /// (no retry fixes it); any other error (stopped, the disk) may pass on a retry.
    #[allow(clippy::too_many_arguments)]
    pub fn init_build(&self, params: &ChainParams, tail: u32, threads: usize, budget: usize, tmp: &Path, stop: &AtomicBool, note: Note) -> Result<Built> {
        let c = params.chain_no;
        self.flush_coins(c)?;
        // Scratch an earlier run left is of no use: its sorts and the derivation start over.
        let _ = std::fs::remove_dir_all(tmp);
        let tip = self.tip(c)?.ok_or_else(|| err("connect genesis first"))?;
        // The target: fixed when the build starts, so a restart builds the same blocks.
        let (to, target) = match self.state_text(c, INIT_TO)?.and_then(|s| {
            let (h, x) = s.split_once(':')?;
            Some((h.parse::<u32>().ok()?, Hash256::from_hex(x).ok()?))
        }) {
            Some(t) => t,
            None => {
                let path = self.init_path(c)?;
                let n = path.len().saturating_sub(tail as usize);
                if n == 0 {
                    self.set_init_phase(c, Phase::Done)?;
                    return Ok(Built { height: tip.height, ..Default::default() });
                }
                let (h, _) = path[n - 1];
                let to = tip.height + n as u32;
                self.set_state_text(c, INIT_TO, Some(&format!("{to}:{}", h.to_hex())))?;
                (to, h)
            }
        };
        let path = self.path_down(c, tip.height + 1, to, &target)?;
        let mut done = self.built(c)?;
        let mut out = Built { height: to, ..Default::default() };
        // 1. The stored blocks join the main chain (heights; the tip stays until the end).
        if !done.contains("main") {
            for h in &path {
                if self.header(c, h)?.map(|r| r.b).unwrap_or(0) == 0 {
                    return Err(err(format!("block {h} is not stored yet")));
                }
            }
            let t = std::time::Instant::now();
            self.join_main(params, &path, tip.height)?;
            self.init_timed(c, &mut out, format!("build main: {} heights joined in {}", path.len(), took(t.elapsed())))?;
            self.mark_built(c, "main")?;
            done.insert("main".into());
        }
        stopped(stop)?;
        // 2. The tx index, one sorted pass.
        if !done.contains("txs") {
            let bs: Vec<u32> = path.iter().map(|h| self.header(c, h).map(|r| r.map(|r| r.b).unwrap_or(0))).collect::<Result<_>>()?;
            note("build: transaction index".into());
            let t = std::time::Instant::now();
            out.txs = self.write_txs_for(params, &bs, &tmp.join("txs"), stop, note)?;
            let line = format!("build txs: {} tx index rows in {} · {} rows/s", out.txs, took(t.elapsed()), per_s(out.txs, t.elapsed()));
            self.init_timed(c, &mut out, line)?;
            self.mark_built(c, "txs")?;
            done.insert("txs".into());
        }
        stopped(stop)?;
        // 3. The coin set derived from the blocks (not written yet), while the rules are checked
        //    on every core.
        let rules_on = (params.value_rules.is_some() || params.block_rules.is_some()) && !done.contains("rules");
        let rules_from = self.state_text(c, INIT_RULES_TO)?.and_then(|s| s.parse::<u32>().ok()).map(|h| h + 1).unwrap_or(1).max(tip.height + 1);
        let halt = AtomicBool::new(false);
        let seen: std::sync::Mutex<Option<crate::rules::Report>> = std::sync::Mutex::new(None);
        let rules_text = |seen: &std::sync::Mutex<Option<crate::rules::Report>>| {
            seen.lock().unwrap().as_ref().map(|r| format!(" · rules checked to {} of {to}", r.done_to)).unwrap_or_default()
        };
        let (coins, rules) = std::thread::scope(|sc| {
            let h = (rules_on && rules_from <= to).then(|| {
                let (halt, seen) = (&halt, &seen);
                sc.spawn(move || {
                    let t = std::time::Instant::now();
                    let r = crate::rules::replay(self, params, rules_from, to, threads, crate::rules::verify_all_scripts(), halt, &|r| {
                        *seen.lock().unwrap() = Some(r.clone());
                    });
                    (r, t.elapsed())
                })
            });
            let coins = if done.contains("coins") {
                None
            } else {
                let t = std::time::Instant::now();
                let r = self.init_coins_derive(params, to, budget, &tmp.join("coins"), stop, &mut |s: String| note(format!("build: {s}{}", rules_text(&seen))));
                if r.is_err() {
                    halt.store(true, Ordering::Relaxed);
                }
                Some((r, t.elapsed()))
            };
            let mut last = std::time::Instant::now();
            while h.as_ref().is_some_and(|h| !h.is_finished()) {
                if stop.load(Ordering::Relaxed) {
                    halt.store(true, Ordering::Relaxed);
                }
                if last.elapsed() > std::time::Duration::from_secs(5) {
                    note(format!("build: checking the rules{}", rules_text(&seen)));
                    last = std::time::Instant::now();
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            (coins, h.map(|h| h.join().expect("rules check thread")))
        });
        let mut first_err = None;
        if let Some((r, d)) = rules {
            let n = (r.done_to + 1).saturating_sub(rules_from) as u64;
            self.init_timed(c, &mut out, format!("build rules: {n} blocks checked (heights {rules_from}..{}) in {} · {} blocks/s", r.done_to, took(d), per_s(n, d)))?;
            if r.broken_count > 0 {
                return self.init_refused(params, &r, out);
            } else if r.no_body > 0 || r.no_coin > 0 {
                first_err = Some(err(format!("the rules check could not read {} blocks and {} spent coins", r.no_body, r.no_coin)));
            } else if r.done_to < to {
                self.set_state_text(c, INIT_RULES_TO, Some(&r.done_to.to_string()))?;
                first_err = Some(err("stopped"));
            } else {
                out.rules_checked = r.checked;
                self.mark_built(c, "rules")?;
            }
        } else if params.value_rules.is_some() || params.block_rules.is_some() {
            // Checked before a restart (or nothing to check).
            self.mark_built(c, "rules")?;
        }
        let derived = match coins {
            Some((Ok(d), t)) => {
                let how = if matches!(d, Derived::Disk(..)) { "on disk" } else { "in memory" };
                self.init_timed(c, &mut out, format!("build coins: {to} blocks derived {how} in {} · {} blocks/s", took(t), per_s(to as u64, t)))?;
                Some(d)
            }
            // The derivation's own error says more than the "stopped" of the rules check it halted.
            Some((Err(e), _)) => {
                if first_err.is_none() || !is_stopped(&e) {
                    first_err = Some(e);
                }
                None
            }
            None => None,
        };
        if let Some(e) = first_err {
            return Err(e);
        }
        // 4. The coin set is written, now that the rules passed.
        if let Some(d) = derived {
            let t = std::time::Instant::now();
            let (n, on_disk) = self.init_coins_apply(params, d, &tmp.join("coins"), stop, note)?;
            self.init_timed(c, &mut out, format!("build coins written: {n} coins in {} · {} coins/s", took(t.elapsed()), per_s(n, t.elapsed())))?;
            out.coins = n;
            out.on_disk = on_disk;
            self.mark_built(c, "coins")?;
        }
        stopped(stop)?;
        // 5. The tip moves: the chain is usable, the normal sync carries on from here.
        let new_b = self.header(c, &target)?.map(|r| r.b).unwrap_or(0);
        self.wstore(c)?.write(|w| {
            let t = Tip { hash: target, height: to, b: new_b };
            w.put(STATE, &k_state(c, "tip"), &encode_tip(&t))?;
            w.put(STATE, &k_state(c, "coins_at"), &encode_tip(&t))?;
            if w.get(STATE, &k_state(c, crate::addrbuild::OFF))?.is_none() {
                // Address rows of heights tip+1..=to are built from the stored blocks.
                let from = tip.height + 1;
                let old_from = w.get(STATE, &k_state(c, crate::addrbuild::BACKFILL))?.map(|_| {
                    w.get(STATE, &k_state(c, crate::addrbuild::BACKFILL_FROM)).ok().flatten().map(|v| be_u32(&v, 0)).unwrap_or(1)
                });
                w.put(STATE, &k_state(c, crate::addrbuild::BACKFILL), &to.to_be_bytes())?;
                w.put(STATE, &k_state(c, crate::addrbuild::BACKFILL_FROM), &old_from.unwrap_or(from).min(from).to_be_bytes())?;
                w.put(STATE, &k_state(c, crate::addrbuild::PENDING), b"1")?;
            }
            let v = format!("{{\"height\":{to},\"coins\":{},\"maker\":\"init\",\"on_disk\":{}}}", out.coins, out.on_disk);
            w.put(STATE, &k_state(c, VERIFIED), v.as_bytes())?;
            w.del(STATE, &k_state(c, TRUSTED))?;
            w.put(STATE, &k_state(c, INIT), Phase::Done.as_str().as_bytes())?;
            w.put(STATE, &k_state(c, INIT_NOTE), format!("built to block {to}; the normal sync carries on").as_bytes())?;
            Ok(())
        })?;
        let _ = std::fs::remove_dir_all(tmp);
        Ok(out)
    }

    /// Record a build step's timing line (STATE and `out.timing`).
    fn init_timed(&self, c: u16, out: &mut Built, line: String) -> Result<()> {
        self.init_add_timing(c, &line)?;
        out.timing.push(line);
        Ok(())
    }

    /// The rules check refused blocks. A stored body may be a damaged copy a source sent rather
    /// than the chain's block, so the first time each is dropped (its header kept, its body
    /// forgotten) and the phase goes back to blocks to fetch it again, from whichever source;
    /// the build then starts over from its first step (heights and the tx index name block
    /// numbers). A block refused again after that is the chain's own: InvalidBlock.
    fn init_refused(&self, params: &ChainParams, r: &crate::rules::Report, mut out: Built) -> Result<Built> {
        let c = params.chain_no;
        let mut again: HashSet<String> = self.state_text(c, INIT_REFETCHED)?.map(|s| s.split(',').filter(|x| !x.is_empty()).map(String::from).collect()).unwrap_or_default();
        let (h, hash, why) = r.broken.first().cloned().unwrap_or_default();
        let hashes: Vec<Hash256> = r.broken.iter().filter_map(|(_, x, _)| Hash256::from_hex(x).ok()).collect();
        if hashes.is_empty() || hashes.iter().any(|x| again.contains(&x.to_hex())) {
            return Err(StoreError::InvalidBlock {
                block: Hash256::from_hex(&hash).map(|x| x.0).unwrap_or([0; 32]),
                why: format!("{} blocks break the chain's rules, the first at {h} ({hash}): {why}; still refused after it was fetched again, the build stops here", r.broken_count),
            });
        }
        for x in &hashes {
            self.init_drop_body(params, x)?;
            again.insert(x.to_hex());
        }
        let mut v: Vec<String> = again.into_iter().collect();
        v.sort();
        self.set_state_text(c, INIT_REFETCHED, Some(&v.join(",")))?;
        self.set_state_text(c, INIT_BUILT, None)?;
        self.set_state_text(c, INIT_NOTE, Some(&format!("the rules refused {} blocks, the first at {h}: {why}; dropped, to be fetched again", hashes.len())))?;
        self.set_init_phase(c, Phase::Blocks)?;
        out.refetch = hashes.len();
        Ok(out)
    }

    /// Forget a stored block's body (its rows, its tx index rows, its height row); its header stays.
    fn init_drop_body(&self, params: &ChainParams, hash: &Hash256) -> Result<()> {
        self.wstore(params.chain_no)?.write(|w| drop_body_in(w, params, hash))
    }

    /// The coin set of heights 1..=to derived from the stored main chain (in memory while it
    /// fits `budget`, else on disk), nothing written yet. Every input must spend a coin made
    /// before it (an earlier block, or earlier in its own block) and not spent yet; else the
    /// blocks are refused (InvalidBlock).
    fn init_coins_derive(&self, params: &ChainParams, to: u32, budget: usize, tmp: &Path, stop: &AtomicBool, note: Note) -> Result<Derived> {
        let c = params.chain_no;
        let mut mem = Replay::default();
        if mem.advance(self, params, to, budget, stop, note)? {
            if mem.missing > 0 {
                return Err(never_made(mem.missing));
            }
            return Ok(Derived::Mem(mem.sorted(c)));
        }
        drop(mem);
        note("the coin set is over its memory budget (ARCADE_INIT_COINS_MB): derived on disk".into());
        let mut d = DiskReplay::new(tmp, c)?;
        d.advance(self, params, to, stop, note)?;
        let mut rows = d.finish()?;
        // Every row once through before anything is written: the count of spends of coins never
        // made is complete only at the end.
        let mut keep = Sorter::new(tmp, &format!("coins-{c}"))?;
        let mut n = 0u64;
        while let Some((k, v)) = rows.next_kv()? {
            if n.is_multiple_of(1_000_000) && stop.load(Ordering::Relaxed) {
                return Err(err("stopped"));
            }
            keep.push(&k, &v)?;
            n += 1;
        }
        if rows.missing > 0 {
            return Err(never_made(rows.missing));
        }
        drop(rows);
        Ok(Derived::Disk(keep.finish()?, n))
    }

    /// Write a derived coin set in place of what is stored (only differences). Returns (coins,
    /// derived on disk).
    fn init_coins_apply(&self, params: &ChainParams, d: Derived, tmp: &Path, stop: &AtomicBool, note: Note) -> Result<(u64, bool)> {
        match d {
            Derived::Mem(rows) => {
                let n = rows.len() as u64;
                let mut it = rows.into_iter();
                self.apply_coin_diff(params, &mut || Ok(it.next()), true, tmp, stop, note)?;
                Ok((n, false))
            }
            Derived::Disk(mut s, n) => {
                self.apply_coin_diff(params, &mut || s.next_kv(), true, tmp, stop, note)?;
                Ok((n, true))
            }
        }
    }
}

/// A coin set derived and checked, not written yet: OUTPUTS rows in key order.
enum Derived {
    Mem(Vec<(Vec<u8>, Vec<u8>)>),
    Disk(Sorted, u64),
}

fn never_made(n: u64) -> StoreError {
    StoreError::InvalidBlock { block: [0; 32], why: format!("{n} inputs spend coins the blocks never made (or not made before them); the blocks are refused") }
}

fn stopped(stop: &AtomicBool) -> Result<()> {
    if stop.load(Ordering::Relaxed) {
        return Err(err("stopped"));
    }
    Ok(())
}

fn is_stopped(e: &StoreError) -> bool {
    matches!(e, StoreError::Other(s) if s == "stopped")
}
