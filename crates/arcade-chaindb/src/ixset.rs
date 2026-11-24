//! A chain's index files as the readers use them (docs/INDEXFILES.md §4).
//!
//! Per kind, a STATE marker `ixf:<H|P|T>` says how far the files reach: heights at or below
//! it are answered from the files, heights above it from the MDBX tables. No marker (every
//! chain until its operator converts it) is exactly the behaviour before index files.
//!
//! The marker is only ever set to a height at least `UNDO_DEPTH` (1000) blocks below the
//! tip, the same depth beyond which this database cannot disconnect a block at all (there
//! is no undo record). So a reorg never reaches a height the files cover, and the files
//! never need rewriting.

use crate::db::{ChainDb, Rd};
use crate::ixfile::{self, Kind, Reader};
use crate::schema::*;
use arcade_store::key::be_u32;
use arcade_store::{Result, StoreError};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The directory of a chain's index files, inside the chain's own directory.
pub const DIR: &str = "index";

/// The STATE name of a kind's marker: sealed height u32 BE | generation u32 BE (the
/// generation changes whenever the set of files does, so open sets are loaded again).
pub fn marker_name(kind: Kind) -> String {
    format!("ixf:{}", kind.tag() as char)
}

/// The STATE name of a kind's last generation. Unlike the marker it is never deleted
/// (only with the chain), so a marker set again after `off` never repeats an old value.
pub fn generation_name(kind: Kind) -> String {
    format!("ixf:gen:{}", kind.tag() as char)
}

pub(crate) fn marker<R: Rd>(r: &R, c: u16, kind: Kind) -> Result<Option<Vec<u8>>> {
    r.get(STATE, &k_state(c, &marker_name(kind)))
}

/// The STATE name of a kind's "tables trimmed" flag: set once the kind's converted rows are
/// removed from its table, so only the files hold them. Until then (all of this part: no
/// step removes rows yet) a tx index or spends lookup that misses the table never reads a
/// file, since the files only hold rows the table has too. History reads its files from
/// the marker on: they split the answer at the sealed height.
pub fn trimmed_name(kind: Kind) -> String {
    format!("ixf:trim:{}", kind.tag() as char)
}

/// STATE: a counter changed whenever the Addresses index is turned off or on. A conversion
/// of history or spends records it when it starts and switches no reader if it changed:
/// the tables it read may have been emptied or rebuilt under it.
pub const ADDR_EPOCH: &str = "ixf:addr_epoch";

pub(crate) fn addr_epoch<R: Rd>(r: &R, c: u16) -> Result<u32> {
    Ok(r.get(STATE, &k_state(c, ADDR_EPOCH))?.filter(|v| v.len() >= 4).map(|v| be_u32(&v, 0)).unwrap_or(0))
}

/// Change the Addresses epoch, and stop answering history and spends from files: with the
/// index off their tables are emptied (files alone would give a part of the answer), and
/// when it comes back the rows are built again before a new conversion. Callers clear the
/// chain's cached sets after the write ([`ChainDb::forget_ix`]).
pub(crate) fn addresses_switched(w: &arcade_store::WriteTxn, c: u16) -> Result<()> {
    let e = addr_epoch(w, c)?.wrapping_add(1);
    w.put(STATE, &k_state(c, ADDR_EPOCH), &e.to_be_bytes())?;
    for k in [Kind::Hist, Kind::Spends] {
        w.del(STATE, &k_state(c, &marker_name(k)))?;
        w.del(STATE, &k_state(c, &trimmed_name(k)))?;
    }
    Ok(())
}

