//! Inscriptions by content-type group, in number order, so the Explorer's type filter reads
//! one page of rows instead of opening every inscription's record (docs/ORDTYPES.md).
//!
//! Table `ord.v1.types` (C = chain number):
//!   C | g u8 | seq u64 -> number u64   the g-th group's inscriptions, oldest first (seq 0, 1, ...)
//!   C | 0xFF          -> 9 x u64 next seq per group, then top u64 (1 + the highest number in the rows)
//! The 0xFF row is also the mark that the table is built: until it exists the ord indexer
//! writes no rows here and the Explorer pages the old way.
//!
//! The ord indexer adds a row for each new inscription in the block's own commit, so a reorg
//! undoes it with the block. The rows for inscriptions from before the table existed are
//! built in one pass ([`build`]) without undo records; a reorg that disconnects one of those
//! blocks leaves rows for numbers that no longer exist, so the next block first drops every
//! row whose number is at or above the chain's next number ([`trim`]), and reads skip them.
//!
//! The table is derived from `ord.v1.insc`: index copies don't carry it, each arcade builds it.

use arcade_chaindb::undo::Logged;
use arcade_chaindb::ChainDb;
use arcade_store::key::be_u64;
use arcade_store::{KeyBuf, Result};

pub const TYPES: &str = "ord.v1.types";
/// The groups, in the order of their u8 in the table. Stored: only add at the end.
pub const GROUP_NAMES: [&str; 9] = ["image", "svg", "text", "html", "json", "audio", "video", "model", "other"];
const N: usize = GROUP_NAMES.len();
const META: u8 = 0xFF;
/// Rows for the newest inscriptions are written at the end of [`build`], in the same write as
/// the mark, read from the records as they are then (a reorg during the build is covered).
const TAIL: u64 = 20_000;

/// The content-type group of `ct` (an index into [`GROUP_NAMES`]).
pub fn group_of(ct: &str) -> u8 {
    let b = ct.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    let g = if b == "image/svg+xml" {
        "svg"
    } else if b.starts_with("image/") {
        "image"
    } else if b == "text/html" {
        "html"
    } else if b == "application/json" || b.ends_with("+json") {
        "json"
    } else if b.starts_with("text/") {
        "text"
    } else if b.starts_with("audio/") {
        "audio"
    } else if b.starts_with("video/") {
        "video"
    } else if b.starts_with("model/") {
        "model"
    } else {
        "other"
    };
    GROUP_NAMES.iter().position(|x| *x == g).unwrap() as u8
}

pub fn group_index(name: &str) -> Option<u8> {
    GROUP_NAMES.iter().position(|g| *g == name).map(|i| i as u8)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Meta {
    pub next: [u64; N],
    pub top: u64,
}

impl Meta {
    fn enc(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 * (N + 1));
        for x in self.next {
            v.extend_from_slice(&x.to_be_bytes());
        }
        v.extend_from_slice(&self.top.to_be_bytes());
        v
    }
    fn dec(v: &[u8]) -> Option<Meta> {
        if v.len() < 8 * (N + 1) {
            return None;
        }
        let mut next = [0u64; N];
        for (i, x) in next.iter_mut().enumerate() {
            *x = be_u64(v, i * 8);
        }
        Some(Meta { next, top: be_u64(v, N * 8) })
    }
}

fn meta_key(c: u16) -> Vec<u8> {
    KeyBuf::new().u16(c).u8(META).done()
}

fn row_key(c: u16, g: u8, seq: u64) -> Vec<u8> {
    KeyBuf::new().u16(c).u8(g).u64(seq).done()
}

/// The table's counters, or None while it isn't built on this chain.
pub fn meta(db: &ChainDb, c: u16) -> Option<Meta> {
    db.store(c).read(|r| r.get(TYPES, &meta_key(c))).ok().flatten().and_then(|v| Meta::dec(&v))
}

