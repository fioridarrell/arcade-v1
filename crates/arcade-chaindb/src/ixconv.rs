//! Index files from the classic tables (docs/INDEXFILES.md §3).
//!
//! A chain that already has `script_hist`, `spends` and `txs` is converted by one scan of
//! each table, in key order, in short read transactions: every settled row (its height at
//! or below the sealed height) is handed to the file of its height span. The tables are
//! not changed. When a kind's files are written and read back, its marker is set and the
//! readers switch (ixset).
//!
//! Only settled heights are converted: the sealed height is the end of the last whole
//! range that lies `depth` (never less than `UNDO_DEPTH`) blocks below both the tip and
//! the height the coins are complete at. Rows that deep cannot change: this database
//! cannot disconnect a block that deep.
//!
//! Safe to stop and run again: a pass saves a checkpoint (how far the scan got, and each
//! open file's state) and carries on from it; a finished kind is skipped; running again
//! later converts the ranges that have settled since, and merges ten ranges into an era.

use crate::db::{decode_tip, ChainDb, UNDO_DEPTH};
use crate::ixfile::{self, Kind, Meta, Reader, Writer};
use crate::addrbuild::{OFF, PENDING, TXS_PENDING};
use crate::ixset::{addr_epoch, list, marker, marker_name};
use crate::schema::*;
use arcade_chain::hash::sha256;
use arcade_chain::params::ChainParams;
use arcade_store::key::{be_u32, be_u64};
use arcade_store::{ReadTxn, Result, StoreError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// How heights are cut into files. Every arcade must use the same cut for its files to
/// equal another's: [`Layout::STANDARD`]. (Tests use small spans.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Heights per range file.
    pub range: u32,
    /// Ranges per era file.
    pub era_ranges: u32,
    /// How far below the tip a range's last block must be before the range is sealed.
    pub depth: u32,
}

impl Layout {
    pub const STANDARD: Layout = Layout { range: 10_000, era_ranges: 10, depth: UNDO_DEPTH };

    fn era(&self) -> u64 {
        self.range as u64 * self.era_ranges as u64
    }

    /// The sealed height for a chain settled up to `base`: the end of the last whole range
    /// at least `depth` below it (None: not even the first range is that deep yet).
    pub fn sealed(&self, base: u32) -> Option<u32> {
        let n = (base.checked_sub(self.depth)? as u64 + 1) / self.range as u64;
        if n > 0 {
            Some((n * self.range as u64 - 1) as u32)
        } else {
            None
        }
    }

    /// The files to write for heights `next..=upto` (both range edges): a whole era where
    /// one fits, ranges otherwise.
    pub fn units(&self, next: u32, upto: u32) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        let mut h = next as u64;
        while h <= upto as u64 {
            let len = if h.is_multiple_of(self.era()) && h + self.era() - 1 <= upto as u64 { self.era() } else { self.range as u64 };
            out.push((h as u32, (h + len - 1) as u32));
            h += len;
        }
        out
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub layout: Layout,
    /// zstd level (ixfile::LEVEL; the file id does not depend on it, the bytes do).
    pub level: i32,
    /// Blocks compressed at once.
    pub threads: usize,
    /// Table rows read per read transaction (short, so the database can recycle pages
    /// while the chain keeps syncing).
    pub chunk_rows: usize,
    /// Table rows between checkpoints.
    pub checkpoint_rows: u64,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            layout: Layout::STANDARD,
            level: ixfile::LEVEL,
            threads: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(16),
            chunk_rows: 200_000,
            checkpoint_rows: 50_000_000,
        }
    }
}

fn other(e: impl std::fmt::Display) -> StoreError {
    StoreError::Other(format!("index files: {e}"))
}

/// The main chain up to a height: each height's block number and hash, and back.
struct Heights {
    b_at: Vec<u32>,
    hashes: Vec<[u8; 32]>,
    h_of_b: Vec<u32>,
}

