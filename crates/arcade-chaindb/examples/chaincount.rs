fn main() {
    let a: Vec<String> = std::env::args().collect();
    let db = arcade_chaindb::ChainDb::open(&a[1]).unwrap();
    let c: u16 = a[2].parse().unwrap();
    let pre = arcade_chaindb::schema::k_chain(c);
    let mut tables: Vec<String> = arcade_chaindb::schema::CHAIN_SCHEMA.tables.iter().map(|t| t.name.to_string()).collect();
    tables.extend(db.store.addon_tables().unwrap());
    let n: usize = tables.iter().map(|t| db.store.read(|r| r.scan_prefix(t, &pre)).map(|v| v.len()).unwrap_or(0)).sum();
    println!("rows stored for chain {c}: {n}");
}
