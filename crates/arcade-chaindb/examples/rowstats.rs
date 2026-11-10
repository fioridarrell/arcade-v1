//! Average key/value bytes per table (actual data, not pages).
use arcade_chaindb::{schema::*, ChainDb};
fn main() {
    let db = ChainDb::open(std::env::args().nth(1).unwrap()).unwrap();
    for t in [OUTPUTS, SCRIPT_HIST, BY_SCRIPT, BLOCK_TX, HEADERS, BLOCK_META, TXS] {
        let (mut n, mut k, mut v, mut big) = (0u64, 0u64, 0u64, 0u64);
        let rows = db.scan_all(t).unwrap();
        for (kk, vv) in &rows { n += 1; k += kk.len() as u64; v += vv.len() as u64; if vv.len() > 200 { big += 1 } }
        println!("{t:22} {n:>9} rows  key {:>5.1} B  value {:>6.1} B  data {:>7.1} MB  values>200B: {big}", k as f64 / n as f64, v as f64 / n as f64, (k + v) as f64 / 1e6);
    }
}
