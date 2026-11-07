//! Live check: connect to a Pepecoin testnet peer, download the first header batches,
//! verify every header (link, MAX_EASIER, scrypt PoW), then fetch and check one block.
use arcade_chain::params::{check_header, PEPECOIN_TEST};
use arcade_chain::{p2p::Peer, Block};
use std::time::{Duration, Instant};

fn main() {
    let p = &PEPECOIN_TEST;
    let addr = std::env::args().nth(1).unwrap_or_else(|| p.fixed_peers[0].to_string());
    let batches: usize = std::env::args().nth(2).and_then(|s| s.parse().ok()).unwrap_or(3);
    let t = Instant::now();
    let mut peer = Peer::connect(p, &addr, Duration::from_secs(15)).expect("connect");
    println!("connected {} {:?} in {:?}", peer.addr, peer.version, t.elapsed());

    let mut tip = p.genesis_header();
    let mut height = 0u32;
    for _ in 0..batches {
        let hs = peer.get_headers(&[tip.hash()]).expect("headers");
        if hs.is_empty() { break; }
        for e in &hs {
            check_header(p, &e.header, &tip).unwrap_or_else(|err| panic!("height {}: {err}", height + 1));
            tip = e.header;
            height += 1;
        }
        println!("verified headers to height {height} ({} in batch), {:?}", hs.len(), t.elapsed());
    }
    let raw = peer.get_blocks(&[tip.hash()]).expect("block").remove(0);
    let b = Block::parse(&raw).expect("parse");
    b.check().expect("merkle");
    assert_eq!(b.hash(), tip.hash());
    assert_eq!(b.encode(), raw, "rebuilt block is byte-identical");
    println!("block {height} {} ok: {} txs, {} bytes, byte-identical", b.hash(), b.txs.len(), raw.len());
}
