//! Where the bytes go in a chain.mdbx.
use arcade_chaindb::ChainDb;
fn main() {
    let path = std::env::args().nth(1).unwrap_or("chain.mdbx".into());
    let db = ChainDb::open(&path).unwrap();
    let mut rows = db.table_sizes_all().unwrap();
    rows.sort_by(|a, b| b.2.cmp(&a.2));
    let total: u64 = rows.iter().map(|r| r.2).sum();
    for (n, e, b) in &rows { println!("{n:24} {e:>12} rows {:>9.1} MB  {:>5.1}%", *b as f64 / 1e6, 100.0 * *b as f64 / total as f64); }
    println!("{:24} {:>12}      {:>9.1} MB  (file {:.1} MB)", "total", "", total as f64 / 1e6, std::fs::metadata(&path).unwrap().len() as f64 / 1e6);
}
