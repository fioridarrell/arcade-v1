//! Headers-first sync from P2P peers into a ChainDb.
//!
//! 1. Headers: ask for headers after our best, check each (link, difficulty step, work),
//!    store with cumulative work; `best` follows the most work.
//! 2. Blocks: walk from the best header back to the connected chain (the fork point),
//!    disconnect anything above the fork, then fetch and connect blocks forward in
//!    batches, each batch one commit.

use crate::db::{ChainDb, HeaderRec, Tip, STATUS_HEADER};
use arcade_chain::params::{check_block_work, check_header, ChainParams, PowCheck};
use arcade_chain::p2p::Peer;
use arcade_chain::pow::{meets_target, pow_hash, work_from_bits, PowAlgo};
use arcade_chain::{Block, Hash256};
use std::collections::HashMap;

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("store: {0}")]
    Store(#[from] arcade_store::StoreError),
    #[error("p2p: {0}")]
    P2p(#[from] arcade_chain::p2p::P2pError),
    #[error("peer sent a bad header at height {height}: {why}")]
    BadHeader { height: u32, why: String },
    #[error("peer sent a bad block {hash}: {why}")]
    BadBlock { hash: Hash256, why: String },
    #[error("reorg of {0} blocks is deeper than the undo window")]
    ReorgTooDeep(u32),
}

pub type Result<T> = std::result::Result<T, SyncError>;

pub struct Syncer<'a> {
    pub db: &'a ChainDb,
    pub params: &'static ChainParams,
    /// Remaining path to the best header, cached between rounds (walking 1.5M headers
    /// back every round would be quadratic). Valid while its first entry extends the tip.
    path: std::cell::RefCell<(Hash256, Vec<Hash256>)>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Progress {
    pub headers: u32,
    pub blocks: u32,
    pub disconnected: u32,
}

impl<'a> Syncer<'a> {
    pub fn new(db: &'a ChainDb, params: &'static ChainParams) -> Self {
        Syncer { db, params, path: Default::default() }
    }

    fn c(&self) -> u16 {
        self.params.chain_no
    }

    /// Make sure genesis is stored and connected (fetching it from the peer if needed).
    pub fn ensure_genesis(&self, peer: &mut Peer) -> Result<Tip> {
        if let Some(t) = self.db.tip(self.c())? {
            return Ok(t);
        }
        let g = self.params.genesis_hash();
        let raw = peer.get_blocks(&[g])?.remove(0);
        let b = Block::parse_with(&raw, &self.params.format)
            .map_err(|e| SyncError::BadBlock { hash: g, why: e.to_string() })?;
        if b.hash() != g {
            return Err(SyncError::BadBlock { hash: g, why: "not the genesis block".into() });
        }
        if let Some(gh) = self.params.genesis_header() {
            if gh != b.header {
                return Err(SyncError::BadBlock { hash: g, why: "genesis header differs".into() });
            }
        }
        let rec = HeaderRec {
            b: 0,
            height: 0,
            status: STATUS_HEADER,
            work: work_from_bits(b.header.bits),
            header: b.header,
            auxpow: vec![],
        };
        self.db.put_headers(self.c(), &[(g, rec)])?;
        Ok(self.db.connect_blocks(self.params, &[b])?)
    }

    fn locator(&self) -> Result<Vec<Hash256>> {
        let c = self.c();
        let mut loc = Vec::new();
        if let Some(b) = self.db.best_header(c)? {
            loc.push(b);
        }
        if let Some(t) = self.db.tip(c)? {
            let mut step = 1u32;
            let mut h = t.height as i64;
            while h > 0 {
                if let Some(x) = self.db.hash_at(c, h as u32)? {
                    loc.push(x);
                }
                if loc.len() > 10 {
                    step *= 2;
                }
                h -= step as i64;
            }
        }
        loc.push(self.params.genesis_hash());
        loc.dedup();
        Ok(loc)
    }

    /// One round of getheaders. Returns how many new headers were stored (0 = caught up).
    pub fn sync_headers_once(&self, peer: &mut Peer) -> Result<u32> {
        let c = self.c();
        let entries = peer.get_headers(&self.locator()?)?;
        let mut batch: Vec<(Hash256, HeaderRec)> = Vec::with_capacity(entries.len());
        let mut pending: HashMap<Hash256, HeaderRec> = HashMap::new();
        for e in entries {
            let hash = self.params.block_id(&e.header);
            if pending.contains_key(&hash) || self.db.header(c, &hash)?.is_some() {
                continue;
            }
            let parent = match pending.get(&e.header.prev) {
                Some(p) => p.clone(),
                None => match self.db.header(c, &e.header.prev)? {
                    Some(p) => p,
                    None => break, // not connected to anything we know; stop this batch
                },
            };
            let height = parent.height + 1;
            let bad = |why: String| SyncError::BadHeader { height, why };
            match check_header(self.params, &e.header, &parent.header, height)
                .map_err(|x| bad(x.to_string()))?
            {
                PowCheck::AuxPow => {
                    let a = e.auxpow.as_ref().ok_or_else(|| bad("AuxPoW flag without data".into()))?;
                    let chain_id = self.params.auxpow_chain_id.unwrap_or(e.header.chain_id());
                    a.check(hash, chain_id, self.params.auxpow_strict_chain_id)
                        .map_err(|x| bad(x.to_string()))?;
                    if !meets_target(&pow_hash(PowAlgo::Scrypt, &a.parent.encode()), e.header.bits) {
                        return Err(bad("AuxPoW parent misses target".into()));
                    }
                }
                // PoS candidates are settled when the block arrives (check_block_work).
                PowCheck::Verified | PowCheck::NotCheckable | PowCheck::NeedsBlock => {}
            }
            let rec = HeaderRec {
                b: 0,
                height,
                status: STATUS_HEADER,
                work: ChainDb::child_work(&parent, e.header.bits),
                header: e.header,
                // Checked above; the proof is kept once, with the block (block_meta).
                auxpow: Vec::new(),
            };
            pending.insert(hash, rec.clone());
            batch.push((hash, rec));
        }
        let n = batch.len() as u32;
        if n > 0 {
            self.db.put_headers(c, &batch)?;
        }
        Ok(n)
    }

    /// Hashes from just above the fork point up to the best header, and the fork height.
    fn path_to_best(&self) -> Result<(u32, Vec<Hash256>)> {
        let c = self.c();
        let Some(best) = self.db.best_header(c)? else { return Ok((0, vec![])) };
        if let Some(tip) = self.db.tip(c)? {
            let cached = self.path.borrow();
            if cached.0 == best {
                if let Some(first) = cached.1.first() {
                    if self.db.header(c, first)?.map(|r| r.header.prev) == Some(tip.hash) {
                        return Ok((tip.height, cached.1.clone()));
                    }
                }
            }
        }
        let (fork, path) = self.walk_to_best(best)?;
        *self.path.borrow_mut() = (best, path.clone());
        Ok((fork, path))
    }

    fn walk_to_best(&self, best: Hash256) -> Result<(u32, Vec<Hash256>)> {
        let c = self.c();
        let mut h = best;
        let mut path = Vec::new();
        loop {
            let rec = self.db.header(c, &h)?.expect("best chain headers are stored");
            if self.db.hash_at(c, rec.height)? == Some(h) {
                path.reverse();
                return Ok((rec.height, path));
            }
            path.push(h);
            h = rec.header.prev;
        }
    }

    /// Connect up to `max` blocks toward the best header. Returns progress.
    pub fn sync_blocks(&self, peer: &mut Peer, max: u32, batch: usize) -> Result<Progress> {
        let c = self.c();
        let mut prog = Progress::default();
        let (fork, path) = self.path_to_best()?;
        let tip = self.db.tip(c)?.expect("genesis connected first");
        if fork < tip.height {
            let depth = tip.height - fork;
            if depth > crate::db::UNDO_DEPTH {
                return Err(SyncError::ReorgTooDeep(depth));
            }
            for _ in 0..depth {
                self.db.disconnect_tip(c)?;
                prog.disconnected += 1;
            }
        }
        let path: Vec<_> = path.into_iter().take(max as usize).collect();
        let mut height = fork;
        for chunk in path.chunks(batch.max(1)) {
            let raws = peer.get_blocks(chunk)?;
            let mut by_hash: HashMap<Hash256, Block> = HashMap::new();
            for raw in raws {
                let b = Block::parse_with(&raw, &self.params.format)
                    .map_err(|e| SyncError::BadBlock { hash: Hash256::ZERO, why: e.to_string() })?;
                if b.encode() != raw {
                    return Err(SyncError::BadBlock { hash: b.hash(), why: "does not rebuild byte-identically".into() });
                }
                by_hash.insert(b.hash(), b);
            }
            let mut blocks = Vec::with_capacity(chunk.len());
            for h in chunk {
                height += 1;
                let b = by_hash
                    .remove(h)
                    .ok_or_else(|| SyncError::BadBlock { hash: *h, why: "peer did not send it".into() })?;
                b.check().map_err(|e| SyncError::BadBlock { hash: *h, why: e.to_string() })?;
                if let Some(rec) = self.db.header(c, h)? {
                    let parent = self.db.header(c, &rec.header.prev)?.expect("parent stored");
                    if let Ok(PowCheck::NeedsBlock) = check_header(self.params, &b.header, &parent.header, height) {
                        check_block_work(self.params, &b, height)
                            .map_err(|e| SyncError::BadBlock { hash: *h, why: e.to_string() })?;
                    }
                }
                blocks.push(b);
            }
            self.db.connect_blocks(self.params, &blocks)?;
            prog.blocks += blocks.len() as u32;
            // Drop what we connected from the cached path.
            let mut cached = self.path.borrow_mut();
            let n = blocks.len().min(cached.1.len());
            cached.1.drain(..n);
        }
        Ok(prog)
    }
}
