//! Print the raw hex of transactions waiting in an arcade's mempool (read only, safe while it runs).
//!   cargo run --example mempool_raw -- <home> <chain key> <txid>...
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let db = arcade_chaindb::ChainDb::open_readonly(format!("{}/chain.mdbx", a[1])).expect("home");
    let p = arcade_chain::params::by_key(&a[2]).expect("chain");
    let pool = db.mempool(p.chain_no).expect("mempool");
    for want in &a[3..] {
        let hit = pool.iter().filter_map(|(_, raw)| arcade_chain::Tx::parse_with(raw, &p.format).ok().map(|t| (t, raw))).find(|(t, _)| &t.txid.to_hex() == want);
        match hit {
            Some((_, raw)) => println!("{want} {}", hex::encode(raw)),
            None => eprintln!("{want} not waiting"),
        }
    }
}
