//! Write a compacted copy of a chain.mdbx (the shared file, or a chain's own) and report the sizes.
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let db = arcade_chaindb::ChainDb::open(&a[1]).unwrap();
    let t = std::time::Instant::now();
    db.legacy_store().compact_copy(&a[2]).unwrap();
    let before = std::fs::metadata(&a[1]).unwrap().len();
    let after = std::fs::metadata(&a[2]).unwrap().len();
    println!("compacted {:.2} GB -> {:.2} GB in {:?}", before as f64 / 1e9, after as f64 / 1e9, t.elapsed());
}
