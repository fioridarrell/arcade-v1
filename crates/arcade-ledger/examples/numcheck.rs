fn main() {
    let db = arcade_chaindb::ChainDb::open(std::env::args().nth(1).unwrap()).unwrap();
    use arcade_ledger::engine::{BY_NUMBER, INSCRIPTION};
    let byn = db.scan_all(BY_NUMBER).unwrap();
    let mut shown = 0;
    let mut orphans = 0;
    for (k, v) in &byn {
        let n = u64::from_be_bytes(k[k.len()-8..].try_into().unwrap());
        let txid = String::from_utf8_lossy(v).to_string();
        let mut key = vec![0u8, 1u8]; key.extend_from_slice(txid.as_bytes());
        let i = db.store_for(&key).read(|r| r.get(INSCRIPTION, &key)).unwrap();
        let ok = i.as_ref().map(|v| serde_json::from_slice::<serde_json::Value>(v).unwrap()["number"].as_u64() == Some(n)).unwrap_or(false);
        if !ok { orphans += 1; if shown < 6 { shown += 1;
            println!("#{n} -> {} : inscription row {}", &txid[..16], match &i { None => "MISSING".into(), Some(v) => format!("has number {}", serde_json::from_slice::<serde_json::Value>(v).unwrap()["number"]) }); } }
    }
    println!("by_number entries {}, orphans {orphans}", byn.len());
}
