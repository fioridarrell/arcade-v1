//! Connect to one coin peer and report the handshake: `peer_probe <chain key> <host:port>`.
use std::time::Duration;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let p = arcade_chain::params::ALL_CHAINS.iter().find(|c| c.key == a[1]).expect("chain");
    match arcade_chain::p2p::Peer::connect(p, &a[2], Duration::from_secs(20)) {
        Ok(_) => println!("connected"),
        Err(e) => println!("error: {e}"),
    }
}
