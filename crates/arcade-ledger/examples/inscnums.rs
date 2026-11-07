//! Inscription numbers around a height (from the ledger's digest rows).
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let db = arcade_chaindb::ChainDb::open(&a[1]).unwrap();
    let (lo, hi): (u32, u32) = (a[2].parse().unwrap(), a[3].parse().unwrap());
    let mut last = None;
    for h in lo..=hi {
        for r in arcade_ledger::engine::block_records(&db, 1, h).unwrap() {
            if r.starts_with("inscription|") {
                let f: Vec<&str> = r.split('|').collect();
                last = Some((h, f[2].to_string()));
                if h + 300 > hi || std::env::var("ALL").is_ok() { println!("{h} #{} {}", f[2], &f[1][..16]); }
            }
        }
    }
    let n = db.store.count(arcade_ledger::engine::INSCRIPTION).unwrap();
    println!("last before {hi}: {last:?}; inscriptions stored: {n}");
}
