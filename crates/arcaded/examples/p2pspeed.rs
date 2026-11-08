//! How fast does one P2P peer hand over a run of blocks?  p2pspeed <chain> <peer> <coreshare arcade> <from> <n>
use arcade_chain::params::by_key;
use arcade_chain::{p2p::Peer, Hash256};
use arcade_mesh::link::{Frame, Link};
use serde_json::{json, Value};
use std::time::{Duration, Instant};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let p = by_key(&a[1]).unwrap();
    let (from, n): (u32, u32) = (a[4].parse().unwrap(), a[5].parse().unwrap());
    let key = ed25519_dalek::SigningKey::from_bytes(&[5; 32]);
    let mut link = Link::dial(&a[3], "arcade-main", &key, None).unwrap();
    link.set_read_timeout(Duration::from_secs(30)).unwrap();
    let calls: Vec<Value> = (from..from + n).map(|h| json!(["getblockhash", [h]])).collect();
    link.send(&Frame { head: json!({"t": "cr", "op": "q", "id": 1, "net": p.net, "seq": 0, "end": true, "z": 0}), blob: json!(calls).to_string().into_bytes() }).unwrap();
    let mut buf = Vec::new();
    loop {
        let f = link.recv().unwrap();
        if f.kind() == "cr" && f.head["op"] == "a" { buf.extend_from_slice(&f.blob); if f.head["end"] == true { break; } }
    }
    let hs: Vec<Value> = serde_json::from_slice(&buf).unwrap();
    let hashes: Vec<Hash256> = hs.iter().map(|x| Hash256::from_hex(x["r"].as_str().unwrap()).unwrap()).collect();
    let t = Instant::now();
    let mut peer = Peer::connect(p, &a[2], Duration::from_secs(10)).unwrap();
    println!("connected in {:.2}s", t.elapsed().as_secs_f64());
    let t = Instant::now();
    let raws = peer.get_blocks(&hashes).unwrap();
    let bytes: usize = raws.iter().map(|r| r.len()).sum();
    println!("{} blocks, {:.1} KB, in {:.2}s ({:.0} blocks/s)", raws.len(), bytes as f64 / 1e3, t.elapsed().as_secs_f64(), raws.len() as f64 / t.elapsed().as_secs_f64());
}
