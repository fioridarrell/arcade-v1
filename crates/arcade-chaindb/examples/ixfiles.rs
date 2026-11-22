//! Index files by hand (docs/INDEXFILES.md), with the arcade stopped:
//!
//!   ixfiles convert <shared chain.mdbx> <chain key>   settled rows -> files, readers switched
//!                                                     (Ctrl-C: stop at the next checkpoint
//!                                                     and carry on next run; twice: quit;
//!                                                     held while the drive has less than
//!                                                     ARCADE_IX_FREE_GB, default 50, plus
//!                                                     the pass's estimated size)
//!   ixfiles info    <shared chain.mdbx> <chain key>   what the readers use now
//!   ixfiles off     <shared chain.mdbx> <chain key>   back to the tables alone
//!   ixfiles check   <shared chain.mdbx> <chain key> [every]   the files against the tables:
//!                                                     every n-th address's history, tx and
//!                                                     spend (default 50), mapped to heights
//!                                                     on their own (HEIGHT), not by ixconv
//!   ixfiles verify  <file.ixf>...                     read files whole and check them (and
//!                                                     mark them checked: <file>.ok)
//!   ixfiles bench   <H|S|T> <out dir> <run file>...   address run files -> one index file (sizes)
use arcade_chaindb::ixconv::Options;
use arcade_chaindb::ixfile::{hist_row, spend_row, tx_row, Kind, Meta, Reader, Writer, LEVEL};
use arcade_chaindb::ChainDb;
use std::collections::{BTreeMap, HashMap};
use std::io::Read;

/// Set by Ctrl-C: the conversion saves a checkpoint and stops.
static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_interrupt(_: libc::c_int) {
    // A second Ctrl-C quits at once (only async-signal-safe calls here).
    if STOP.swap(true, std::sync::atomic::Ordering::SeqCst) {
        unsafe { libc::_exit(130) };
    }
}

fn catch_interrupt() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGINT, on_interrupt as *const () as libc::sighandler_t);
    }
}

/// Free bytes on the drive of `p` (statvfs).
#[cfg(unix)]
fn free_bytes(p: &std::path::Path) -> Option<u64> {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
        return None;
    }
    Some(s.f_bavail as u64 * s.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_bytes(_: &std::path::Path) -> Option<u64> {
    None
}

fn be32(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}
fn be64(b: &[u8], at: usize) -> u64 {
    u64::from_be_bytes(b[at..at + 8].try_into().unwrap())
}

/// The rows (key | value) of an address run file (addrbuild): the fixed format ("ARUN"
/// version 2), or the older one (each row as length u32 | key length u16 | key | value).
fn run_rows(path: &str, want: u8) -> (usize, Vec<u8>) {
    let mut v = Vec::new();
    std::fs::File::open(path).unwrap().read_to_end(&mut v).unwrap();
    if v.len() >= 28 && &v[..4] == b"ARUN" {
        assert!(v[4] == 2 && v[5] == want, "{path}: another version or kind of run");
        let len = u16::from_be_bytes([v[6], v[7]]) as usize + u16::from_be_bytes([v[8], v[9]]) as usize;
        let rows = be64(&v, v.len() - 16) as usize;
        assert_eq!(v.len(), 12 + rows * len + 16, "{path}: size and row count disagree");
        v.truncate(v.len() - 16);
        v.drain(..12);
        return (len, v);
    }
    let len = be32(&v, 0) as usize - 2;
    let mut out = Vec::with_capacity(v.len() / (len + 6) * len);
    for r in v.chunks(len + 6) {
        assert!(r.len() == len + 6 && be32(r, 0) as usize == len + 2, "{path}: rows of several sizes");
        out.extend_from_slice(&r[6..]);
    }
    (len, out)
}

fn bench(a: &[String]) {
    let (tag, out) = (a[0].as_bytes()[0], std::path::Path::new(&a[1]));
    std::fs::create_dir_all(out).unwrap();
    let (mut rows, mut raw_rows, mut raw_bytes): (Vec<Vec<u8>>, u64, u64) = (Vec::new(), 0, 0);
    let kind = match tag {
        b'H' => {
            // Run rows: C | script id | height | txid prefix -> received | spent, one per
            // output or input. A file row is one per (script, transaction): sums. The run
            // has no tx position; the rank of the txid prefix among its block's (seen here)
            // stands in for it: the same range and spread as real positions.
            let mut sum: BTreeMap<(u32, [u8; 12], [u8; 21]), (u64, u64)> = BTreeMap::new();
            for path in &a[2..] {
                let (len, v) = run_rows(path, b'H');
                assert_eq!(len, 55);
                for r in v.chunks_exact(len) {
                    let e = sum.entry((be32(r, 23), r[27..39].try_into().unwrap(), r[2..23].try_into().unwrap())).or_default();
                    e.0 += be64(r, 39);
                    e.1 += be64(r, 47);
                    raw_rows += 1;
                    raw_bytes += len as u64;
                }
            }
            let (mut at, mut pos) = ((u32::MAX, [0u8; 12]), 0u32);
            for ((h, t12, sid), (recv, spent)) in &sum {
                if (*h, *t12) != at {
                    pos = if *h == at.0 { pos + 1 } else { 0 };
                    at = (*h, *t12);
                }
                rows.push(hist_row(sid, *h, pos, *recv, *spent).to_vec());
            }
            Kind::Hist
        }
        b'S' => {
            // C | block | position | vout -> spender block | position | vin (block numbers
            // stand in for heights: on this chain they run alike).
            for path in &a[2..] {
                let (len, v) = run_rows(path, b'S');
                assert_eq!(len, 26);
                for r in v.chunks_exact(len) {
                    rows.push(spend_row(be32(r, 2), be32(r, 6), be32(r, 10), be32(r, 14), be32(r, 18), be32(r, 22)).to_vec());
                    raw_rows += 1;
                    raw_bytes += len as u64;
                }
            }
            Kind::Spends
        }
        b'T' => {
            for path in &a[2..] {
                let (len, v) = run_rows(path, b'T');
                assert_eq!(len, 22);
                for r in v.chunks_exact(len) {
                    rows.push(tx_row(r[2..10].try_into().unwrap(), be32(r, 14), be32(r, 18)).to_vec());
                    raw_rows += 1;
                    raw_bytes += len as u64;
                }
            }
            Kind::Txs
        }
        _ => panic!("kind: H, S or T"),
    };
    rows.sort_unstable();
    rows.dedup_by(|x, y| x[..kind.key_len()] == y[..kind.key_len()]);
    let (first, last) = rows.iter().fold((u32::MAX, 0), |(lo, hi), r| (lo.min(kind.height_of(r)), hi.max(kind.height_of(r))));
    let t = std::time::Instant::now();
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).min(8);
    let mut w = Writer::create(out, Meta { kind, chain: 0, first, last, last_hash: [0; 32], range: last - first + 1, era_ranges: 1 }, LEVEL, threads).unwrap();
    for r in &rows {
        w.push(r).unwrap();
    }
    let info = w.finish().unwrap();
    let wrote = t.elapsed();
    let t = std::time::Instant::now();
    Reader::open(&info.path).unwrap().verify().unwrap();
    println!(
        "{} files={} heights={first}..={last} run_rows={raw_rows} run_bytes={raw_bytes} file_rows={} file_bytes={} bytes_per_run_row={:.2} bytes_per_file_row={:.2} write={wrote:.1?} verify={:.1?}",
        tag as char,
        a.len() - 2,
        info.rows,
        info.bytes,
        info.bytes as f64 / raw_rows as f64,
        info.bytes as f64 / info.rows.max(1) as f64,
        t.elapsed()
    );
}