/// In the ord indexer's commit, before the block's inscriptions: the counters (None = not
/// built), with the rows made to match the inscriptions the chain has: rows at or above
/// `next` (left by a reorg of a block from before the build) dropped, and rows for numbers
/// between the last row and `next` (inscriptions indexed by an arcade version without this
/// table, after it was built) added from their records.
pub(crate) fn trim(l: &Logged, c: u16, next: u64) -> Result<Option<Meta>> {
    let Some(mut m) = l.get(TYPES, &meta_key(c))?.and_then(|v| Meta::dec(&v)) else { return Ok(None) };
    if m.top < next {
        for num in m.top..next {
            let Some(iid) = l.get(crate::ord::NUM, &KeyBuf::new().u16(c).u64(num).done())? else { continue };
            let Some(v) = l.get(crate::ord::INSC, &KeyBuf::new().u16(c).bytes(&iid).done())? else { continue };
            add(l, c, &mut m, num, &n_and_ct(&v).map(|x| x.1).unwrap_or_default())?;
        }
        m.top = next;
        l.put(TYPES, &meta_key(c), &m.enc())?;
    } else if m.top > next {
        for g in 0..N {
            while m.next[g] > 0 {
                let key = row_key(c, g as u8, m.next[g] - 1);
                match l.get(TYPES, &key)? {
                    Some(v) if be_u64(&v, 0) >= next => {
                        l.del(TYPES, &key)?;
                        m.next[g] -= 1;
                    }
                    _ => break,
                }
            }
        }
        m.top = next;
        l.put(TYPES, &meta_key(c), &m.enc())?;
    }
    Ok(Some(m))
}

/// In the ord indexer's commit: inscription `number` of type `ct` is born.
pub(crate) fn add(l: &Logged, c: u16, m: &mut Meta, number: u64, ct: &str) -> Result<()> {
    let g = group_of(ct);
    l.put(TYPES, &row_key(c, g, m.next[g as usize]), &number.to_be_bytes())?;
    m.next[g as usize] += 1;
    m.top = m.top.max(number + 1);
    Ok(())
}

pub(crate) fn save(l: &Logged, c: u16, m: &Meta) -> Result<()> {
    l.put(TYPES, &meta_key(c), &m.enc())
}

/// Group `g`'s inscription numbers from `offset` (0 = newest), up to `n`, newest first, and
/// the group's total. None while the table isn't built.
pub fn page(db: &ChainDb, c: u16, g: u8, offset: u64, n: u64) -> Option<(Vec<u64>, u64)> {
    let m = meta(db, c)?;
    let total = m.next[g as usize];
    if offset >= total || n == 0 {
        return Some((Vec::new(), total));
    }
    let hi = total - 1 - offset; // seq of the newest row on the page
    let lo = hi.saturating_sub(n - 1);
    let next = crate::ord::count(db, c);
    let pre = KeyBuf::new().u16(c).u8(g).done();
    let after = (lo > 0).then(|| row_key(c, g, lo - 1));
    let rows = db.store(c).read(|r| r.scan_after(TYPES, &pre, after.as_deref(), (hi - lo + 1) as usize)).ok()?;
    let mut out: Vec<u64> = rows.iter().map(|(_, v)| be_u64(v, 0)).filter(|num| *num < next).collect();
    out.reverse();
    Some((out, total))
}

/// Every group's total (None while the table isn't built).
pub fn totals(db: &ChainDb, c: u16) -> Option<[u64; N]> {
    meta(db, c).map(|m| m.next)
}

/// Number and content type from an `ord.v1.insc` record, without parsing the whole JSON.
fn n_and_ct(v: &[u8]) -> Option<(u64, String)> {
    let find = |pat: &[u8]| v.windows(pat.len()).position(|w| w == pat).map(|i| i + pat.len());
    let i = find(b"\"n\":")?;
    let digits: Vec<u8> = v[i..].iter().take_while(|b| b.is_ascii_digit()).copied().collect();
    let n: u64 = std::str::from_utf8(&digits).ok()?.parse().ok()?;
    let ct = match find(b"\"ct\":") {
        Some(j) => serde_json::Deserializer::from_slice(&v[j..]).into_iter::<String>().next()?.ok()?,
        None => String::new(),
    };
    Some((n, ct))
}

/// Build the table on chain `c` from its `ord.v1.insc` records, while the arcade runs. Does
/// nothing when it is built already. One read of every record (in key order, one snapshot),
/// then the rows in key order a batch at a time, then the newest inscriptions and the mark in
/// one write with the chain's current state. Returns the rows written.
pub fn build(db: &ChainDb, c: u16, say: &dyn Fn(&str)) -> Result<u64> {
    build_tail(db, c, TAIL, say)
}

