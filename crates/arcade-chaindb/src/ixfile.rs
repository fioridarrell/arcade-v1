//! Index files: settled address and tx index rows in immutable, compressed, sorted files
//! (docs/INDEXFILES.md; the design is "A1" of the Bitcoin swarm plan).
//!
//! One file holds the rows of one kind for one span of heights (a range of 10,000 heights,
//! or an era of 100,000). Rows are fixed-size and big-endian, so their bytes sort as their
//! keys do, and they name blocks by HEIGHT and POSITION, never by the local block number:
//! two arcades on the same chain write the same rows, cut the same blocks and so the same
//! file id.
//!
//! ```text
//! header   64 bytes: magic, format, kind, chain, row and key size, rows per block,
//!          first and last height, hash of the block at the last height
//! blocks   a fixed number of rows each (the last one shorter); a block is its columns,
//!          each delta/varint or byte-plane coded and then zstd-compressed on its own
//! footer   per block: first key, file offset, sha256 of the block's canonical rows
//! trailer  row count, block count, footer offset, file id, checksum, end magic
//! ```
//!
//! Hashes are over canonical (uncompressed) rows, so the file id does not depend on the
//! zstd version; with the same zstd version and level the whole file is byte-identical.
//! A file is written under a `.tmp` name, synced, and renamed: a file with its final name
//! is complete.

use arcade_chain::hash::sha256;
use arcade_store::key::{be_u32, be_u64};
use arcade_store::{Result, StoreError};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

pub const MAGIC: &[u8; 8] = b"ARCIXF\r\n";
pub const END_MAGIC: &[u8; 8] = b"ARCIXEND";
/// The format version. A change to the row layouts, the block sizes or the column coding
/// is a new version: files of different versions never mix (their ids differ).
pub const FMT: u16 = 1;
pub const HEADER_LEN: usize = 64;
const TRAILER_LEN: usize = 8 + 4 + 8 + 32 + 32 + 8;
/// The zstd level of sealed files: they are written once and read for years.
pub const LEVEL: i32 = 19;
/// File name ending.
pub const EXT: &str = "ixf";

/// What a file's rows are.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    /// Address history: script id (21) | height | tx position -> received u64 | spent u64.
    Hist,
    /// Spends: created height | position | vout -> spender height | position | vin. Filed
    /// under the SPENDER's height (a spend comes into being with the spending block).
    Spends,
    /// Tx index: first 8 bytes of the txid | height | position (the whole row is the key:
    /// two transactions may share a prefix; a lookup confirms the full txid on the block).
    Txs,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::Hist, Kind::Spends, Kind::Txs];

    pub fn tag(self) -> u8 {
        match self {
            Kind::Hist => b'H',
            Kind::Spends => b'P',
            Kind::Txs => b'T',
        }
    }
    pub fn from_tag(t: u8) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.tag() == t)
    }
    pub fn row_len(self) -> usize {
        match self {
            Kind::Hist => 45,
            Kind::Spends => 24,
            Kind::Txs => 16,
        }
    }
    pub fn key_len(self) -> usize {
        match self {
            Kind::Hist => 29,
            Kind::Spends => 12,
            Kind::Txs => 16,
        }
    }
    /// Rows per block: fixed per kind (not a size target), so every arcade cuts alike.
    pub fn block_rows(self) -> usize {
        match self {
            Kind::Hist => 8192,
            Kind::Spends => 16384,
            Kind::Txs => 4096,
        }
    }
    /// The height a row is filed under (what must lie inside its file's span).
    pub fn height_of(self, row: &[u8]) -> u32 {
        match self {
            Kind::Hist => be_u32(row, 21),
            Kind::Spends => be_u32(row, 12),
            Kind::Txs => be_u32(row, 8),
        }
    }
    fn columns(self) -> usize {
        match self {
            Kind::Hist | Kind::Spends => 6,
            Kind::Txs => 16,
        }
    }
}

/// A history row.
pub fn hist_row(sid: &[u8; 21], height: u32, pos: u32, received: u64, spent: u64) -> [u8; 45] {
    let mut r = [0u8; 45];
    r[..21].copy_from_slice(sid);
    r[21..25].copy_from_slice(&height.to_be_bytes());
    r[25..29].copy_from_slice(&pos.to_be_bytes());
    r[29..37].copy_from_slice(&received.to_be_bytes());
    r[37..45].copy_from_slice(&spent.to_be_bytes());
    r
}

/// A spends row: the coin made at (height, pos, vout) was spent by input `vin` of the
/// transaction at (sheight, spos).
pub fn spend_row(height: u32, pos: u32, vout: u32, sheight: u32, spos: u32, vin: u32) -> [u8; 24] {
    let mut r = [0u8; 24];
    for (i, v) in [height, pos, vout, sheight, spos, vin].into_iter().enumerate() {
        r[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    r
}

/// A tx index row.
pub fn tx_row(prefix: &[u8; 8], height: u32, pos: u32) -> [u8; 16] {
    let mut r = [0u8; 16];
    r[..8].copy_from_slice(prefix);
    r[8..12].copy_from_slice(&height.to_be_bytes());
    r[12..16].copy_from_slice(&pos.to_be_bytes());
    r
}

/// What a file is: fixed when it is created, written in its header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Meta {
    pub kind: Kind,
    /// The chain number (arcade_chain::params; the same on every arcade).
    pub chain: u16,
    /// The span of heights the file covers, both ends included.
    pub first: u32,
    pub last: u32,
    /// The hash of the block at `last`: which chain these rows belong to.
    pub last_hash: [u8; 32],
}

impl Meta {
    pub fn header(&self) -> [u8; HEADER_LEN] {
        let mut h = [0u8; HEADER_LEN];
        h[..8].copy_from_slice(MAGIC);
        h[8..10].copy_from_slice(&FMT.to_be_bytes());
        h[10] = self.kind.tag();
        h[12..14].copy_from_slice(&self.chain.to_be_bytes());
        h[14..16].copy_from_slice(&(self.kind.row_len() as u16).to_be_bytes());
        h[16..18].copy_from_slice(&(self.kind.key_len() as u16).to_be_bytes());
        h[20..24].copy_from_slice(&(self.kind.block_rows() as u32).to_be_bytes());
        h[24..28].copy_from_slice(&self.first.to_be_bytes());
        h[28..32].copy_from_slice(&self.last.to_be_bytes());
        h[32..64].copy_from_slice(&self.last_hash);
        h
    }

    fn parse(h: &[u8]) -> std::result::Result<Meta, &'static str> {
        if h.len() < HEADER_LEN || &h[..8] != MAGIC {
            return Err("not an index file");
        }
        if u16::from_be_bytes([h[8], h[9]]) != FMT {
            return Err("unknown format version");
        }
        let kind = Kind::from_tag(h[10]).ok_or("unknown kind")?;
        let n = |i: usize| u16::from_be_bytes([h[i], h[i + 1]]) as usize;
        if n(14) != kind.row_len() || n(16) != kind.key_len() || be_u32(h, 20) as usize != kind.block_rows() {
            return Err("row layout differs from this format version's");
        }
        let (first, last) = (be_u32(h, 24), be_u32(h, 28));
        if first > last {
            return Err("bad height span");
        }
        Ok(Meta { kind, chain: n(12) as u16, first, last, last_hash: h[32..64].try_into().unwrap() })
    }

    /// The file's name: kind and span, so a directory listing says what is there.
    pub fn file_name(&self) -> String {
        file_name(self.kind, self.first, self.last)
    }
}

