//! Fetch one block by hash from a live peer and check it.
//!   cargo run --release -p arcade-chain --example fetch -- <chain> <block hash>
use arcade_chain::params::by_key;
use arcade_chain::{p2p::Peer, Block, Hash256};
use std::net::ToSocketAddrs;
use std::time::Duration;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let p = by_key(&a[1]).expect("chain");
    let h = Hash256::from_hex(&a[2]).expect("hash");
    let addrs: Vec<String> = p.fixed_peers.iter().map(|s| s.to_string())
        .chain(p.dns_seeds.iter().flat_map(|d| (*d, p.p2p_port).to_socket_addrs().into_iter().flatten().map(|x| x.to_string()))).collect();
    for addr in addrs {
        let Ok(mut peer) = Peer::connect(p, &addr, Duration::from_secs(15)) else { continue };
        let Ok(mut raws) = peer.get_blocks(&[h]) else { continue };
        let raw = raws.remove(0);
        let b = Block::parse_with(&raw, &p.format).expect("parse");
        b.check().expect("merkle");
        assert_eq!(b.hash(), h);
        assert_eq!(b.encode(), raw, "byte-identical");
        let wit = b.txs.iter().filter(|t| t.has_witness()).count();
        let hog = b.txs.iter().filter(|t| t.hogex).count();
        println!("{} from {}: {} bytes, {} txs ({} with witness, {} HogEx), extension {} bytes: merkle ok, byte-identical",
            h, peer.version.user_agent, raw.len(), b.txs.len(), wit, hog, b.extra.len());
        return;
    }
    panic!("no peer served it");
}
