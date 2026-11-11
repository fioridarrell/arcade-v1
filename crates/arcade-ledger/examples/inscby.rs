fn main() {
    let a: Vec<String> = std::env::args().collect();
    let db = arcade_chaindb::ChainDb::open(&a[1]).unwrap();
    let (lo, hi): (u64, u64) = (a[2].parse().unwrap(), a[3].parse().unwrap());
    let mut v: Vec<serde_json::Value> = db.scan_all(arcade_ledger::engine::INSCRIPTION).unwrap().iter()
        .map(|(_, v)| serde_json::from_slice::<serde_json::Value>(v).unwrap()).filter(|i| (lo..=hi).contains(&i["number"].as_u64().unwrap())).collect();
    v.sort_by_key(|i| i["number"].as_u64());
    for i in v { println!("#{} h{} pos{} {} chunks {}", i["number"], i["block_height"], i["position"], &i["txid"].as_str().unwrap()[..16], i["chunks"]); }
    // chunks still pending
    let pend = db.count_all(arcade_ledger::engine::CHUNK).unwrap();
    println!("pending chunk rows: {pend}");
}