pub fn file_name(kind: Kind, first: u32, last: u32) -> String {
    format!("{}-{first:08}-{last:08}.{EXT}", kind.tag() as char)
}

/// (kind, first, last) of a file name written by [`file_name`].
pub fn parse_name(name: &str) -> Option<(Kind, u32, u32)> {
    let stem = name.strip_suffix(EXT)?.strip_suffix('.')?;
    let mut it = stem.split('-');
    let (k, a, b) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() || k.len() != 1 || a.len() != 8 || b.len() != 8 {
        return None;
    }
    Some((Kind::from_tag(k.as_bytes()[0])?, a.parse().ok()?, b.parse().ok()?))
}

fn bad(path: &Path, why: impl std::fmt::Display) -> StoreError {
    StoreError::Other(format!("index file {}: {why}", path.display()))
}

/// Reads fields off the front of a byte string.
pub(crate) struct Take<'a>(pub &'a [u8]);

impl<'a> Take<'a> {
    pub fn bytes(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    pub fn u32(&mut self) -> Option<u32> {
        self.bytes(4).map(|b| be_u32(b, 0))
    }
    pub fn u64(&mut self) -> Option<u64> {
        self.bytes(8).map(|b| be_u64(b, 0))
    }
}

// ---------------- column coding ----------------

fn put_var(o: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        o.push(v as u8 | 0x80);
        v >>= 7;
    }
    o.push(v as u8);
}

fn get_var(b: &[u8], at: &mut usize) -> Option<u64> {
    let (mut v, mut s) = (0u64, 0u32);
    loop {
        let x = *b.get(*at)?;
        *at += 1;
        if s >= 64 {
            return None;
        }
        v |= ((x & 0x7f) as u64) << s;
        if x & 0x80 == 0 {
            return Some(v);
        }
        s += 7;
    }
}

/// `bytes` byte planes of a column of numbers, most significant first: the high planes of
/// small or slowly changing numbers are zeros (stored as nothing), and each remaining plane
/// gets its own entropy table. Better than varints for near-random values (txid prefix
/// gaps, heights and positions in txid order).
fn planes(cols: &mut Vec<Vec<u8>>, vals: &[u64], bytes: usize) {
    for j in 0..bytes {
        let shift = 8 * (bytes - 1 - j);
        cols.push(vals.iter().map(|v| (v >> shift) as u8).collect());
    }
}

fn unplanes(cols: &[Vec<u8>], n: usize, bytes: usize) -> Option<Vec<u64>> {
    let mut out = vec![0u64; n];
    for col in cols.iter().take(bytes) {
        if col.len() != n {
            return None;
        }
        for (o, b) in out.iter_mut().zip(col) {
            *o = (*o << 8) | *b as u64;
        }
    }
    Some(out)
}

/// A block's rows as columns. Wrapping arithmetic throughout: decode is the exact inverse
/// for any input, and the row hash decides whether the result is right.
fn encode(kind: Kind, first: u32, rows: &[u8]) -> Vec<Vec<u8>> {
    let n = rows.len() / kind.row_len();
    match kind {
        Kind::Hist => {
            // new-script flag | the new script ids | height (from the row above when the
            // script is the same, else from the file's first height) | position | received | spent
            let mut c: Vec<Vec<u8>> = (0..6).map(|_| Vec::with_capacity(n * 2)).collect();
            let mut prev: Option<&[u8]> = None;
            for r in rows.chunks_exact(45) {
                let h = be_u32(r, 21);
                match prev {
                    Some(q) if q[..21] == r[..21] => {
                        c[0].push(0);
                        put_var(&mut c[2], h.wrapping_sub(be_u32(q, 21)) as u64);
                    }
                    _ => {
                        c[0].push(1);
                        c[1].extend_from_slice(&r[..21]);
                        put_var(&mut c[2], h.wrapping_sub(first) as u64);
                    }
                }
                put_var(&mut c[3], be_u32(r, 25) as u64);
                put_var(&mut c[4], be_u64(r, 29));
                put_var(&mut c[5], be_u64(r, 37));
                prev = Some(r);
            }
            c
        }
        Kind::Spends => {
            // created height (step from the row above) | position (step when the height is
            // the same) | vout (step when the transaction is the same) | the coin's age in
            // blocks (spender height less created height) | spender position | vin
            let mut c: Vec<Vec<u8>> = (0..6).map(|_| Vec::with_capacity(n * 2)).collect();
            let (mut ph, mut pp, mut pv) = (0u32, 0u32, 0u32);
            for (i, r) in rows.chunks_exact(24).enumerate() {
                let (h, p, v) = (be_u32(r, 0), be_u32(r, 4), be_u32(r, 8));
                let same_h = i > 0 && h == ph;
                let same_tx = same_h && p == pp;
                put_var(&mut c[0], h.wrapping_sub(ph) as u64);
                let dp = if same_h { p.wrapping_sub(pp) } else { p };
                let dv = if same_tx { v.wrapping_sub(pv) } else { v };
                put_var(&mut c[1], dp as u64);
                put_var(&mut c[2], dv as u64);
                put_var(&mut c[3], be_u32(r, 12).wrapping_sub(h) as u64);
                put_var(&mut c[4], be_u32(r, 16) as u64);
                put_var(&mut c[5], be_u32(r, 20) as u64);
                (ph, pp, pv) = (h, p, v);
            }
            c
        }
        Kind::Txs => {
            // txid prefix (step from the row above) | height above the file's first | position
            let (mut d, mut hs, mut ps) = (Vec::with_capacity(n), Vec::with_capacity(n), Vec::with_capacity(n));
            let mut prev = 0u64;
            for r in rows.chunks_exact(16) {
                let x = be_u64(r, 0);
                d.push(x.wrapping_sub(prev));
                prev = x;
                hs.push(be_u32(r, 8).wrapping_sub(first) as u64);
                ps.push(be_u32(r, 12) as u64);
            }
            let mut c = Vec::with_capacity(16);
            planes(&mut c, &d, 8);
            planes(&mut c, &hs, 4);
            planes(&mut c, &ps, 4);
            c
        }
    }
}

