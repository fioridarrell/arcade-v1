//! Listen to a live peer's transaction announcements for N seconds and parse each tx.
use arcade_chain::encode::Reader;
use arcade_chain::p2p::{getdata_payload, Peer};
use arcade_chain::params::by_key;
use std::net::ToSocketAddrs;
use std::time::{Duration, Instant};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let p = by_key(&a[1]).unwrap();
    let secs: u64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(30);
    let addrs: Vec<String> = p.fixed_peers.iter().map(|s| s.to_string()).chain(p.dns_seeds.iter().flat_map(|d| (*d, p.p2p_port).to_socket_addrs().into_iter().flatten().map(|x| x.to_string()))).collect();
    let mut peer = addrs.iter().find_map(|x| Peer::connect_with(p, x, Duration::from_secs(15), true).ok()).expect("peer");
    peer.set_read_timeout(Duration::from_secs(2)).unwrap();
    let t = Instant::now();
    let (mut announced, mut parsed, mut bad, mut wit) = (0, 0, 0, 0);
    while t.elapsed() < Duration::from_secs(secs) {
        let Ok(m) = peer.recv() else { continue };
        if m.command == "inv" {
            let mut r = Reader::new(&m.payload);
            let n = r.len().unwrap();
            let txs: Vec<_> = (0..n).filter_map(|_| { let k = r.u32().unwrap(); let h = r.hash().unwrap(); (k & 0xff == 1).then_some(h) }).collect();
            announced += txs.len();
            if !txs.is_empty() { let k = peer.tx_inv(); peer.send("getdata", &getdata_payload(k, &txs)).unwrap(); }
        } else if m.command == "tx" {
            match arcade_chain::Tx::parse_with(&m.payload, &p.format) {
                Ok(tx) => { parsed += 1; if tx.has_witness() { wit += 1 } assert_eq!(tx.raw, m.payload); }
                Err(_) => bad += 1,
            }
        }
    }
    println!("{} via {}: {secs}s, {announced} announced, {parsed} parsed byte-identically ({wit} with witness), {bad} unreadable", p.name, peer.version.user_agent);
}