/// [`build`] with the newest `tail` inscriptions done in the final write (tests use a small one).
#[doc(hidden)]
pub fn build_tail(db: &ChainDb, c: u16, tail: u64, say: &dyn Fn(&str)) -> Result<u64> {
    if meta(db, c).is_some() {
        return Ok(0);
    }
    let store = db.wstore(c)?;
    let t0 = std::time::Instant::now();
    let pre = KeyBuf::new().u16(c).done();
    // 1. Every record's group, by number (0 = no record here; else group + 1).
    let (snap_next, groups) = store.read(|r| {
        let next = r.get(crate::ord::STATE, &KeyBuf::new().u16(c).bytes(b"next").done())?.map(|v| be_u64(&v, 0)).unwrap_or(0);
        let mut g = vec![0u8; next as usize];
        let mut seen = 0u64;
        r.for_each_prefix(crate::ord::INSC, &pre, |_, v| {
            if let Some((n, ct)) = n_and_ct(v) {
                if (n as usize) < g.len() {
                    g[n as usize] = group_of(&ct) + 1;
                }
            }
            seen += 1;
            if seen % 20_000_000 == 0 {
                say(&format!("types: read {seen} of {next} inscription records ({:.0} s)", t0.elapsed().as_secs_f64()));
            }
            Ok(true)
        })?;
        Ok((next, g))
    })?;
    say(&format!("types: read {} records ({:.0} s); writing the rows", groups.iter().filter(|x| **x > 0).count(), t0.elapsed().as_secs_f64()));
    // 2. Rows for numbers below the tail, group by group, in key order. Rows left by a build
    //    that was stopped go first. Appends need every key above the table's last one: in a
    //    file shared with chains numbered higher, plain puts.
    loop {
        let n = store.write(|w| {
            let rows = w.scan_prefix_limit(TYPES, &pre, 100_000)?;
            for (k, _) in &rows {
                w.del(TYPES, k)?;
            }
            Ok(rows.len())
        })?;
        if n == 0 {
            break;
        }
    }
    let append = store.read(|r| r.last_with_prefix(TYPES, &[]))?.is_none_or(|(k, _)| k[..] < pre[..]);
    let cut = snap_next.saturating_sub(tail);
    let mut m = Meta { next: [0; N], top: cut };
    let mut written = 0u64;
    for g in 0..N as u8 {
        let mut w = store.begin_write()?;
        let mut in_txn = 0u32;
        for num in 0..cut {
            if groups[num as usize] != g + 1 {
                continue;
            }
            let key = row_key(c, g, m.next[g as usize]);
            if append {
                w.append(TYPES, &key, &num.to_be_bytes())?;
            } else {
                w.put(TYPES, &key, &num.to_be_bytes())?;
            }
            m.next[g as usize] += 1;
            written += 1;
            in_txn += 1;
            if in_txn >= 500_000 {
                w.commit()?;
                w = store.begin_write()?;
                in_txn = 0;
            }
        }
        w.commit()?;
    }
    drop(groups);
    say(&format!("types: {written} rows written ({:.0} s); the newest and the mark", t0.elapsed().as_secs_f64()));
    // 3. The newest inscriptions as they are now, and the mark, in one write. The rows of step
    //    2 can't be appended to from here (a group's rows are followed by the next group's),
    //    so these are plain puts.
    let tail = store.write(|w| {
        let next = w.get(crate::ord::STATE, &KeyBuf::new().u16(c).bytes(b"next").done())?.map(|v| be_u64(&v, 0)).unwrap_or(0);
        let after = (cut > 0).then(|| KeyBuf::new().u16(c).u64(cut - 1).done());
        let rows = w.scan_after(crate::ord::NUM, &pre, after.as_deref(), next.saturating_sub(cut) as usize)?;
        let mut k = 0u64;
        for (key, iid) in rows {
            let num = be_u64(&key, 2);
            if num >= next {
                break;
            }
            let Some(v) = w.get(crate::ord::INSC, &KeyBuf::new().u16(c).bytes(&iid).done())? else { continue };
            let ct = n_and_ct(&v).map(|x| x.1).unwrap_or_default();
            let g = group_of(&ct);
            w.put(TYPES, &row_key(c, g, m.next[g as usize]), &num.to_be_bytes())?;
            m.next[g as usize] += 1;
            k += 1;
        }
        m.top = next;
        w.put(TYPES, &meta_key(c), &m.enc())?;
        Ok(k)
    })?;
    say(&format!("types: built, {} rows in {:.0} s", written + tail, t0.elapsed().as_secs_f64()));
    Ok(written + tail)
}
