//! The coin set of a chain file: its tip, the number of unspent outputs and their total.
//! For comparing an install with another copy of the chain (Core's gettxoutsetinfo
//! counts the same: unspendable outputs are never coins). Run with the arcade stopped (its
//! coin cache is written at a clean stop).
//!
//!     cargo run --release -p arcade-chaindb --example coinstats -- <chain.mdbx> <chain key>
use arcade_chaindb::schema::{k_chain, OUTPUTS};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let p = arcade_chain::params::by_key(&a[2]).expect("chain key");
    let db = arcade_chaindb::ChainDb::open(&a[1]).unwrap();
    let c = p.chain_no;
    let tip = db.tip(c).unwrap().expect("no tip");
    let pre = k_chain(c);
    let store = db.store(c);
    let (mut n, mut total, mut after): (u64, u128, Option<Vec<u8>>) = (0, 0, None);
    loop {
        let rows = store.read(|r| r.scan_after(OUTPUTS, &pre, after.as_deref(), 200_000)).unwrap();
        let Some(last) = rows.last() else { break };
        for (_, v) in &rows {
            n += 1;
            total += u64::from_le_bytes(v[0..8].try_into().unwrap()) as u128;
        }
        after = Some(last.0.clone());
    }
    let unit = 10u128.pow(8);
    println!("{} tip {} {} · coins {n} · total {}.{:08}", p.key, tip.height, tip.hash, total / unit, total % unit);
}