impl Heights {
    fn load(db: &ChainDb, c: u16, top: u32) -> Result<Heights> {
        let store = db.store(c);
        let pre = k_chain(c);
        let (mut b_at, mut hashes) = (Vec::with_capacity(top as usize + 1), Vec::with_capacity(top as usize + 1));
        let mut after: Option<Vec<u8>> = None;
        'scan: loop {
            let rows = store.read(|r| r.scan_after(HEIGHT, &pre, after.as_deref(), 50_000))?;
            for (k, v) in &rows {
                let h = be_u32(k, 2);
                if h > top {
                    break 'scan;
                }
                let b = be_u32(v, 32);
                if h as usize != b_at.len() {
                    return Err(StoreError::Corrupt { table: HEIGHT, why: "a height is missing from the main chain" });
                }
                // The scans rely on it: rows sorted by block number are sorted by height.
                if b_at.last().is_some_and(|p| *p >= b) {
                    return Err(other(format!("block numbers are not in height order at height {h}: this chain cannot be converted by scans")));
                }
                b_at.push(b);
                hashes.push(v[..32].try_into().unwrap());
            }
            match rows.last() {
                Some(l) if rows.len() == 50_000 => after = Some(l.0.clone()),
                _ => break,
            }
        }
        if b_at.len() != top as usize + 1 {
            return Err(other(format!("the main chain ends below height {top}")));
        }
        let mut h_of_b = vec![u32::MAX; *b_at.last().unwrap() as usize + 1];
        for (h, b) in b_at.iter().enumerate() {
            h_of_b[*b as usize] = h as u32;
        }
        Ok(Heights { b_at, hashes, h_of_b })
    }

    /// The height of block number `b`, when it is on the main chain at or below the top.
    fn of_b(&self, b: u32) -> Option<u32> {
        self.h_of_b.get(b as usize).copied().filter(|h| *h != u32::MAX)
    }
}

// ---------------- the checkpoint of a pass ----------------

struct Checkpoint {
    /// The marker when the pass began (None: no files yet).
    from: Option<u32>,
    /// The sealed height the pass writes up to, and that block's hash.
    target: u32,
    target_hash: [u8; 32],
    layout: Layout,
    level: i32,
    /// The Addresses epoch when the pass began (ixset::ADDR_EPOCH).
    epoch: u32,
    /// The last table key of the last finished group: the scan carries on after it.
    resume: Option<Vec<u8>>,
    scanned: u64,
    /// Per file: first, last, the writer's saved state.
    units: Vec<(u32, u32, Vec<u8>)>,
}

const CK_MAGIC: &[u8; 8] = b"ARCIXCK2";

impl Checkpoint {
    fn encode(&self) -> Vec<u8> {
        let mut v = CK_MAGIC.to_vec();
        let n = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_be_bytes());
        n(&mut v, self.from.is_some() as u32);
        n(&mut v, self.from.unwrap_or(0));
        n(&mut v, self.target);
        v.extend_from_slice(&self.target_hash);
        n(&mut v, self.layout.range);
        n(&mut v, self.layout.era_ranges);
        n(&mut v, self.layout.depth);
        n(&mut v, self.level as u32);
        n(&mut v, self.epoch);
        n(&mut v, self.resume.is_some() as u32);
        let key = self.resume.clone().unwrap_or_default();
        n(&mut v, key.len() as u32);
        v.extend_from_slice(&key);
        v.extend_from_slice(&self.scanned.to_be_bytes());
        n(&mut v, self.units.len() as u32);
        for (first, last, state) in &self.units {
            n(&mut v, *first);
            n(&mut v, *last);
            n(&mut v, state.len() as u32);
            v.extend_from_slice(state);
        }
        let sum = sha256(&v);
        v.extend_from_slice(&sum);
        v
    }

    fn decode(v: &[u8]) -> Option<Checkpoint> {
        let body = v.get(..v.len().checked_sub(32)?)?;
        if !body.starts_with(CK_MAGIC) || sha256(body)[..] != v[body.len()..] {
            return None;
        }
        let mut t = ixfile::Take(&body[8..]);
        let (has_from, from, target) = (t.u32()?, t.u32()?, t.u32()?);
        let target_hash = t.bytes(32)?.try_into().ok()?;
        let layout = Layout { range: t.u32()?, era_ranges: t.u32()?, depth: t.u32()? };
        let level = t.u32()? as i32;
        let epoch = t.u32()?;
        let (has_key, klen) = (t.u32()?, t.u32()? as usize);
        let key = t.bytes(klen)?.to_vec();
        let scanned = t.u64()?;
        let count = t.u32()?;
        let mut units = Vec::new();
        for _ in 0..count {
            let (first, last, len) = (t.u32()?, t.u32()?, t.u32()? as usize);
            units.push((first, last, t.bytes(len)?.to_vec()));
        }
        if !t.0.is_empty() {
            return None;
        }
        Some(Checkpoint { from: (has_from == 1).then_some(from), target, target_hash, layout, level, epoch, resume: (has_key == 1).then_some(key), scanned, units })
    }

    fn save(&self, path: &Path) -> Result<()> {
        let tmp = ixfile::tmp_path(path);
        let write = || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&self.encode())?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        };
        write().map_err(other)
    }
}

