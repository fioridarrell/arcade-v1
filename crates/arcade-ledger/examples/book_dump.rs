//! Print the mined coin book of every token (docs/MARKET.md step 1 check), as JSON lines:
//! {"pid", "asks": [[order, address, tokens, coins]], "bids": [...]}. Opens the store read only.
//!   cargo run --example book_dump -- <home> [chain key]
use arcade_chaindb::ChainDb;
use arcade_ledger::market as mk;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let db = ChainDb::open_readonly(format!("{}/chain.mdbx", a[1])).expect("home");
    let p = arcade_chain::params::by_key(a.get(2).map(String::as_str).unwrap_or("peptest")).expect("chain");
    let orders = mk::book_orders(&db, p.chain_no);
    eprintln!("{} orders on the book", orders.len());
    let rows = |rs: &[mk::Row]| rs.iter().map(|r| serde_json::json!([r.order.txid, r.order.address, r.base, r.quote])).collect::<Vec<_>>();
    for pid in mk::coin_markets(&orders) {
        let b = mk::coin_book(&orders, pid, 500);
        println!("{}", serde_json::json!({"pid": pid, "asks": rows(&b.asks), "bids": rows(&b.bids)}));
    }
    for t in mk::takes(&db, p.chain_no, None) {
        println!("{}", serde_json::json!({"take": [t["txid"], t["property_id"], t["tokens"], t["coins"], t["maker"]]}));
    }
    let t0 = std::time::Instant::now();
    let trades = mk::pair_trades_all(&db, p.chain_no);
    eprintln!("pair trades read in {:?}", t0.elapsed());
    for t in &trades {
        println!("{}", serde_json::json!({"trade": [t.height, t.txid, t.seq, t.position, t.taker, t.maker, t.maker_txid, t.gave_property, t.gave, t.got_property, t.got]}));
    }
    for (x, y) in mk::token_markets(&orders, &trades) {
        let b = mk::pair_book(&orders, x, y, 500);
        println!("{}", serde_json::json!({"pair": [x, y], "asks": rows(&b.asks), "bids": rows(&b.bids)}));
    }
}
