//! Wait for the next block a live peer announces, fetch it, and check it parses,
//! passes merkle, rebuilds byte-identically, and (where implemented) meets its PoW.
//!   cargo run --release -p arcade-chain --example tipwatch -- <test|dgb> [peer] [blocks]
use arcade_chain::encode::Reader;
use arcade_chain::params::{by_key, ChainParams};
use arcade_chain::pow::{meets_target, try_pow_hash};
use arcade_chain::{p2p::Peer, Block, Hash256};
use std::net::ToSocketAddrs;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let p: &ChainParams = by_key(args.get(1).map(String::as_str).unwrap_or("dgb")).expect("chain key");
    let want: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(3);
    let addrs: Vec<String> = match args.get(2) {
        Some(a) if a != "-" => vec![a.clone()],
        _ => p.fixed_peers.iter().map(|s| s.to_string())
            .chain(p.dns_seeds.iter().flat_map(|s| (*s, p.p2p_port).to_socket_addrs().into_iter().flatten().map(|a| a.to_string())))
            .collect(),
    };
    let mut peer = addrs.iter().find_map(|a| Peer::connect(p, a, Duration::from_secs(120)).ok()).expect("no peer");
    println!("{}: {} {} tip {}", p.name, peer.addr, peer.version.user_agent, peer.version.start_height);
    let t = Instant::now();
    let mut seen = 0;
    while seen < want {
        let m = peer.recv().expect("recv");
        let hashes: Vec<Hash256> = match m.command.as_str() {
            "inv" => {
                let mut r = Reader::new(&m.payload);
                let n = r.len().unwrap();
                (0..n).filter_map(|_| { let k = r.u32().unwrap(); let h = r.hash().unwrap(); (k == 2).then_some(h) }).collect()
            }
            "headers" => arcade_chain::p2p::parse_headers(&m.payload, &p.format).unwrap().iter().map(|e| e.header.hash()).collect(),
            _ => continue,
        };
        for h in hashes {
            let raw = peer.get_blocks(&[h]).expect("getdata").remove(0);
            let b = Block::parse_with(&raw, &p.format).expect("parse");
            b.check().expect("merkle");
            assert_eq!(b.hash(), h);
            assert_eq!(b.encode(), raw, "byte-identical");
            let algo = p.pow.algo(b.header.version, u32::MAX).expect("algo");
            let pow = match algo.and_then(|a| try_pow_hash(a, &b.header.encode())) {
                Some(ph) => if meets_target(&ph, b.header.bits) { "PoW verified" } else { panic!("bad PoW") },
                None => "PoW not checkable yet",
            };
            let wit = b.txs.iter().filter(|t| t.has_witness()).count();
            let dd = b.txs.iter().filter(|t| (t.version & 0xffff) == 0x0770).count();
            if dd > 0 { println!("    {dd} DigiDollar tx(s)"); }
            println!("  +{:?} block {} algo {:?} ({pow}), version {:#x}, {} txs ({} witness), {} bytes, extra {} bytes: byte-identical",
                t.elapsed(), h, algo, b.header.version, b.txs.len(), wit, raw.len(), b.extra.len());
            println!("    header {}", hex::encode(b.header.encode()));
            seen += 1;
        }
    }
}