/// The files against the tables, every `every`-th row: history (per address: heights and
/// amounts), tx index (each txid's height and position among the T files' candidates), spends
/// (the spender found from the coin's height). Block numbers become heights through HEIGHT here.
fn check(db: &ChainDb, p: &'static arcade_chain::params::ChainParams, every: usize) {
    use arcade_chaindb::ixset::KindSet;
    use arcade_chaindb::schema::{k_chain, HEIGHT, SCRIPT_HIST, SPENDS, TXS};
    let c = p.chain_no;
    let store = db.store(c);
    let pre = k_chain(c);
    let scan = |table: &str, f: &mut dyn FnMut(&[u8], &[u8])| {
        let mut after: Option<Vec<u8>> = None;
        loop {
            let rows = store.read(|r| r.scan_after(table, &pre, after.as_deref(), 200_000)).unwrap();
            let Some(last) = rows.last() else { break };
            for (k, v) in &rows {
                f(k, v);
            }
            after = Some(last.0.clone());
        }
    };
    // Block number -> height on the main chain.
    let mut h_of_b: HashMap<u32, u32> = HashMap::new();
    scan(HEIGHT, &mut |k, v| {
        h_of_b.insert(be32(v, 32), be32(k, 2));
    });
    let hash_at = |h: u32| -> arcade_store::Result<Option<[u8; 32]>> { Ok(db.hash_at(c, h)?.map(|x| x.0)) };
    let layout = arcade_chaindb::ixconv::Layout::for_chain(p);
    let load = |k: Kind| db.index_files_upto(c, k).unwrap().map(|upto| (upto, KindSet::load(&db.index_dir(c), k, c, upto, Some((layout.range, layout.era_ranges)), hash_at).unwrap()));
    let (mut bad, mut seen) = (0u64, [0u64; 3]);
    if let Some((upto, set)) = load(Kind::Hist) {
        // Table key: C | script id (21) | height | txid prefix (12) -> received | spent.
        let mut cur: Option<[u8; 21]> = None;
        let mut rows: Vec<(u32, u64, u64)> = Vec::new();
        let mut n = 0usize;
        let mut done = |sid: [u8; 21], rows: &mut Vec<(u32, u64, u64)>| {
            let mut want: Vec<(u32, u64, u64)> = rows.drain(..).filter(|r| r.0 <= upto).collect();
            let mut got: Vec<(u32, u64, u64)> = set.history(&sid).unwrap().into_iter().map(|(h, _, r, s)| (h, r, s)).collect();
            want.sort_unstable();
            got.sort_unstable();
            seen[0] += 1;
            if want != got {
                bad += 1;
                println!("H differs for script id {}: table {} rows, files {}", hex::encode(sid), want.len(), got.len());
            }
        };
        scan(SCRIPT_HIST, &mut |k, v| {
            let sid: [u8; 21] = k[2..23].try_into().unwrap();
            if cur != Some(sid) {
                if let Some(prev) = cur.take() {
                    if n.is_multiple_of(every) {
                        done(prev, &mut rows);
                    }
                    n += 1;
                }
                rows.clear();
                cur = Some(sid);
            }
            rows.push((be32(k, 23), be64(v, 0), be64(v, 8)));
        });
        if let Some(prev) = cur {
            done(prev, &mut rows);
        }
    }
    if let Some((upto, set)) = load(Kind::Txs) {
        let mut n = 0usize;
        scan(TXS, &mut |k, v| {
            n += 1;
            let Some(&h) = h_of_b.get(&be32(v, 0)) else { return };
            if h > upto || !n.is_multiple_of(every) {
                return;
            }
            seen[1] += 1;
            if !set.tx_candidates(&k[2..10]).unwrap().contains(&(h, be32(v, 4))) {
                bad += 1;
                println!("T misses txid prefix {} at {h}/{}", hex::encode(&k[2..14]), be32(v, 4));
            }
        });
    }
    if let Some((upto, set)) = load(Kind::Spends) {
        let mut n = 0usize;
        scan(SPENDS, &mut |k, v| {
            n += 1;
            let (Some(&ch), Some(&sh)) = (h_of_b.get(&be32(k, 2)), h_of_b.get(&be32(v, 0))) else { return };
            if sh > upto || !n.is_multiple_of(every) {
                return;
            }
            seen[2] += 1;
            if set.spender(ch, be32(k, 6), be32(k, 10)).unwrap() != Some((sh, be32(v, 4), be32(v, 8))) {
                bad += 1;
                println!("P differs for {ch}/{}/{}", be32(k, 6), be32(k, 10));
            }
        });
    }
    println!("checked: {} addresses, {} txs, {} spends; {bad} differ", seen[0], seen[1], seen[2]);
    if bad > 0 {
        std::process::exit(1);
    }
}

