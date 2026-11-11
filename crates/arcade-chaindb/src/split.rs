//! Moving a chain out of the shared `chain.mdbx` into its own file (docs/SPLITDB.md).
//!
//! Every row of the chain (every key starts with its 2-byte chain number) is copied in key
//! order with appends into `chains/<key>/chain.mdbx.part`, in transactions of about 256 MB,
//! from one read snapshot of the shared file. The copy is then checked (rows per table, first
//! and last key per table, STATE row for row) and renamed into place. The shared file is
//! only read: its rows stay until the person deletes the shared file.
//!
//! Run it with the arcade stopped (the tool takes the home's lock).

use crate::schema::*;
use arcade_store::{Result, Store, StoreError, TableDef};
use std::path::{Path, PathBuf};

/// Bytes (keys plus values) per write transaction.
const TXN_BYTES: u64 = 256 << 20;

#[derive(Debug, Clone)]
pub struct TableCopy {
    pub table: String,
    pub rows: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone)]
pub struct SplitReport {
    pub chain: u16,
    pub tables: Vec<TableCopy>,
    pub secs: f64,
    /// Size of the new file.
    pub file_bytes: u64,
}

impl SplitReport {
    pub fn rows(&self) -> u64 {
        self.tables.iter().map(|t| t.rows).sum()
    }
    pub fn bytes(&self) -> u64 {
        self.tables.iter().map(|t| t.bytes).sum()
    }
}

fn other(s: String) -> StoreError {
    StoreError::Other(s)
}

/// The chains that have rows in a shared file (by its STATE, headers and blocks).
pub fn chains_in(shared: &Store) -> Result<Vec<u16>> {
    let mut out = Vec::new();
    shared.read(|r| {
        for c in 0..=u16::MAX {
            let pre = k_chain(c);
            for t in [STATE, HEADERS, BLOCK_META] {
                if !r.scan_prefix_limit(t, &pre, 1)?.is_empty() {
                    out.push(c);
                    break;
                }
            }
        }
        Ok(())
    })?;
    Ok(out)
}

/// Every table that holds rows of chain `c` in the shared file: (name, dup).
fn tables_of(shared: &Store, c: u16) -> Result<Vec<(String, bool)>> {
    let pre = k_chain(c);
    let mut v: Vec<(String, bool)> = CHAIN_SCHEMA.tables.iter().map(|t| (t.name.to_string(), t.dup)).collect();
    for (t, dup) in shared.addon_defs()? {
        if !shared.read(|r| r.scan_prefix_limit(&t, &pre, 1))?.is_empty() {
            v.push((t, dup));
        }
    }
    Ok(v)
}

fn first_last(s: &Store, t: &str, pre: &[u8]) -> Result<(Option<Vec<u8>>, Option<Vec<u8>>)> {
    s.read(|r| {
        let first = r.scan_prefix_limit(t, pre, 1)?.into_iter().next().map(|(k, _)| k);
        let last = r.last_with_prefix(t, pre)?.map(|(k, _)| k);
        Ok((first, last))
    })
}

fn lck_of(p: &Path) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push("-lck");
    PathBuf::from(s)
}

/// Copy chain `c` from the shared file into `own` (its own file), check it, and rename it into
/// place. `own` must not exist. A `.part` file left by an interrupted copy is started over.
pub fn split_chain(shared: &Store, own: &Path, c: u16, say: &dyn Fn(&str)) -> Result<SplitReport> {
    if own.exists() {
        return Err(other(format!("{} already exists", own.display())));
    }
    let t0 = std::time::Instant::now();
    let dir = own.parent().ok_or_else(|| other("no directory".into()))?;
    std::fs::create_dir_all(dir).map_err(|e| other(format!("{}: {e}", dir.display())))?;
    let part = own.with_extension("mdbx.part");
    for p in [part.clone(), lck_of(&part)] {
        if p.exists() {
            std::fs::remove_file(&p).map_err(|e| other(format!("{}: {e}", p.display())))?;
        }
    }
    let pre = k_chain(c);
    let tables = tables_of(shared, c)?;
    let mut copies = Vec::new();
    {
        // Durable: each transaction of the copy is on disk when it commits.
        let new = Store::open(&part, &CHAIN_SCHEMA)?;
        for (t, dup) in &tables {
            if !CHAIN_SCHEMA.tables.iter().any(|d| d.name == t.as_str()) {
                let name: &'static str = Box::leak(t.clone().into_boxed_str());
                new.add_table(TableDef { name, dup: *dup })?;
            }
        }
        for (t, dup) in &tables {
            let (mut rows, mut bytes, mut in_txn) = (0u64, 0u64, 0u64);
            let mut next_note = 10u64 << 30;
            let mut w = Some(new.begin_write()?);
            shared.read(|r| {
                r.for_each_prefix(t, &pre, |k, v| {
                    let wt = w.as_ref().unwrap();
                    if *dup {
                        wt.append_dup(t, k, v)?;
                    } else {
                        wt.append(t, k, v)?;
                    }
                    let n = (k.len() + v.len()) as u64;
                    rows += 1;
                    bytes += n;
                    in_txn += n;
                    if in_txn >= TXN_BYTES {
                        w.take().unwrap().commit()?;
                        w = Some(new.begin_write()?);
                        in_txn = 0;
                    }
                    if bytes >= next_note {
                        say(&format!("  {t}: {rows} rows, {:.1} GB so far ({:.0} s)", bytes as f64 / 1e9, t0.elapsed().as_secs_f64()));
                        next_note += 10 << 30;
                    }
                    Ok(true)
                })
            })?;
            w.take().unwrap().commit()?;
            if rows > 0 {
                say(&format!("  {t}: {rows} rows, {:.2} GB ({:.0} s)", bytes as f64 / 1e9, t0.elapsed().as_secs_f64()));
            }
            copies.push(TableCopy { table: t.clone(), rows, bytes });
        }
        // The check: the same rows per table, the same first and last key, STATE row for row.
        for tc in &copies {
            let n = new.count(&tc.table)?;
            if n != tc.rows {
                return Err(other(format!("{}: {n} rows in the copy, {} read", tc.table, tc.rows)));
            }
            if first_last(shared, &tc.table, &pre)? != first_last(&new, &tc.table, &pre)? {
                return Err(other(format!("{}: first or last key differs in the copy", tc.table)));
            }
        }
        let a = shared.read(|r| r.scan_prefix(STATE, &pre))?;
        let b = new.read(|r| r.scan_prefix(STATE, &pre))?;
        if a != b {
            return Err(other(format!("STATE differs in the copy ({} rows read, {} copied)", a.len(), b.len())));
        }
    }
    std::fs::rename(&part, own).map_err(|e| other(format!("{} -> {}: {e}", part.display(), own.display())))?;
    let _ = std::fs::remove_file(lck_of(&part));
    let file_bytes = std::fs::metadata(own).map(|m| m.len()).unwrap_or(0);
    Ok(SplitReport { chain: c, tables: copies, secs: t0.elapsed().as_secs_f64(), file_bytes })
}
