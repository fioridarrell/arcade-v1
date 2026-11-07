fn main() {
    let db = arcade_chaindb::ChainDb::open(std::env::args().nth(1).unwrap()).unwrap();
    for n in ["ledger.v1", "profiles.v1"] { println!("{n}: {:?}", db.indexed_to(1, n).unwrap()); }
}