fn main() {
    let a: Vec<String> = std::env::args().skip(1).collect();
    let usage = "ixfiles convert|info|off|check <chain.mdbx> <chain key> | verify <file>... | bench <H|S|T> <out dir> <run file>...";
    match a.first().map(String::as_str) {
        Some("bench") if a.len() >= 4 => bench(&a[1..]),
        Some("verify") if a.len() >= 2 => {
            for path in &a[1..] {
                let r = Reader::open(std::path::Path::new(path)).unwrap();
                r.verify().unwrap();
                // Read whole here: a reader adopts it without reading it again.
                arcade_chaindb::ixset::mark_checked(std::path::Path::new(path), &r.id);
                println!("{path}: {:?} heights {}..={} rows {} bytes {} id {} ok", r.meta.kind, r.meta.first, r.meta.last, r.rows, r.bytes, hex::encode(r.id));
            }
        }
        Some("check") if a.len() == 3 || a.len() == 4 => {
            let p = *arcade_chain::params::ALL_CHAINS.iter().find(|p| p.key == a[2]).expect("no chain of that key");
            check(&ChainDb::open(&a[1]).unwrap(), p, a.get(3).map(|n| n.parse().expect("every: a number")).unwrap_or(50));
        }
        Some(cmd @ ("convert" | "info" | "off")) if a.len() == 3 => {
            let p = *arcade_chain::params::ALL_CHAINS.iter().find(|p| p.key == a[2]).expect("no chain of that key");
            let db = ChainDb::open(&a[1]).unwrap();
            let c = p.chain_no;
            match cmd {
                "convert" => {
                    catch_interrupt();
                    // The drive is watched: held under ARCADE_IX_FREE_GB (default 50) plus a
                    // pass's estimated size.
                    let floor = std::env::var("ARCADE_IX_FREE_GB").ok().map(|v| v.trim().parse::<u64>().expect("ARCADE_IX_FREE_GB: a number of GB")).map(|g| g.saturating_mul(1 << 30));
                    let o = Options { free_bytes: Some(free_bytes), floor_bytes: floor.unwrap_or(arcade_chaindb::ixconv::FLOOR_BYTES), layout: arcade_chaindb::ixconv::Layout::for_chain(p), ..Options::default() };
                    let done = db.convert_to_index_files(p, &o, &STOP, &mut |s| println!("{s}")).unwrap();
                    println!("{}", if done { "done" } else { "stopped" });
                }
                "off" => db.set_index_files_off(c).unwrap(),
                _ => {}
            }
            for (kind, upto, files, rows, bytes) in db.index_files_info(c).unwrap() {
                println!("{kind:?}: up to height {upto}, {files} file(s), {rows} rows, {bytes} bytes ({:.2} bytes a row)", bytes as f64 / rows.max(1) as f64);
            }
        }
        _ => eprintln!("{usage}"),
    }
}