fn decode(kind: Kind, first: u32, n: usize, c: &[Vec<u8>]) -> Option<Vec<u8>> {
    if c.len() != kind.columns() {
        return None;
    }
    let mut out = Vec::with_capacity(n * kind.row_len());
    match kind {
        Kind::Hist => {
            if c[0].len() != n {
                return None;
            }
            let mut at = [0usize; 6];
            let (mut sid, mut ph) = ([0u8; 21], 0u32);
            for i in 0..n {
                let dh = get_var(&c[2], &mut at[2])? as u32;
                let h = match c[0][i] {
                    0 if i > 0 => ph.wrapping_add(dh),
                    1 => {
                        sid.copy_from_slice(c[1].get(at[1]..at[1] + 21)?);
                        at[1] += 21;
                        first.wrapping_add(dh)
                    }
                    _ => return None,
                };
                let p = u32::try_from(get_var(&c[3], &mut at[3])?).ok()?;
                let (recv, spent) = (get_var(&c[4], &mut at[4])?, get_var(&c[5], &mut at[5])?);
                out.extend_from_slice(&hist_row(&sid, h, p, recv, spent));
                ph = h;
            }
            if (1..6).any(|j| at[j] != c[j].len()) {
                return None;
            }
        }
        Kind::Spends => {
            let mut at = [0usize; 6];
            let (mut ph, mut pp, mut pv) = (0u32, 0u32, 0u32);
            for i in 0..n {
                let mut col = |j: usize| -> Option<u32> { u32::try_from(get_var(&c[j], &mut at[j])?).ok() };
                let h = ph.wrapping_add(col(0)?);
                let same_h = i > 0 && h == ph;
                let p = if same_h { pp.wrapping_add(col(1)?) } else { col(1)? };
                let same_tx = same_h && p == pp;
                let v = if same_tx { pv.wrapping_add(col(2)?) } else { col(2)? };
                let sh = h.wrapping_add(col(3)?);
                let (sp, vin) = (col(4)?, col(5)?);
                out.extend_from_slice(&spend_row(h, p, v, sh, sp, vin));
                (ph, pp, pv) = (h, p, v);
            }
            if (0..6).any(|j| at[j] != c[j].len()) {
                return None;
            }
        }
        Kind::Txs => {
            let d = unplanes(&c[..8], n, 8)?;
            let hs = unplanes(&c[8..12], n, 4)?;
            let ps = unplanes(&c[12..16], n, 4)?;
            let mut prev = 0u64;
            for ((d, h), p) in d.iter().zip(&hs).zip(&ps) {
                prev = prev.wrapping_add(*d);
                out.extend_from_slice(&tx_row(&prev.to_be_bytes(), first.wrapping_add(*h as u32), *p as u32));
            }
        }
    }
    Some(out)
}

/// A block as it is stored: per column, raw length u32 | stored length u32 | zstd frame.
/// A column of nothing but zero bytes is stored as no bytes at all.
fn pack(kind: Kind, first: u32, rows: &[u8], level: i32) -> Result<(Vec<u8>, [u8; 32])> {
    let mut out = Vec::with_capacity(rows.len() / 3);
    for col in encode(kind, first, rows) {
        out.extend_from_slice(&(col.len() as u32).to_be_bytes());
        if col.iter().all(|b| *b == 0) {
            out.extend_from_slice(&0u32.to_be_bytes());
            continue;
        }
        let z = zstd::bulk::compress(&col, level).map_err(|e| StoreError::Other(format!("zstd: {e}")))?;
        out.extend_from_slice(&(z.len() as u32).to_be_bytes());
        out.extend_from_slice(&z);
    }
    Ok((out, sha256(rows)))
}

/// The rows of a stored block. Each column is decompressed under a hard size cap (its
/// stated length, itself capped by the row count), so a damaged or hostile block cannot
/// ask for unbounded memory.
fn unpack(kind: Kind, first: u32, n: usize, data: &[u8]) -> Option<Vec<u8>> {
    let mut cols = Vec::with_capacity(kind.columns());
    let mut at = 0usize;
    for _ in 0..kind.columns() {
        let raw = be_u32(data.get(at..at + 8)?, 0) as usize;
        let len = be_u32(data, at + 4) as usize;
        at += 8;
        if raw > n * 32 {
            return None;
        }
        if len == 0 {
            cols.push(vec![0u8; raw]);
            continue;
        }
        let col = zstd::bulk::decompress(data.get(at..at + len)?, raw).ok()?;
        at += len;
        if col.len() != raw {
            return None;
        }
        cols.push(col);
    }
    if at != data.len() {
        return None;
    }
    decode(kind, first, n, &cols)
}

/// The id of a file's content: its header, row count and every block's row hash.
fn file_id(header: &[u8; HEADER_LEN], rows: u64, hashes: &[[u8; 32]]) -> [u8; 32] {
    let mut v = Vec::with_capacity(HEADER_LEN + 8 + hashes.len() * 32);
    v.extend_from_slice(header);
    v.extend_from_slice(&rows.to_be_bytes());
    for h in hashes {
        v.extend_from_slice(h);
    }
    sha256(&v)
}

// ---------------- writer ----------------

/// A finished file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub path: PathBuf,
    pub rows: u64,
    pub bytes: u64,
    pub id: [u8; 32],
}

/// Writes one file from rows handed over in key order. Full blocks are compressed on
/// several threads; the bytes written do not depend on how many.
pub struct Writer {
    meta: Meta,
    level: i32,
    threads: usize,
    path: PathBuf,
    tmp: PathBuf,
    file: File,
    offset: u64,
    /// The open block's rows.
    buf: Vec<u8>,
    /// Full blocks waiting to be compressed.
    pending: Vec<Vec<u8>>,
    keys: Vec<u8>,
    offsets: Vec<u64>,
    hashes: Vec<[u8; 32]>,
    rows: u64,
    last: Vec<u8>,
}

fn io(e: std::io::Error) -> StoreError {
    StoreError::Other(format!("index file: {e}"))
}

pub fn tmp_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".tmp");
    PathBuf::from(s)
}

fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

