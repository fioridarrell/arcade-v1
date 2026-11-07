//! Chain thread: keep the chain synced from coin P2P peers, send queued transactions,
//! and fold new blocks into the tag index.

use crate::node::Node;
use arcade_chain::p2p::Peer;
use arcade_chaindb::sync::Syncer;
use std::net::ToSocketAddrs;
use std::sync::Arc;
use std::time::Duration;

fn candidates(node: &Node) -> Vec<String> {
    let p = node.params;
    p.fixed_peers
        .iter()
        .map(|s| s.to_string())
        .chain(p.dns_seeds.iter().flat_map(|d| {
            (*d, p.p2p_port).to_socket_addrs().into_iter().flatten().map(|a| a.to_string())
        }))
        .collect()
}

fn set_state(node: &Node, s: String) {
    *node.sync_state.lock().unwrap() = s;
}

pub fn run(node: Arc<Node>) {
    let p = node.params;
    let syncer = Syncer::new(&node.chain, p);
    loop {
        let Some(mut peer) = candidates(&node).iter().find_map(|a| Peer::connect(p, a, Duration::from_secs(20)).ok())
        else {
            set_state(&node, "no coin peer reachable; retrying".into());
            std::thread::sleep(Duration::from_secs(15));
            continue;
        };
        let _ = node.app.event("chain", &format!("connected to {} ({})", peer.addr, peer.version.user_agent));
        if let Err(e) = syncer.ensure_genesis(&mut peer) {
            set_state(&node, format!("genesis: {e}"));
            continue;
        }
        loop {
            // Headers, then blocks, then queued transactions, then the tag index.
            let mut new = 0;
            match syncer.sync_headers_once(&mut peer) {
                Ok(n) => new += n,
                Err(e) => {
                    set_state(&node, format!("headers: {e}"));
                    break;
                }
            }
            match syncer.sync_blocks(&mut peer, 2000, 250) {
                Ok(pr) => new += pr.blocks,
                Err(e) => {
                    set_state(&node, format!("blocks: {e}"));
                    break;
                }
            }
            let txs: Vec<Vec<u8>> = std::mem::take(&mut *node.tx_outbox.lock().unwrap());
            for raw in txs {
                if let Err(e) = peer.send_tx(&raw) {
                    node.tx_outbox.lock().unwrap().push(raw);
                    set_state(&node, format!("send tx: {e}"));
                }
            }
            if let Err(e) = crate::ledger::catch_up(&node) {
                set_state(&node, format!("tag index: {e}"));
            }
            let tip = node.chain.tip(p.chain_no).ok().flatten();
            let best = node.chain.best_header(p.chain_no).ok().flatten().and_then(|h| node.chain.header(p.chain_no, &h).ok().flatten());
            let (th, bh) = (tip.map(|t| t.height).unwrap_or(0), best.map(|b| b.height).unwrap_or(0));
            set_state(&node, if th >= bh { format!("synced at {th}") } else { format!("syncing {th} / {bh}") });
            if new == 0 {
                // Caught up: wait a little for new blocks (testnet: one a minute).
                std::thread::sleep(Duration::from_secs(10));
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}
