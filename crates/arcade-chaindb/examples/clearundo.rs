//! Dev tool: drop undo records written in an older format (the arcade must be stopped).
fn main() {
    let db = arcade_chaindb::ChainDb::open(std::env::args().nth(1).unwrap()).unwrap();
    let n = db.store.count(arcade_chaindb::schema::UNDO).unwrap();
    db.store.write(|w| w.clear(arcade_chaindb::schema::UNDO)).unwrap();
    println!("cleared {n} undo records");
}
