//! Live check against a real peer: download header batches from genesis, verify every
//! header (link, per-algorithm difficulty step, PoW where implemented), then fetch the
//! last block and check it rebuilds byte-identically.
//!
//!   cargo run --release -p arcade-chain --example probe -- <test|dgb> [peer] [batches]
use arcade_chain::params::{by_key, check_header, ChainParams, PowCheck};
use arcade_chain::{p2p::Peer, Block, Header};
use std::collections::HashMap;
#[allow(unused_imports)]
use arcade_chain::Format;
use std::net::ToSocketAddrs;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let p: &ChainParams = by_key(args.get(1).map(String::as_str).unwrap_or("test")).expect("chain key");
    let batches: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);
    let candidates: Vec<String> = match args.get(2) {
        Some(a) if a != "-" => vec![a.clone()],
        _ => p
            .fixed_peers
            .iter()
            .map(|s| s.to_string())
            .chain(p.dns_seeds.iter().flat_map(|s| {
                (*s, p.p2p_port).to_socket_addrs().into_iter().flatten().map(|a| a.to_string())
            }))
            .collect(),
    };

    let t = Instant::now();
    let mut peer = candidates
        .iter()
        .find_map(|a| match Peer::connect(p, a, Duration::from_secs(10)) {
            Ok(peer) => Some(peer),
            Err(e) => {
                eprintln!("  {a}: {e}");
                None
            }
        })
        .expect("no peer reachable");
    println!("{}: connected {} {:?} in {:?}", p.name, peer.addr, peer.version, t.elapsed());

    let genesis = match p.genesis_header() {
        Some(g) => g,
        None => {
            let raw = peer.get_blocks(&[p.genesis_hash()]).expect("genesis block").remove(0);
            let b = Block::parse_with(&raw, &p.format).expect("genesis parse");
            b.check().expect("genesis merkle");
            b.header
        }
    };
    assert_eq!(p.block_id(&genesis), p.genesis_hash(), "genesis hash");
    println!("genesis {} ok, header {}", p.block_id(&genesis), hex::encode(genesis.encode()));

    let mut tip: Header = genesis;
    let mut height = 0u32;
    let mut counts: HashMap<String, (u32, u32, u32)> = HashMap::new(); // algo -> (pow verified, unchecked, proof-of-stake blocks)
    for _ in 0..batches {
        let hs = peer.get_headers(&[p.block_id(&tip)]).expect("headers");
        if hs.is_empty() {
            break;
        }
        for e in &hs {
            let algo = p.pow.algo(e.header.version, height + 1);
            let r = check_header(p, &e.header, &tip, height + 1)
                .unwrap_or_else(|err| panic!("height {}: {err}", height + 1));
            let c = counts.entry(format!("{:?}", algo.flatten())).or_default();
            match r {
                PowCheck::Verified => c.0 += 1,
                PowCheck::NeedsBlock => {
                    eprintln!("needs block at {} id {} version {} bits {:x}", height + 1, p.block_id(&e.header), e.header.version, e.header.bits);
                    let raw = peer.get_blocks(&[p.block_id(&e.header)]).expect("block").remove(0);
                    let b = Block::parse_with(&raw, &p.format).expect("parse");
                    b.check().expect("merkle");
                    assert_eq!(b.encode(), raw);
                    match arcade_chain::params::check_block_work(p, &b, height + 1) {
                        Ok(PowCheck::NotCheckable) => c.2 += 1,
                        Ok(_) => c.0 += 1,
                        Err(err) => panic!("height {}: {err}", height + 1),
                    }
                }
                _ => c.1 += 1,
            }
            tip = e.header;
            height += 1;
        }
        println!("verified headers to height {height}, {:?}", t.elapsed());
    }
    println!("per algorithm (pow verified, not checkable, stake blocks): {counts:?}");

    let raw = peer.get_blocks(&[p.block_id(&tip)]).expect("block").remove(0);
    let b = Block::parse_with(&raw, &p.format).expect("parse");
    b.check().expect("merkle");
    assert_eq!(b.hash(), p.block_id(&tip));
    assert_eq!(b.encode(), raw, "rebuilt block is byte-identical");
    let wit = b.txs.iter().filter(|t| t.has_witness()).count();
    println!(
        "block {height} {} ok: {} txs ({} with witness), {} bytes, byte-identical",
        b.hash(),
        b.txs.len(),
        wit,
        raw.len()
    );
}