/// The files of one kind in `dir`: (first, last, path), in order of first then last.
pub fn list(dir: &Path, kind: Kind) -> Result<Vec<(u32, u32, PathBuf)>> {
    let mut out = Vec::new();
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(StoreError::Other(format!("index files in {}: {e}", dir.display()))),
    };
    for e in rd.flatten() {
        if let Some((k, first, last)) = e.file_name().to_str().and_then(ixfile::parse_name) {
            if k == kind && first <= last {
                out.push((first, last, e.path()));
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The files that tile heights `0..=upto` with no gap or overlap: at each height the file
/// reaching furthest (an era before the ranges it was merged from).
pub fn tiling(files: &[(u32, u32, PathBuf)], upto: u32) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut next = 0u64;
    while next <= upto as u64 {
        let Some(f) = files.iter().filter(|f| f.0 as u64 == next && f.1 <= upto).max_by_key(|f| f.1) else {
            return Err(StoreError::Other(format!("index files: none starts at height {next} (they should reach {upto})")));
        };
        out.push(f.2.clone());
        next = f.1 as u64 + 1;
    }
    Ok(out)
}

/// The mark beside a file that was read back whole here (`Reader::verify`): `<file>.ok`, holding
/// the file's id in hex. A file without a matching mark (copied in from elsewhere, or written by
/// a build before marks) is verified whole before a reader adopts it ([`KindSet::load`]).
pub fn checked_mark(path: &Path) -> PathBuf {
    let mut p = path.as_os_str().to_owned();
    p.push(".ok");
    PathBuf::from(p)
}

/// Mark `path` as read back whole here (best effort: without the mark it is only verified again).
pub fn mark_checked(path: &Path, id: &[u8; 32]) {
    let _ = std::fs::write(checked_mark(path), hex::encode(id));
}

/// Whether `path` carries the mark of a whole read of this very file (its id).
pub fn is_checked(path: &Path, id: &[u8; 32]) -> bool {
    std::fs::read(checked_mark(path)).is_ok_and(|v| v == hex::encode(id).as_bytes())
}

/// Remove a file and its mark.
pub fn remove_with_mark(path: &Path) -> std::io::Result<()> {
    let _ = std::fs::remove_file(checked_mark(path));
    std::fs::remove_file(path)
}

/// One kind's open files for a chain, lowest heights first.
pub struct KindSet {
    pub kind: Kind,
    /// The sealed height: the files answer for heights up to and including it.
    pub upto: u32,
    pub files: Vec<Reader>,
}

impl KindSet {
    /// Open the files covering `0..=upto`; `hash_at` is the main chain's block hash at a
    /// height, and every file must end on it (files of another chain or fork are refused).
    /// A file not read back whole here (no matching [`checked_mark`]: copied in from another
    /// arcade, or written before marks) is verified whole first, and marked.
    ///
    /// Every file must be cut by one layout (format 3 pins it in the header) on its boundaries,
    /// and by `layout` (range, ranges per era) where given: the chain's own, so files cut
    /// another way, which other arcades' would never equal, are refused.
    pub fn load(dir: &Path, kind: Kind, chain: u16, upto: u32, layout: Option<(u32, u32)>, mut hash_at: impl FnMut(u32) -> Result<Option<[u8; 32]>>) -> Result<KindSet> {
        let mut files: Vec<Reader> = Vec::new();
        for path in tiling(&list(dir, kind)?, upto)? {
            let r = Reader::open(&path)?;
            let named = path.file_name().and_then(|n| n.to_str()).and_then(ixfile::parse_name);
            if named != Some((r.meta.kind, r.meta.first, r.meta.last)) || r.meta.chain != chain {
                return Err(StoreError::Other(format!("index file {}: its header is of another file", path.display())));
            }
            let cut = (r.meta.range, r.meta.era_ranges);
            if !r.meta.on_layout() || layout.is_some_and(|l| l != cut) || files.first().is_some_and(|f| (f.meta.range, f.meta.era_ranges) != cut) {
                return Err(StoreError::Other(format!("index file {}: not cut by this chain's layout (ranges of {}, eras of {})", path.display(), cut.0, cut.1)));
            }
            if hash_at(r.meta.last)? != Some(r.meta.last_hash) {
                return Err(StoreError::Other(format!("index file {}: not of this chain (the block at {} differs)", path.display(), r.meta.last)));
            }
            if !is_checked(&path, &r.id) {
                r.verify()?;
                mark_checked(&path, &r.id);
            }
            files.push(r);
        }
        Ok(KindSet { kind, upto, files })
    }

    /// An address's history rows in the files: (height, tx position, received, spent), in
    /// height then position order. One block read per file, more only for a long history.
    pub fn history(&self, sid: &[u8; 21]) -> Result<Vec<(u32, u32, u64, u64)>> {
        let mut out = Vec::new();
        for f in &self.files {
            f.scan_prefix(sid, |r| out.push((be_u32(r, 21), be_u32(r, 25), arcade_store::key::be_u64(r, 29), arcade_store::key::be_u64(r, 37))))?;
        }
        Ok(out)
    }

    /// How many history rows an address has in the files.
    pub fn history_len(&self, sid: &[u8; 21]) -> Result<usize> {
        let mut n = 0usize;
        for f in &self.files {
            f.scan_prefix(sid, |_| n += 1)?;
        }
        Ok(n)
    }

    /// Where transactions whose id starts with these 8 bytes are: (height, position), in
    /// order. Usually one; the caller confirms the full txid on the stored transaction.
    pub fn tx_candidates(&self, prefix: &[u8]) -> Result<Vec<(u32, u32)>> {
        let mut out = Vec::new();
        for f in &self.files {
            f.scan_prefix(&prefix[..8], |r| out.push((be_u32(r, 8), be_u32(r, 12))))?;
        }
        Ok(out)
    }

    /// Who spent the coin made at (height, position, vout): (height, position, input) of
    /// the spender. A spend is filed under the spender's height, so only files that end at
    /// or after the coin's height can hold it; the first hit ends the search.
    pub fn spender(&self, height: u32, pos: u32, vout: u32) -> Result<Option<(u32, u32, u32)>> {
        let key = ixfile::spend_row(height, pos, vout, 0, 0, 0);
        for f in self.files.iter().filter(|f| f.meta.last >= height) {
            if let Some(r) = f.get(&key[..12])? {
                return Ok(Some((be_u32(&r, 12), be_u32(&r, 16), be_u32(&r, 20))));
            }
        }
        Ok(None)
    }
}

/// Open sets, by (chain, kind), each with the marker value it was loaded for.
pub(crate) type Cache = std::sync::Mutex<std::collections::HashMap<(u16, u8), (Vec<u8>, Arc<KindSet>)>>;

impl ChainDb {
    /// Where chain `c`'s index files are.
    pub fn index_dir(&self, c: u16) -> PathBuf {
        self.own_dir(c).join(DIR)
    }

    /// The height up to which chain `c`'s rows of a kind are answered from index files
    /// (None: all from the tables, as before index files).
    pub fn index_files_upto(&self, c: u16, kind: Kind) -> Result<Option<u32>> {
        Ok(self.store(c).read(|r| marker(r, c, kind))?.map(|m| be_u32(&m, 0)))
    }

    /// Whether any kind of chain `c` is answered from index files.
    pub fn has_index_files(&self, c: u16) -> Result<bool> {
        self.store(c).read(|r| {
            for k in Kind::ALL {
                if marker(r, c, k)?.is_some() {
                    return Ok(true);
                }
            }
            Ok(false)
        })
    }

    /// Stop answering from index files: every row comes from the tables again. Only right
    /// while the tables still hold the converted rows (nothing removes them yet). The files
    /// stay on disk.
    pub fn set_index_files_off(&self, c: u16) -> Result<()> {
        self.wstore(c)?.write(|w| {
            for k in Kind::ALL {
                w.del(STATE, &k_state(c, &marker_name(k)))?;
                w.del(STATE, &k_state(c, &trimmed_name(k)))?;
            }
            Ok(())
        })?;
        self.forget_ix(c);
        Ok(())
    }

    /// Drop chain `c`'s open file sets (they are loaded again from the marker when needed).
    pub(crate) fn forget_ix(&self, c: u16) {
        self.ixsets.lock().unwrap().retain(|(cc, _), _| *cc != c);
    }

    /// Chain `c`'s files of a kind when they are the only place its settled rows are (the
    /// table was trimmed): what tx index and spends lookups use after a table miss. None
    /// while the table still holds every row (all of this part).
    pub(crate) fn ix_trimmed<R: Rd>(&self, r: &R, c: u16, kind: Kind) -> Result<Option<Arc<KindSet>>> {
        if r.get(STATE, &k_state(c, &trimmed_name(kind)))?.is_none() {
            return Ok(None);
        }
        self.ix(r, c, kind)
    }

    /// Chain `c`'s open files of a kind, as of the snapshot `r` (None: no marker). The
    /// marker is read in the caller's transaction, so the files and the table rows above
    /// them are one consistent view.
    pub(crate) fn ix<R: Rd>(&self, r: &R, c: u16, kind: Kind) -> Result<Option<Arc<KindSet>>> {
        let Some(m) = marker(r, c, kind)? else { return Ok(None) };
        if m.len() < 4 {
            return Err(StoreError::Corrupt { table: STATE, why: "bad index files marker" });
        }
        let cached = |m: &[u8]| self.ixsets.lock().unwrap().get(&(c, kind.tag())).filter(|(have, _)| have.as_slice() == m).map(|(_, set)| set.clone());
        if let Some(set) = cached(&m) {
            return Ok(Some(set));
        }
        // Loaded outside the lock (opening a big chain's files reads tens of MB of footers;
        // other chains and kinds keep reading meanwhile). Two readers may both load; the
        // first one stored wins.
        // The chain's own layout (the tests convert with small ones: any one layout there).
        let layout = arcade_chain::params::ALL_CHAINS.iter().find(|p| p.chain_no == c).map(|p| crate::ixconv::Layout::for_chain(p)).filter(|_| !cfg!(test)).map(|l| (l.range, l.era_ranges));
        let set = Arc::new(KindSet::load(&self.index_dir(c), kind, c, be_u32(&m, 0), layout, |h| {
            let v = r.get(HEIGHT, &k_height(c, h))?;
            Ok(v.and_then(|v| v.get(..32).and_then(|x| x.try_into().ok())))
        })?);
        let mut cache = self.ixsets.lock().unwrap();
        if let Some((have, won)) = cache.get(&(c, kind.tag())) {
            if *have == m {
                return Ok(Some(won.clone()));
            }
        }
        cache.insert((c, kind.tag()), (m, set.clone()));
        Ok(Some(set))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tiling_prefers_eras_and_refuses_gaps() {
        let f = |a: u32, b: u32| (a, b, PathBuf::from(format!("{a}-{b}")));
        let names = |v: Vec<PathBuf>| v.iter().map(|p| p.to_str().unwrap().to_string()).collect::<Vec<_>>();
        let mut files = vec![f(0, 99), f(100, 199), f(200, 299), f(0, 999), f(1000, 1099), f(1100, 1199)];
        files.sort();
        assert_eq!(names(tiling(&files, 1199).unwrap()), ["0-999", "1000-1099", "1100-1199"]);
        assert_eq!(names(tiling(&files, 1099).unwrap()), ["0-999", "1000-1099"]);
        // Up to 299 the era reaches too far: the ranges answer.
        assert_eq!(names(tiling(&files, 299).unwrap()), ["0-99", "100-199", "200-299"]);
        assert!(tiling(&files, 1299).is_err(), "nothing covers 1200..");
        assert!(tiling(&files[..2], 299).is_err());
        assert!(tiling(&[], 99).is_err());
    }
}
