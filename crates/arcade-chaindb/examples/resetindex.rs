//! Dev tool: drop one indexer's tables and progress so it rebuilds from stored blocks.
//!   resetindex <chain.mdbx> <indexer name> <table prefix>   (the arcade must be stopped; chain 1)
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let db = arcade_chaindb::ChainDb::open(&a[1]).unwrap();
    let st = db.store(1);
    for t in st.addon_tables().unwrap() {
        if t.starts_with(&a[3]) {
            st.drop_table(&t).unwrap();
            println!("dropped {t}");
        }
    }
    let k = arcade_chaindb::schema::k_state(1, &format!("ix:{}", a[2]));
    st.write(|w| w.del(arcade_chaindb::schema::STATE, &k)).unwrap();
    println!("reset progress of {}", a[2]);
}