impl Writer {
    /// Start `dir/<name>` (as `<name>.tmp` until [`finish`](Self::finish)).
    pub fn create(dir: &Path, meta: Meta, level: i32, threads: usize) -> Result<Writer> {
        let path = dir.join(meta.file_name());
        let tmp = tmp_path(&path);
        let mut file = File::create(&tmp).map_err(io)?;
        file.write_all(&meta.header()).map_err(io)?;
        Ok(Writer {
            meta,
            level,
            threads: threads.max(1),
            path,
            tmp,
            file,
            offset: HEADER_LEN as u64,
            buf: Vec::new(),
            pending: Vec::new(),
            keys: Vec::new(),
            offsets: Vec::new(),
            hashes: Vec::new(),
            rows: 0,
            last: Vec::new(),
        })
    }

    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Add a row. Rows must come in strictly rising key order, with heights inside the span.
    pub fn push(&mut self, row: &[u8]) -> Result<()> {
        let k = self.meta.kind;
        if row.len() != k.row_len() {
            return Err(bad(&self.path, "row of the wrong size"));
        }
        let key = &row[..k.key_len()];
        if self.rows > 0 && key <= self.last.as_slice() {
            return Err(bad(&self.path, "rows out of order (or a key twice)"));
        }
        let h = k.height_of(row);
        if h < self.meta.first || h > self.meta.last {
            return Err(bad(&self.path, format!("a row of height {h} outside {}..={}", self.meta.first, self.meta.last)));
        }
        self.last.clear();
        self.last.extend_from_slice(key);
        self.buf.extend_from_slice(row);
        self.rows += 1;
        if self.buf.len() == k.block_rows() * k.row_len() {
            self.pending.push(std::mem::take(&mut self.buf));
            if self.pending.len() >= self.threads {
                self.flush_pending()?;
            }
        }
        Ok(())
    }

    fn flush_pending(&mut self) -> Result<()> {
        let blocks = std::mem::take(&mut self.pending);
        if blocks.is_empty() {
            return Ok(());
        }
        let (kind, first, level) = (self.meta.kind, self.meta.first, self.level);
        let packed: Vec<Result<(Vec<u8>, [u8; 32])>> = if blocks.len() == 1 {
            vec![pack(kind, first, &blocks[0], level)]
        } else {
            std::thread::scope(|s| {
                let hs: Vec<_> = blocks.iter().map(|b| s.spawn(move || pack(kind, first, b, level))).collect();
                hs.into_iter().map(|h| h.join().unwrap_or_else(|_| Err(StoreError::Other("index file: a compression thread failed".into())))).collect()
            })
        };
        for (rows, p) in blocks.iter().zip(packed) {
            let (data, hash) = p?;
            self.file.write_all(&data).map_err(io)?;
            self.keys.extend_from_slice(&rows[..kind.key_len()]);
            self.offsets.push(self.offset);
            self.hashes.push(hash);
            self.offset += data.len() as u64;
        }
        Ok(())
    }

    /// Everything needed to carry on after a stop: the full blocks are written and synced,
    /// and the returned bytes hold the block list and the open block's rows. See [`resume`](Self::resume).
    pub fn save_state(&mut self) -> Result<Vec<u8>> {
        self.flush_pending()?;
        self.file.sync_data().map_err(io)?;
        let mut v = Vec::with_capacity(64 + self.keys.len() + self.hashes.len() * 40 + self.buf.len());
        v.extend_from_slice(&self.offset.to_be_bytes());
        v.extend_from_slice(&self.rows.to_be_bytes());
        v.extend_from_slice(&(self.last.len() as u32).to_be_bytes());
        v.extend_from_slice(&self.last);
        v.extend_from_slice(&(self.hashes.len() as u32).to_be_bytes());
        v.extend_from_slice(&self.keys);
        for o in &self.offsets {
            v.extend_from_slice(&o.to_be_bytes());
        }
        for h in &self.hashes {
            v.extend_from_slice(h);
        }
        v.extend_from_slice(&(self.buf.len() as u32).to_be_bytes());
        v.extend_from_slice(&self.buf);
        Ok(v)
    }

    /// Carry on a file from [`save_state`](Self::save_state): anything written after that
    /// state was taken is cut off.
    pub fn resume(dir: &Path, meta: Meta, level: i32, threads: usize, state: &[u8]) -> Result<Writer> {
        let path = dir.join(meta.file_name());
        let tmp = tmp_path(&path);
        let parse = || -> Option<_> {
            let mut t = Take(state);
            let (offset, rows) = (t.u64()?, t.u64()?);
            let n = t.u32()? as usize;
            let last = t.bytes(n)?.to_vec();
            let blocks = t.u32()? as usize;
            let keys = t.bytes(blocks.checked_mul(meta.kind.key_len())?)?.to_vec();
            let offsets: Vec<u64> = t.bytes(blocks.checked_mul(8)?)?.chunks_exact(8).map(|x| be_u64(x, 0)).collect();
            let hashes: Vec<[u8; 32]> = t.bytes(blocks.checked_mul(32)?)?.chunks_exact(32).map(|x| x.try_into().unwrap()).collect();
            let n = t.u32()? as usize;
            let buf = t.bytes(n)?.to_vec();
            t.0.is_empty().then_some((offset, rows, last, keys, offsets, hashes, buf))
        };
        let (offset, rows, last, keys, offsets, hashes, buf) = parse().ok_or_else(|| bad(&path, "bad saved writer state"))?;
        let mut file = std::fs::OpenOptions::new().write(true).open(&tmp).map_err(io)?;
        if file.metadata().map_err(io)?.len() < offset {
            return Err(bad(&tmp, "shorter than its saved state"));
        }
        file.set_len(offset).map_err(io)?;
        file.seek(SeekFrom::Start(offset)).map_err(io)?;
        Ok(Writer { meta, level, threads: threads.max(1), path, tmp, file, offset, buf, pending: Vec::new(), keys, offsets, hashes, rows, last })
    }

    /// Write the last block, the footer and the trailer; sync; give the file its name.
    pub fn finish(mut self) -> Result<FileInfo> {
        if !self.buf.is_empty() {
            self.pending.push(std::mem::take(&mut self.buf));
        }
        self.flush_pending()?;
        let kl = self.meta.kind.key_len();
        let header = self.meta.header();
        let mut foot = Vec::with_capacity(self.hashes.len() * (kl + 40) + TRAILER_LEN);
        for (i, h) in self.hashes.iter().enumerate() {
            foot.extend_from_slice(&self.keys[i * kl..(i + 1) * kl]);
            foot.extend_from_slice(&self.offsets[i].to_be_bytes());
            foot.extend_from_slice(h);
        }
        let id = file_id(&header, self.rows, &self.hashes);
        foot.extend_from_slice(&self.rows.to_be_bytes());
        foot.extend_from_slice(&(self.hashes.len() as u32).to_be_bytes());
        foot.extend_from_slice(&self.offset.to_be_bytes());
        foot.extend_from_slice(&id);
        let sum = sha256(&[&header[..], &foot[..]].concat());
        foot.extend_from_slice(&sum);
        foot.extend_from_slice(END_MAGIC);
        self.file.write_all(&foot).map_err(io)?;
        self.file.sync_all().map_err(io)?;
        drop(self.file);
        std::fs::rename(&self.tmp, &self.path).map_err(io)?;
        if let Some(d) = self.path.parent() {
            sync_dir(d);
        }
        Ok(FileInfo { path: self.path, rows: self.rows, bytes: self.offset + foot.len() as u64, id })
    }
}