fn checkpoint_path(dir: &Path, kind: Kind) -> PathBuf {
    dir.join(format!("convert-{}.ckpt", kind.tag() as char))
}

/// Unfinished files of a kind left by a pass that will not be carried on.
fn clear_unfinished(dir: &Path, kind: Kind) {
    let _ = std::fs::remove_file(checkpoint_path(dir, kind));
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.strip_suffix(".tmp").and_then(ixfile::parse_name).is_some_and(|(k, ..)| k == kind) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// The table a kind is converted from, and how many key bytes after the chain number make
/// one group: rows of a group are sorted together before they are written (the tables sort
/// by txid prefix where the files sort by position).
fn source(kind: Kind) -> (&'static str, usize) {
    match kind {
        Kind::Hist => (SCRIPT_HIST, 25), // script id | height
        Kind::Spends => (SPENDS, 12),    // the whole key
        Kind::Txs => (TXS, 8),           // the 8-byte txid prefix
    }
}

/// One table row as a file row (None: not a settled main-chain row of `lo..=top`).
fn convert_row(r: &ReadTxn, c: u16, kind: Kind, hs: &Heights, lo: u32, k: &[u8], v: &[u8]) -> Result<Option<Vec<u8>>> {
    let top = hs.b_at.len() as u32 - 1;
    let short = |table| StoreError::Corrupt { table, why: "short row" };
    Ok(match kind {
        Kind::Hist => {
            if k.len() != 39 || v.len() < 16 {
                return Err(short(SCRIPT_HIST));
            }
            let h = be_u32(k, 23);
            if h < lo || h > top {
                return Ok(None);
            }
            // The table names the transaction by txid prefix; the files by its position in
            // the block, which the tx index knows.
            let t12: [u8; 12] = k[27..39].try_into().unwrap();
            let mut cands: Vec<(u32, u32)> = r.scan_prefix(TXS, &k_txs(c, &t12))?.iter().map(|(_, bp)| (be_u32(bp, 0), be_u32(bp, 4))).collect();
            cands.extend(crate::txruns::find(r, r.store_id(), c, &t12)?);
            let mut pos: Option<u32> = None;
            for (b, p) in cands {
                if hs.of_b(b) == Some(h) {
                    if pos.is_some_and(|q| q != p) {
                        return Err(other(format!("two transactions of block {h} share the txid prefix {}: history rows cannot be told apart", hex::encode(t12))));
                    }
                    pos = Some(p);
                }
            }
            let p = pos.ok_or_else(|| other(format!("a history row of block {h} names transaction {}.. that the tx index does not have there", hex::encode(t12))))?;
            Some(ixfile::hist_row(k[2..23].try_into().unwrap(), h, p, be_u64(v, 0), be_u64(v, 8)).to_vec())
        }
        Kind::Spends => {
            if k.len() != 14 || v.len() < 12 {
                return Err(short(SPENDS));
            }
            match hs.of_b(be_u32(v, 0)) {
                Some(sh) if sh >= lo => {
                    let ch = hs.of_b(be_u32(k, 2)).ok_or_else(|| other(format!("a coin spent in block {sh} was made in a block that is not on the main chain")))?;
                    Some(ixfile::spend_row(ch, be_u32(k, 6), be_u32(k, 10), sh, be_u32(v, 4), be_u32(v, 8)).to_vec())
                }
                _ => None,
            }
        }
        Kind::Txs => {
            if k.len() != 14 || v.len() < 8 {
                return Err(short(TXS));
            }
            match hs.of_b(be_u32(v, 0)) {
                Some(h) if h >= lo => Some(ixfile::tx_row(k[2..10].try_into().unwrap(), h, be_u32(v, 4)).to_vec()),
                _ => None,
            }
        }
    })
}

/// A kind's files and the rows waiting to go into them.
struct Out {
    kind: Kind,
    writers: Vec<Writer>,
    /// The open group: its key, its rows, the last table key read into it.
    group_key: Vec<u8>,
    group: Vec<Vec<u8>>,
    group_src: Option<Vec<u8>>,
    /// The last table key of the last group written out.
    done_src: Option<Vec<u8>>,
}

impl Out {
    fn flush_group(&mut self) -> Result<()> {
        self.group.sort_unstable();
        self.group.dedup();
        for row in self.group.drain(..) {
            let h = self.kind.height_of(&row);
            let i = self.writers.partition_point(|w| w.meta().last < h);
            self.writers.get_mut(i).ok_or_else(|| other("a row above the sealed height"))?.push(&row)?;
        }
        if let Some(k) = self.group_src.take() {
            self.done_src = Some(k);
        }
        Ok(())
    }
}

/// Whether every row of `kind` at or below `upto` is in its table: no address rows or tx
/// index rows wait in the log or in runs, and (history, spends) the Addresses index is on
/// and has not been switched since `epoch` (when given). Checked when a pass starts and
/// again in the transaction that switches the readers: a pass takes hours on a big chain,
/// and rows that reach the table only after it was read would be missing from the files
/// for good (the readers no longer consult the table at those heights).
fn not_ready<R: crate::db::Rd>(r: &R, c: u16, kind: Kind, upto: u32, epoch: Option<u32>) -> Result<Option<String>> {
    let addr = kind != Kind::Txs;
    if r.get(STATE, &k_state(c, TXS_PENDING))?.is_some() || (addr && r.get(STATE, &k_state(c, PENDING))?.is_some()) {
        return Ok(Some("address rows or tx index rows are still being built".into()));
    }
    if let Some((k, _)) = r.first(ADDR_LOG, &k_chain(c))?.as_ref() {
        if k.len() >= 6 && be_u32(k, 2) <= upto {
            return Ok(Some(format!("rows of height {} still wait in the address log", be_u32(k, 2))));
        }
    }
    if addr && r.get(STATE, &k_state(c, OFF))?.is_some() {
        return Ok(Some("the Addresses index is off".into()));
    }
    if let (true, Some(e)) = (addr, epoch) {
        if e != addr_epoch(r, c)? {
            return Ok(Some("the Addresses index was turned off or on during the conversion".into()));
        }
    }
    Ok(None)
}

/// What a pass did.
enum Pass {
    /// Nothing new is settled, or the files now reach this height.
    Done,
    Stopped,
}

fn pass(db: &ChainDb, params: &ChainParams, kind: Kind, o: &Options, stop: &AtomicBool, note: &mut dyn FnMut(String)) -> Result<Pass> {
    let c = params.chain_no;
    let dir = db.index_dir(c);
    std::fs::create_dir_all(&dir).map_err(other)?;
    let store = db.store(c);
    let tag = kind.tag() as char;
    let (table, group_len) = source(kind);
    let from = db.index_files_upto(c, kind)?;
    let next = from.map(|f| f + 1).unwrap_or(0);
    let ck_path = checkpoint_path(&dir, kind);
    if crate::txruns::of(store.id(), c).is_some() {
        return Err(other("this chain's tx index is still being built; convert it afterwards"));
    }

    // Carry on an interrupted pass when it is still the same job on the same chain.
    let mut carried: Option<(Checkpoint, Vec<Writer>)> = None;
    if let Some(ck) = std::fs::read(&ck_path).ok().and_then(|v| Checkpoint::decode(&v)) {
        let same = ck.from == from && ck.layout == o.layout && ck.level == o.level && ck.epoch == store.read(|r| addr_epoch(r, c))? && db.hash_at(c, ck.target)?.map(|h| h.0) == Some(ck.target_hash);
        if same {
            let writers: Result<Vec<Writer>> = ck
                .units
                .iter()
                .map(|(first, last, state)| {
                    let last_hash = db.hash_at(c, *last)?.ok_or_else(|| other("no block at a file's last height"))?.0;
                    Writer::resume(&dir, Meta { kind, chain: c, first: *first, last: *last, last_hash }, o.level, o.threads, state)
                })
                .collect();
            match writers {
                Ok(w) => carried = Some((ck, w)),
                Err(e) => note(format!("index files {tag}: the interrupted pass cannot be carried on ({e}); starting it again")),
            }
        }
    }
    let (target, epoch) = match &carried {
        Some((ck, _)) => (ck.target, ck.epoch),
        None => {
            clear_unfinished(&dir, kind);
            // Settled: deep below the tip, and below where the coins are complete (a tip
            // past that point is wound back to it when the arcade starts).
            let (tip, coins_at) = store.read(|r| Ok((r.get(STATE, &k_state(c, "tip"))?.map(|v| decode_tip(&v)), r.get(STATE, &k_state(c, "coins_at"))?.map(|v| decode_tip(&v)))))?;
            let Some(tip) = tip else { return Ok(Pass::Done) };
            let base = coins_at.map(|t| t.height.min(tip.height)).unwrap_or(tip.height);
            let t = match o.layout.sealed(base) {
                Some(t) if next <= t => t,
                _ => return Ok(Pass::Done),
            };
            (t, store.read(|r| addr_epoch(r, c))?)
        }
    };
    if let Some(why) = store.read(|r| not_ready(r, c, kind, target, Some(epoch)))? {
        return Err(other(format!("{tag}: {why}; convert it afterwards")));
    }
    let hs = Heights::load(db, c, target)?;
    let (mut out, mut scanned) = match carried {
        Some((ck, writers)) => {
            note(format!("index files {tag}: carrying on at {} table rows", ck.scanned));
            (Out { kind, writers, group_key: Vec::new(), group: Vec::new(), group_src: None, done_src: ck.resume }, ck.scanned)
        }
        None => {
            let writers = o
                .layout
                .units(next, target)
                .into_iter()
                .map(|(first, last)| Writer::create(&dir, Meta { kind, chain: c, first, last, last_hash: hs.hashes[last as usize] }, o.level, o.threads))
                .collect::<Result<Vec<_>>>()?;
            (Out { kind, writers, group_key: Vec::new(), group: Vec::new(), group_src: None, done_src: None }, 0u64)
        }
    };
    note(format!("index files {tag}: heights {next}..={target} into {} file(s)", out.writers.len()));

    // The scan. Rows at or before `resumed` were written before the checkpoint (a table
    // with several values per key hands some of them over again after a restart).
    let pre = k_chain(c);
    let chunk = o.chunk_rows.max(64);
    let resumed = out.done_src.clone();
    let mut after = resumed.clone();
    let mut last_ck = scanned;
    loop {
        let n = store.read(|r| {
            let rows = r.scan_after(table, &pre, after.as_deref(), chunk)?;
            for (k, v) in &rows {
                if resumed.as_ref().is_some_and(|s| k <= s) {
                    continue;
                }
                let gk = k.get(2..2 + group_len).ok_or(StoreError::Corrupt { table, why: "short key" })?;
                if gk != out.group_key.as_slice() {
                    out.flush_group()?;
                    out.group_key = gk.to_vec();
                }
                out.group_src = Some(k.clone());
                if let Some(row) = convert_row(r, c, kind, &hs, next, k, v)? {
                    out.group.push(row);
                }
            }
            if let Some(l) = rows.last() {
                after = Some(l.0.clone());
            }
            Ok(rows.len())
        })?;
        scanned += n as u64;
        if n < chunk {
            break;
        }
        let stopping = stop.load(Ordering::Relaxed);
        if stopping || scanned - last_ck >= o.checkpoint_rows {
            let units = out.writers.iter_mut().map(|w| Ok((w.meta().first, w.meta().last, w.save_state()?))).collect::<Result<Vec<_>>>()?;
            Checkpoint { from, target, target_hash: hs.hashes[target as usize], layout: o.layout, level: o.level, epoch, resume: out.done_src.clone(), scanned, units }.save(&ck_path)?;
            last_ck = scanned;
            note(format!("index files {tag}: {scanned} table rows read, {} written", out.writers.iter().map(|w| w.rows()).sum::<u64>()));
            if stopping {
                return Ok(Pass::Stopped);
            }
        }
    }
    out.flush_group()?;

    // Finish every file and read each one back whole before anything relies on it.
    let mut rows = 0u64;
    let mut bytes = 0u64;
    for w in out.writers {
        let info = w.finish()?;
        Reader::open(&info.path)?.verify()?;
        rows += info.rows;
        bytes += info.bytes;
    }
    let _ = std::fs::remove_file(&ck_path);
    note(format!("index files {tag}: {rows} rows in {bytes} bytes, read back and checked"));
    merge_eras(&dir, kind, &o.layout, target, o.level, o.threads, note)?;

    // Switch the readers, unless the chain changed under the files meanwhile (it cannot,
    // that deep; checked all the same, in the transaction that sets the marker).
    db.wstore(c)?.write(|w| {
        let tip = w.get(STATE, &k_state(c, "tip"))?.map(|v| decode_tip(&v));
        let same = w.get(HEIGHT, &k_height(c, target))?.is_some_and(|v| v[..32] == hs.hashes[target as usize]);
        if !same || tip.is_none_or(|t| (t.height as u64) < target as u64 + o.layout.depth as u64) {
            return Err(other(format!("the chain changed at or below height {target} while its files were written; nothing was switched")));
        }
        if let Some(why) = not_ready(w, c, kind, target, Some(epoch))? {
            return Err(other(format!("{tag}: {why}; the files were written but nothing was switched")));
        }
        let gen = marker(w, c, kind)?.filter(|m| m.len() >= 8).map(|m| be_u32(&m, 4)).unwrap_or(0).wrapping_add(1);
        w.put(STATE, &k_state(c, &marker_name(kind)), &[target.to_be_bytes(), gen.to_be_bytes()].concat())
    })?;
    // The marker must be on disk before any file it no longer uses goes: chain.mdbx does not
    // flush every commit (SafeNoSync), and a power cut that lost the marker would leave the
    // old one pointing at deleted ranges.
    db.wstore(c)?.sync()?;
    // Ranges now inside an era file are no longer read.
    db.ixsets.lock().unwrap().remove(&(c, kind.tag()));
    let files = list(&dir, kind)?;
    for (a, b, path) in &files {
        if files.iter().any(|(a2, b2, _)| a2 <= a && b <= b2 && (a2, b2) != (a, b) && *b2 <= target) {
            let _ = std::fs::remove_file(path);
        }
    }
    note(format!("index files {tag}: on up to height {target}"));
    Ok(Pass::Done)
}

/// Merge each era's ranges into the era's file, where every range is there and settled.
/// The ranges stay until the readers have switched (the caller removes them).
fn merge_eras(dir: &Path, kind: Kind, layout: &Layout, upto: u32, level: i32, threads: usize, note: &mut dyn FnMut(String)) -> Result<()> {
    let files = list(dir, kind)?;
    let mut first = 0u64;
    while first + layout.era() - 1 <= upto as u64 {
        let last = first + layout.era() - 1;
        if !files.iter().any(|f| (f.0 as u64, f.1 as u64) == (first, last)) {
            let parts: Vec<&PathBuf> = (0..layout.era_ranges as u64)
                .filter_map(|i| {
                    let a = first + i * layout.range as u64;
                    files.iter().find(|f| (f.0 as u64, f.1 as u64) == (a, a + layout.range as u64 - 1)).map(|f| &f.2)
                })
                .collect();
            if parts.len() == layout.era_ranges as usize {
                let readers = parts.iter().map(|p| Reader::open(p)).collect::<Result<Vec<_>>>()?;
                let info = ixfile::merge(dir, &readers, level, threads)?;
                Reader::open(&info.path)?.verify()?;
                note(format!("index files {}: era {first}..={last} merged from its ranges ({} rows)", kind.tag() as char, info.rows));
            }
        }
        first += layout.era();
    }
    Ok(())
}

impl ChainDb {
    /// Convert chain `params`'s settled rows to index files and switch its readers to them,
    /// kind by kind (tx index, spends, address history; the last two only while the
    /// Addresses index is on). The tables are left as they are. Returns false when stopped
    /// part way (`stop`); run it again to carry on. Running it again later converts what
    /// has settled since.
    pub fn convert_to_index_files(&self, params: &ChainParams, o: &Options, stop: &AtomicBool, note: &mut dyn FnMut(String)) -> Result<bool> {
        let c = params.chain_no;
        if o.layout.range == 0 || o.layout.era_ranges == 0 || o.layout.depth < UNDO_DEPTH {
            return Err(other(format!("a range must have heights, an era ranges, and the sealing depth be at least {UNDO_DEPTH}")));
        }
        // Rows still in the address log or the tx index runs are in no table yet.
        if self.addresses_pending(c)? || self.txs_pending(c)? || crate::txruns::of(self.store(c).id(), c).is_some() {
            return Err(other("this chain's address rows or tx index are still being built; convert it afterwards"));
        }
        let mut kinds = vec![Kind::Txs];
        if self.addresses_on(c)? {
            kinds.extend([Kind::Spends, Kind::Hist]);
        }
        for kind in kinds {
            if let Pass::Stopped = pass(self, params, kind, o, stop, note)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Per kind with a marker: (kind, sealed height, files, rows, bytes).
    pub fn index_files_info(&self, c: u16) -> Result<Vec<(Kind, u32, usize, u64, u64)>> {
        self.store(c).read(|r| {
            let mut out = Vec::new();
            for kind in Kind::ALL {
                if let Some(set) = self.ix(r, c, kind)? {
                    out.push((kind, set.upto, set.files.len(), set.files.iter().map(|f| f.rows).sum(), set.files.iter().map(|f| f.bytes).sum()));
                }
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_cuts() {
        let l = Layout { range: 100, era_ranges: 10, depth: 1000 };
        assert_eq!(l.sealed(999), None);
        assert_eq!(l.sealed(1098), None, "0..=99 is not 1000 deep until the tip is 1099");
        assert_eq!(l.sealed(1099), Some(99));
        assert_eq!(l.sealed(1198), Some(99));
        assert_eq!(l.sealed(1199), Some(199));
        assert_eq!(Layout::STANDARD.sealed(20_999), Some(19_999));
        assert_eq!(Layout::STANDARD.sealed(20_998), Some(9_999));
        assert_eq!(l.units(0, 299), [(0, 99), (100, 199), (200, 299)]);
        assert_eq!(l.units(0, 1299), [(0, 999), (1000, 1099), (1100, 1199), (1200, 1299)]);
        assert_eq!(l.units(300, 2099), [(300, 399), (400, 499), (500, 599), (600, 699), (700, 799), (800, 899), (900, 999), (1000, 1999), (2000, 2099)]);
        assert!(l.units(300, 299).is_empty());
    }

    #[test]
    fn checkpoint_round_trip() {
        let ck = Checkpoint {
            from: Some(99),
            target: 1299,
            target_hash: [7; 32],
            layout: Layout { range: 100, era_ranges: 10, depth: 1000 },
            level: -3,
            epoch: 4,
            resume: Some(vec![1, 2, 3]),
            scanned: 1 << 40,
            units: vec![(100, 199, vec![9; 50]), (200, 299, Vec::new())],
        };
        let v = ck.encode();
        let back = Checkpoint::decode(&v).unwrap();
        assert_eq!((back.from, back.target, back.target_hash, back.layout, back.level, back.epoch, back.resume, back.scanned, back.units), (ck.from, ck.target, ck.target_hash, ck.layout, ck.level, ck.epoch, ck.resume.clone(), ck.scanned, ck.units.clone()));
        let none = Checkpoint { from: None, resume: None, ..ck };
        let back = Checkpoint::decode(&none.encode()).unwrap();
        assert_eq!((back.from, back.resume), (None, None));
        let mut bad = v.clone();
        bad[20] ^= 1;
        assert!(Checkpoint::decode(&bad).is_none());
        assert!(Checkpoint::decode(&v[..v.len() - 1]).is_none());
        assert!(Checkpoint::decode(&[]).is_none());
    }
}
