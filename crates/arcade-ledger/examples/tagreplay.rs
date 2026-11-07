//! Replay stored blocks from the chain's index start and build the @tag table, then print
//! names_digest to compare with the mesh's `ix` tip `n`.
//!   cargo run --release -p arcade-ledger --example tagreplay -- <chain> <chain.mdbx>
use arcade_chain::params::by_key;
use arcade_chain::{Block, Tx};
use arcade_chaindb::ChainDb;
use arcade_ledger::extract::{class_b_marker, detect_class, extract, Prevout};
use arcade_ledger::tags::{self, Tags};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let p = by_key(a.get(1).map(String::as_str).unwrap_or("test")).unwrap();
    let db = ChainDb::open(a.get(2).map(String::as_str).unwrap_or("chain.mdbx")).unwrap();
    let tip = db.tip(p.chain_no).unwrap().expect("synced chain");
    let marker = class_b_marker(p);
    let (mut tags, mut marked, mut skipped, mut tag_ops, mut invalid) = (Tags::default(), 0, 0, 0, 0);
    let t = std::time::Instant::now();
    for h in p.index_start..=tip.height {
        let hash = db.hash_at(p.chain_no, h).unwrap().unwrap();
        let b = Block::parse_with(&db.block_raw(p.chain_no, &hash).unwrap().unwrap(), &p.format).unwrap();
        for (pos, tx) in b.txs.iter().enumerate() {
            if detect_class(tx, &marker, p).is_none() { continue; }
            marked += 1;
            let prev: Vec<Prevout> = tx.inputs.iter().map(|i| {
                let (raw, _) = db.tx_raw(p, &i.prevout.txid).unwrap().expect("prevout tx stored");
                let ptx = Tx::parse_with(&raw, &p.format).unwrap();
                let o = &ptx.outputs[i.prevout.vout as usize];
                Prevout { value: o.value, script: o.script_pubkey.clone() }
            }).collect();
            match extract(tx, &prev, p).unwrap() {
                Err(_) => skipped += 1,
                Ok(atx) => if let Some(op) = tags::parse(&atx.payload) {
                    tag_ops += 1;
                    match op {
                        Ok(op) => if tags.apply(&op, &atx.sender, atx.reference.as_deref(), &tx.txid.to_hex(), h, pos as u32).is_err() { invalid += 1 },
                        Err(_) => invalid += 1,
                    }
                },
            }
        }
    }
    println!("{} blocks {}..={} in {:?}: {marked} arcade txs, {skipped} unreadable, {tag_ops} tag ops ({invalid} invalid), {} tags",
        tip.height - p.index_start + 1, p.index_start, tip.height, t.elapsed(), tags.by_tag.len());
    println!("names_digest at {}: {}", tip.height, tags.digest());
    for (tag, r) in tags.by_tag.iter().take(10) { println!("  @{tag} -> {} (h {})", r.address, r.block_height); }
}