// ---------------- reader ----------------

#[cfg(unix)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(f, buf, off)
}
#[cfg(windows)]
fn read_at(f: &File, buf: &mut [u8], off: u64) -> std::io::Result<()> {
    let mut done = 0usize;
    while done < buf.len() {
        let n = std::os::windows::fs::FileExt::seek_read(f, &mut buf[done..], off + done as u64)?;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
    }
    Ok(())
}

/// An open file: its header and block list in memory (one entry per block: first key,
/// offset, row hash), its blocks read and checked one at a time.
pub struct Reader {
    pub meta: Meta,
    pub rows: u64,
    pub bytes: u64,
    pub id: [u8; 32],
    path: PathBuf,
    file: File,
    keys: Vec<u8>,
    /// One more than there are blocks: the last entry is where the footer starts.
    offsets: Vec<u64>,
    hashes: Vec<[u8; 32]>,
}

impl Reader {
    /// Open a file and check its header, footer and checksum (not its blocks: each is
    /// checked against its row hash when read; [`verify`](Self::verify) reads them all).
    pub fn open(path: &Path) -> Result<Reader> {
        let file = File::open(path).map_err(|e| bad(path, e))?;
        let bytes = file.metadata().map_err(|e| bad(path, e))?.len();
        if bytes < (HEADER_LEN + TRAILER_LEN) as u64 {
            return Err(bad(path, "too short"));
        }
        let mut header = [0u8; HEADER_LEN];
        read_at(&file, &mut header, 0).map_err(|e| bad(path, e))?;
        let meta = Meta::parse(&header).map_err(|e| bad(path, e))?;
        let mut t = [0u8; TRAILER_LEN];
        read_at(&file, &mut t, bytes - TRAILER_LEN as u64).map_err(|e| bad(path, e))?;
        if &t[84..92] != END_MAGIC {
            return Err(bad(path, "no end mark (cut short?)"));
        }
        let (rows, blocks, foot_off) = (be_u64(&t, 0), be_u32(&t, 8) as u64, be_u64(&t, 12));
        let kl = meta.kind.key_len();
        let entry = (kl + 40) as u64;
        if blocks != rows.div_ceil(meta.kind.block_rows() as u64) || foot_off < HEADER_LEN as u64 || foot_off.checked_add(blocks * entry + TRAILER_LEN as u64) != Some(bytes) {
            return Err(bad(path, "footer does not fit the file"));
        }
        let mut foot = vec![0u8; (blocks * entry) as usize];
        read_at(&file, &mut foot, foot_off).map_err(|e| bad(path, e))?;
        if sha256(&[&header[..], &foot[..], &t[..52]].concat())[..] != t[52..84] {
            return Err(bad(path, "checksum mismatch"));
        }
        let (mut keys, mut offsets, mut hashes) = (Vec::with_capacity(blocks as usize * kl), Vec::with_capacity(blocks as usize + 1), Vec::with_capacity(blocks as usize));
        for e in foot.chunks_exact(entry as usize) {
            keys.extend_from_slice(&e[..kl]);
            offsets.push(be_u64(e, kl));
            hashes.push(e[kl + 8..].try_into().unwrap());
        }
        offsets.push(foot_off);
        if offsets[0] < HEADER_LEN as u64 || offsets.windows(2).any(|w| w[0] >= w[1]) {
            return Err(bad(path, "bad block offsets"));
        }
        let id: [u8; 32] = t[20..52].try_into().unwrap();
        if file_id(&header, rows, &hashes) != id {
            return Err(bad(path, "file id mismatch"));
        }
        Ok(Reader { meta, rows, bytes, id, path: path.to_path_buf(), file, keys, offsets, hashes })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn blocks(&self) -> usize {
        self.hashes.len()
    }

    fn first_key(&self, i: usize) -> &[u8] {
        let kl = self.meta.kind.key_len();
        &self.keys[i * kl..(i + 1) * kl]
    }

    /// Block `i`'s rows, checked against its row hash.
    pub fn block(&self, i: usize) -> Result<Vec<u8>> {
        let k = self.meta.kind;
        let n = if i + 1 == self.blocks() { self.rows as usize - i * k.block_rows() } else { k.block_rows() };
        let mut data = vec![0u8; (self.offsets[i + 1] - self.offsets[i]) as usize];
        read_at(&self.file, &mut data, self.offsets[i]).map_err(|e| bad(&self.path, e))?;
        let rows = unpack(k, self.meta.first, n, &data).ok_or_else(|| bad(&self.path, format!("block {i} is damaged (does not decode)")))?;
        if rows.len() != n * k.row_len() || sha256(&rows) != self.hashes[i] {
            return Err(bad(&self.path, format!("block {i} is damaged (row hash mismatch)")));
        }
        Ok(rows)
    }

    /// Every row whose key starts with `prefix`, in order. Reads one block, and the next
    /// ones only while the rows run on.
    pub fn scan_prefix(&self, prefix: &[u8], mut f: impl FnMut(&[u8])) -> Result<()> {
        let k = self.meta.kind;
        let n = self.blocks();
        // The last block that starts below the prefix may hold its first rows.
        let (mut lo, mut hi) = (0usize, n);
        while lo < hi {
            let mid = (lo + hi) / 2;
            if self.first_key(mid) < prefix {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        for i in lo.saturating_sub(1)..n {
            let fk = self.first_key(i);
            if fk > prefix && !fk.starts_with(prefix) {
                break;
            }
            for row in self.block(i)?.chunks_exact(k.row_len()) {
                let key = &row[..k.key_len()];
                if key.starts_with(prefix) {
                    f(row);
                } else if key > prefix {
                    return Ok(());
                }
            }
        }
        Ok(())
    }

    /// The row with exactly this key.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut out = None;
        self.scan_prefix(key, |r| out = Some(r.to_vec()))?;
        Ok(out)
    }

    /// Every row in order.
    pub fn for_each_row(&self, mut f: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        for i in 0..self.blocks() {
            for row in self.block(i)?.chunks_exact(self.meta.kind.row_len()) {
                f(row)?;
            }
        }
        Ok(())
    }

    /// Read the whole file: every block decodes to its row hash, begins with its listed
    /// first key, rows rise strictly and lie inside the span.
    pub fn verify(&self) -> Result<()> {
        let k = self.meta.kind;
        let mut last: Vec<u8> = Vec::new();
        let mut seen = 0u64;
        for i in 0..self.blocks() {
            let rows = self.block(i)?;
            if rows[..k.key_len()] != *self.first_key(i) {
                return Err(bad(&self.path, format!("block {i} does not start with its listed key")));
            }
            for row in rows.chunks_exact(k.row_len()) {
                let key = &row[..k.key_len()];
                let h = k.height_of(row);
                if (seen > 0 && key <= last.as_slice()) || h < self.meta.first || h > self.meta.last {
                    return Err(bad(&self.path, format!("block {i} holds a row out of order or out of its span")));
                }
                last.clear();
                last.extend_from_slice(key);
                seen += 1;
            }
        }
        if seen != self.rows {
            return Err(bad(&self.path, "row count mismatch"));
        }
        Ok(())
    }
}

/// Walks a file's rows in order, a block in memory at a time (for merging).
pub struct Cursor<'a> {
    reader: &'a Reader,
    block: usize,
    rows: Vec<u8>,
    at: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(reader: &'a Reader) -> Result<Cursor<'a>> {
        let mut c = Cursor { reader, block: 0, rows: Vec::new(), at: 0 };
        if reader.blocks() > 0 {
            c.rows = reader.block(0)?;
        }
        Ok(c)
    }

    /// The row the cursor is on (None at the end).
    pub fn row(&self) -> Option<&[u8]> {
        let n = self.reader.meta.kind.row_len();
        self.rows.get(self.at..self.at + n)
    }

    pub fn advance(&mut self) -> Result<()> {
        self.at += self.reader.meta.kind.row_len();
        if self.at >= self.rows.len() && self.block + 1 < self.reader.blocks() {
            self.block += 1;
            self.rows = self.reader.block(self.block)?;
            self.at = 0;
        }
        Ok(())
    }
}

/// Merge files of one kind (their spans next to each other, in order) into one file over
/// the whole span: ranges into an era. The result is the same bytes as writing the era's
/// rows directly.
pub fn merge(dir: &Path, sources: &[Reader], level: i32, threads: usize) -> Result<FileInfo> {
    let (Some(a), Some(z)) = (sources.first(), sources.last()) else { return Err(StoreError::Other("index files: nothing to merge".into())) };
    for w in sources.windows(2) {
        if w[0].meta.kind != w[1].meta.kind || w[0].meta.chain != w[1].meta.chain || w[0].meta.last.checked_add(1) != Some(w[1].meta.first) {
            return Err(bad(w[1].path(), "does not follow the file before it"));
        }
    }
    let meta = Meta { kind: a.meta.kind, chain: a.meta.chain, first: a.meta.first, last: z.meta.last, last_hash: z.meta.last_hash };
    let kl = meta.kind.key_len();
    let mut out = Writer::create(dir, meta, level, threads)?;
    let mut cur = sources.iter().map(Cursor::new).collect::<Result<Vec<_>>>()?;
    loop {
        let mut best: Option<usize> = None;
        for (i, c) in cur.iter().enumerate() {
            if let Some(r) = c.row() {
                if best.is_none_or(|b| r[..kl] < cur[b].row().unwrap()[..kl]) {
                    best = Some(i);
                }
            }
        }
        let Some(b) = best else { break };
        out.push(cur[b].row().unwrap())?;
        cur[b].advance()?;
    }
    out.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rnd(seed: &mut u64) -> u64 {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        *seed
    }

    /// Sorted, unique rows of a kind with heights in first..=last.
    pub(crate) fn sample(kind: Kind, n: usize, first: u32, last: u32, seed: u64) -> Vec<Vec<u8>> {
        let mut s = seed | 1;
        let span = (last - first + 1) as u64;
        let mut rows: Vec<Vec<u8>> = (0..n)
            .map(|_| match kind {
                Kind::Hist => {
                    let mut sid = [0u8; 21];
                    // Few scripts, so most have several rows.
                    let who = rnd(&mut s) % (n as u64 / 6 + 1);
                    sid[0] = (who % 3) as u8;
                    sid[1..].copy_from_slice(&sha256(&who.to_be_bytes())[..20]);
                    let (r, sp) = if rnd(&mut s).is_multiple_of(2) { (rnd(&mut s) % 5_000_000_000, 0) } else { (0, rnd(&mut s)) };
                    hist_row(&sid, first + (rnd(&mut s) % span) as u32, (rnd(&mut s) % 3000) as u32, r, sp).to_vec()
                }
                Kind::Spends => {
                    let sh = first + (rnd(&mut s) % span) as u32;
                    let age: u64 = if rnd(&mut s).is_multiple_of(4) { rnd(&mut s) % (sh as u64 + 1) } else { rnd(&mut s) % 8 };
                    spend_row(sh - (age as u32).min(sh), (rnd(&mut s) % 2500) as u32, (rnd(&mut s) % 4) as u32, sh, (rnd(&mut s) % 2500) as u32, (rnd(&mut s) % 3) as u32).to_vec()
                }
                Kind::Txs => tx_row(&rnd(&mut s).to_be_bytes(), first + (rnd(&mut s) % span) as u32, (rnd(&mut s) % 3000) as u32).to_vec(),
            })
            .collect();
        rows.sort();
        rows.dedup_by(|a, b| a[..kind.key_len()] == b[..kind.key_len()]);
        rows
    }

    fn meta(kind: Kind, first: u32, last: u32) -> Meta {
        Meta { kind, chain: 7, first, last, last_hash: [9; 32] }
    }

    fn write(dir: &Path, m: Meta, rows: &[Vec<u8>], level: i32, threads: usize) -> FileInfo {
        let mut w = Writer::create(dir, m, level, threads).unwrap();
        for r in rows {
            w.push(r).unwrap();
        }
        w.finish().unwrap()
    }

    #[test]
    fn names() {
        assert_eq!(file_name(Kind::Spends, 100_000, 199_999), "P-00100000-00199999.ixf");
        assert_eq!(parse_name("P-00100000-00199999.ixf"), Some((Kind::Spends, 100_000, 199_999)));
        assert_eq!(parse_name("P-00100000-00199999.ixf.tmp"), None);
        assert_eq!(parse_name("X-00100000-00199999.ixf"), None);
    }

    #[test]
    fn round_trip_every_kind() {
        let dir = tempfile::tempdir().unwrap();
        for kind in Kind::ALL {
            // Empty, one row, exactly one block, several blocks with a short last one.
            for n in [0usize, 1, kind.block_rows(), kind.block_rows() * 3 + 17] {
                let rows = if n == 0 { Vec::new() } else { sample(kind, n * 11 / 10 + 8, 20_000, 29_999, n as u64 + 3)[..n].to_vec() };
                assert_eq!(rows.len(), n);
                let info = write(dir.path(), meta(kind, 20_000, 29_999), &rows, 3, 4);
                let r = Reader::open(&info.path).unwrap();
                assert_eq!((r.rows, r.id, r.bytes), (n as u64, info.id, std::fs::metadata(&info.path).unwrap().len()));
                assert_eq!(r.meta, meta(kind, 20_000, 29_999));
                r.verify().unwrap();
                let mut back = Vec::new();
                r.for_each_row(|x| {
                    back.push(x.to_vec());
                    Ok(())
                })
                .unwrap();
                assert_eq!(back, rows, "{kind:?} {n}");
            }
        }
    }

    #[test]
    fn extreme_values_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let m = |k| Meta { kind: k, chain: 1, first: 0, last: u32::MAX, last_hash: [0; 32] };
        let h = vec![
            hist_row(&[0; 21], 0, 0, 0, 0).to_vec(),
            hist_row(&[0; 21], 0, u32::MAX, u64::MAX, u64::MAX).to_vec(),
            hist_row(&[0; 21], u32::MAX, 0, 1, 0).to_vec(),
            hist_row(&[255; 21], 0, 0, 0, u64::MAX).to_vec(),
            hist_row(&[255; 21], u32::MAX, u32::MAX, u64::MAX, 0).to_vec(),
        ];
        let p = vec![
            spend_row(0, 0, 0, 0, 0, 0).to_vec(),
            spend_row(0, 0, u32::MAX, u32::MAX, u32::MAX, u32::MAX).to_vec(),
            spend_row(0, u32::MAX, 0, 5, 0, 0).to_vec(),
            spend_row(u32::MAX, 0, 0, u32::MAX, 1, 1).to_vec(),
            spend_row(u32::MAX, u32::MAX, u32::MAX, u32::MAX, 0, 0).to_vec(),
        ];
        let t = vec![tx_row(&[0; 8], 0, 0).to_vec(), tx_row(&[0; 8], u32::MAX, u32::MAX).to_vec(), tx_row(&[255; 8], 0, 7).to_vec(), tx_row(&[255; 8], u32::MAX, u32::MAX).to_vec()];
        for (kind, rows) in [(Kind::Hist, h), (Kind::Spends, p), (Kind::Txs, t)] {
            let info = write(dir.path(), m(kind), &rows, 3, 1);
            let r = Reader::open(&info.path).unwrap();
            r.verify().unwrap();
            assert_eq!(r.block(0).unwrap(), rows.concat());
        }
    }

    #[test]
    fn lookups() {
        let dir = tempfile::tempdir().unwrap();
        for kind in Kind::ALL {
            let rows = sample(kind, kind.block_rows() * 4, 0, 9_999, 77);
            let info = write(dir.path(), meta(kind, 0, 9_999), &rows, 3, 3);
            let r = Reader::open(&info.path).unwrap();
            let kl = kind.key_len();
            // Every exact key, at block edges and in between.
            for i in (0..rows.len()).step_by(97).chain([0, kind.block_rows() - 1, kind.block_rows(), rows.len() - 1]) {
                assert_eq!(r.get(&rows[i][..kl]).unwrap().as_ref(), Some(&rows[i]));
            }
            // Prefix scans equal a filter over the rows (prefixes that cross blocks too).
            let plen = match kind {
                Kind::Hist => 21,
                Kind::Spends => 4,
                Kind::Txs => 1,
            };
            for i in (0..rows.len()).step_by(rows.len() / 40) {
                let pre = &rows[i][..plen];
                let want: Vec<&Vec<u8>> = rows.iter().filter(|x| x.starts_with(pre)).collect();
                let mut got = Vec::new();
                r.scan_prefix(pre, |x| got.push(x.to_vec())).unwrap();
                assert_eq!(got.iter().collect::<Vec<_>>(), want);
            }
            // Keys that are not there: below everything, above everything, in a gap.
            let mut none = 0;
            r.scan_prefix(&[0u8; 1][..0], |_| none += 1).unwrap();
            assert_eq!(none, rows.len(), "the empty prefix is every row");
            let mut miss = rows[rows.len() / 2][..kl].to_vec();
            *miss.last_mut().unwrap() ^= 0x55;
            if !rows.iter().any(|x| x[..kl] == miss[..]) {
                assert!(r.get(&miss).unwrap().is_none());
            }
            assert!(r.get(&vec![0xffu8; kl]).unwrap().is_none());
        }
    }

    #[test]
    fn same_rows_same_bytes_whatever_the_threads() {
        let dir = tempfile::tempdir().unwrap();
        for kind in Kind::ALL {
            let rows = sample(kind, kind.block_rows() * 5 + 100, 0, 9_999, 5);
            let (a, b, c) = (dir.path().join("a"), dir.path().join("b"), dir.path().join("c"));
            for d in [&a, &b, &c] {
                std::fs::create_dir_all(d).unwrap();
            }
            let ia = write(&a, meta(kind, 0, 9_999), &rows, LEVEL, 1);
            let ib = write(&b, meta(kind, 0, 9_999), &rows, LEVEL, 8);
            assert_eq!(std::fs::read(&ia.path).unwrap(), std::fs::read(&ib.path).unwrap(), "two builders, one file");
            // Another compression level: other bytes, the same id (it is over the rows).
            let ic = write(&c, meta(kind, 0, 9_999), &rows, 1, 3);
            assert_eq!(ia.id, ic.id);
            assert_ne!(std::fs::read(&ia.path).unwrap(), std::fs::read(&ic.path).unwrap());
            // Another span or chain is another id.
            let other = write(&c, Meta { chain: 8, ..meta(kind, 0, 9_999) }, &rows, 1, 3);
            assert_ne!(other.id, ia.id);
        }
    }

    #[test]
    fn damage_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        for kind in Kind::ALL {
            let rows = sample(kind, kind.block_rows() * 2 + 50, 0, 9_999, 11);
            let info = write(dir.path(), meta(kind, 0, 9_999), &rows, 3, 2);
            let good = std::fs::read(&info.path).unwrap();
            Reader::open(&info.path).unwrap().verify().unwrap();
            let entry = kind.key_len() + 40;
            let foot = good.len() - TRAILER_LEN - 3 * entry;
            // One flipped bit anywhere: in the header, a block, the footer, the trailer.
            let mut s = 99u64;
            let mut spots = vec![3usize, 12, 30, 40, HEADER_LEN + 9, foot + 2, foot + entry + kind.key_len() + 3, foot + entry * 2 + kind.key_len() + 20, good.len() - 90, good.len() - 60, good.len() - 30, good.len() - 2];
            spots.extend((0..60).map(|_| (rnd(&mut s) % good.len() as u64) as usize));
            for at in spots {
                let mut v = good.clone();
                v[at] ^= 1 << (at % 8);
                std::fs::write(&info.path, &v).unwrap();
                // Caught when the file is opened or read through. (Inside a block, a flipped
                // bit zstd does not use leaves the rows as they were: that is no damage.)
                let caught = match Reader::open(&info.path) {
                    Err(_) => true,
                    Ok(r) => r.verify().is_err() || (at >= HEADER_LEN && at < foot && (0..r.blocks()).map(|i| r.block(i).unwrap()).collect::<Vec<_>>().concat() == rows.concat()),
                };
                assert!(caught, "{kind:?}: a flipped bit at {at} of {} went unseen", good.len());
            }
            // Cut short, or with bytes added.
            std::fs::write(&info.path, &good[..good.len() - 1]).unwrap();
            assert!(Reader::open(&info.path).is_err());
            std::fs::write(&info.path, &good[..good.len() / 2]).unwrap();
            assert!(Reader::open(&info.path).is_err());
            std::fs::write(&info.path, [&good[..], &[0u8; 4]].concat()).unwrap();
            assert!(Reader::open(&info.path).is_err());
            // A damaged block fails the lookup that reads it, not only verify.
            let mut v = good.clone();
            for b in &mut v[HEADER_LEN + 30..HEADER_LEN + 60] {
                *b ^= 0xff;
            }
            std::fs::write(&info.path, &v).unwrap();
            let r = Reader::open(&info.path).unwrap();
            assert!(r.get(&rows[1][..kind.key_len()]).is_err());
            assert_eq!(r.get(&rows[rows.len() - 1][..kind.key_len()]).unwrap().as_ref(), Some(&rows[rows.len() - 1]), "other blocks still read");
        }
    }

