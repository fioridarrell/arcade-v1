//! A consistent, compacted copy of a chain file, safe while its arcaded runs (for test arcades).
//!   cargo run --example snapshot_copy -- <source chain.mdbx> <dest chain.mdbx>
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let s = arcade_store::Store::open_readonly(&a[1], &arcade_chaindb::schema::CHAIN_SCHEMA).expect("source");
    s.compact_copy(&a[2]).expect("copy");
    eprintln!("copied {} -> {}", a[1], a[2]);
}
