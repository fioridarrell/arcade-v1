fn main() {
    let db = arcade_chaindb::ChainDb::open(std::env::args().nth(1).unwrap()).unwrap();
    for (k, v) in db.scan_all(arcade_ledger::engine::META).unwrap() {
        println!("{} = {}", String::from_utf8_lossy(&k[2..]), if v.len() == 8 { u64::from_be_bytes(v[..8].try_into().unwrap()).to_string() } else { hex::encode(&v) });
    }
    let mut nums: Vec<u64> = db.scan_all(arcade_ledger::engine::INSCRIPTION).unwrap().iter()
        .map(|(_, v)| serde_json::from_slice::<serde_json::Value>(v).unwrap()["number"].as_u64().unwrap()).collect();
    nums.sort();
    let dups = nums.windows(2).filter(|w| w[0] == w[1]).count();
    println!("inscriptions {}, numbers {}..={}, duplicates {dups}, missing {}", nums.len(), nums[0], nums[nums.len()-1], (nums[nums.len()-1]+1) as usize - nums.len() + dups);
}