    #[test]
    fn writer_refuses_bad_rows() {
        let dir = tempfile::tempdir().unwrap();
        let rows = sample(Kind::Hist, 100, 10_000, 19_999, 1);
        let mut w = Writer::create(dir.path(), meta(Kind::Hist, 10_000, 19_999), 3, 1).unwrap();
        w.push(&rows[5]).unwrap();
        assert!(w.push(&rows[5]).is_err(), "the same key twice");
        assert!(w.push(&rows[4]).is_err(), "out of order");
        assert!(w.push(&rows[6][..40]).is_err(), "wrong size");
        assert!(w.push(&hist_row(&[255; 21], 9_999, 0, 1, 0)).is_err(), "below the span");
        assert!(w.push(&hist_row(&[255; 21], 20_000, 0, 1, 0)).is_err(), "above the span");
        w.push(&rows[6]).unwrap();
        // Nothing has the final name until finish.
        assert!(!dir.path().join(meta(Kind::Hist, 10_000, 19_999).file_name()).exists());
        assert_eq!(w.finish().unwrap().rows, 2);
    }

    #[test]
    fn stopped_and_resumed_writer_gives_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        for kind in Kind::ALL {
            let rows = sample(kind, kind.block_rows() * 4 + 321, 0, 9_999, 21);
            let m = meta(kind, 0, 9_999);
            let whole = write(&a, m, &rows, 3, 2);
            // Stop at several points (inside a block, at a block edge), each time with rows
            // written after the saved state that the resume must cut off.
            let mut w = Writer::create(&b, m, 3, 2).unwrap();
            let mut done = 0usize;
            for stop in [10, kind.block_rows(), kind.block_rows() * 2 + 5, kind.block_rows() * 4 + 1] {
                for r in &rows[done..stop] {
                    w.push(r).unwrap();
                }
                done = stop;
                let state = w.save_state().unwrap();
                for r in &rows[done..(done + kind.block_rows() * 2).min(rows.len())] {
                    w.push(r).unwrap();
                }
                let _ = w.save_state().unwrap(); // those rows reach the file...
                drop(w);
                w = Writer::resume(&b, m, 3, 2, &state).unwrap(); // ...and are cut off again
                assert_eq!(w.rows(), done as u64);
            }
            for r in &rows[done..] {
                w.push(r).unwrap();
            }
            let resumed = w.finish().unwrap();
            assert_eq!(resumed.id, whole.id);
            assert_eq!(std::fs::read(&resumed.path).unwrap(), std::fs::read(&whole.path).unwrap());
            assert!(Writer::resume(&b, m, 3, 2, &[1, 2, 3]).is_err());
        }
    }

    #[test]
    fn ranges_merge_into_the_era_written_directly() {
        let dir = tempfile::tempdir().unwrap();
        let (parts, whole) = (dir.path().join("parts"), dir.path().join("whole"));
        std::fs::create_dir_all(&parts).unwrap();
        std::fs::create_dir_all(&whole).unwrap();
        for kind in Kind::ALL {
            let rows = sample(kind, kind.block_rows() * 6, 0, 999, 31);
            let m = |first, last| Meta { kind, chain: 7, first, last, last_hash: [(last % 251) as u8; 32] };
            let direct = write(&whole, m(0, 999), &rows, 3, 2);
            let mut readers = Vec::new();
            for i in 0..10u32 {
                let (first, last) = (i * 100, i * 100 + 99);
                let part: Vec<Vec<u8>> = rows.iter().filter(|r| (first..=last).contains(&kind.height_of(r))).cloned().collect();
                readers.push(Reader::open(&write(&parts, m(first, last), &part, 3, 2).path).unwrap());
            }
            let merged = merge(&parts, &readers, 3, 4).unwrap();
            assert_eq!(merged.rows, rows.len() as u64);
            assert_eq!(merged.id, direct.id);
            assert_eq!(std::fs::read(&merged.path).unwrap(), std::fs::read(&direct.path).unwrap());
            // Files that do not follow each other are refused.
            assert!(merge(&parts, &readers[1..].iter().rev().map(|r| Reader::open(r.path()).unwrap()).collect::<Vec<_>>(), 3, 1).is_err());
        }
    }
}
