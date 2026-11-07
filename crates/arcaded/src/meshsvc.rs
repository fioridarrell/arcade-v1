//! Mesh thread per seed: keep a link to an existing arcade, answer pings, learn index tips
//! and peers, and send queued frames (welcome asks).

use crate::node::{LinkInfo, MeshTip, Node};
use arcade_mesh::link::{Frame, Link, LinkError};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub const NET: &str = "arcade-main"; // one mesh for every chain (verified live)

pub const SEEDS: &[&str] = &[
    "156.67.28.112:8421",
    "207.244.232.43:8421",
    "147.124.204.26:8421",
    "192.168.1.25:8421",
];

fn now() -> u64 {
    arcade_app::now()
}

pub fn run(node: Arc<Node>, addr: String) {
    let mut fails = 0u32;
    loop {
        match Link::dial(&addr, NET, &node.identity, None) {
            Err(_) => {
                fails += 1;
                let wait = (5u64 << fails.min(7)).min(600);
                std::thread::sleep(Duration::from_secs(wait));
                continue;
            }
            Ok(mut link) => {
                fails = 0;
                let id = link.peer_id.clone();
                node.links.lock().unwrap().insert(addr.clone(), LinkInfo { addr: addr.clone(), node_id: id.clone(), since: now() });
                let _ = node.app.put_peer(&arcade_app::Peer {
                    id: id.clone(),
                    host: addr.rsplit_once(':').map(|x| x.0).unwrap_or(&addr).into(),
                    port: addr.rsplit_once(':').and_then(|x| x.1.parse().ok()).unwrap_or(8421),
                    source: "seed".into(),
                    last_seen: now(),
                    failures: 0,
                });
                let _ = node.app.event("mesh", &format!("linked to {addr} ({})", &id[..16]));
                let r = serve(&node, &mut link, &addr);
                node.links.lock().unwrap().remove(&addr);
                let _ = node.app.event("mesh", &format!("link to {addr} ended: {}", r.err().map(|e| e.to_string()).unwrap_or_default()));
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn serve(node: &Node, link: &mut Link, addr: &str) -> Result<(), LinkError> {
    link.set_read_timeout(Duration::from_secs(5))?;
    let mut last_ping = std::time::Instant::now();
    let mut sent_outbox = 0usize;
    loop {
        // Queued frames: every link sends every frame once.
        let pending: Vec<Value> = node.mesh_outbox.lock().unwrap().clone();
        for f in pending.iter().skip(sent_outbox) {
            link.send(&Frame::new(f.clone()))?;
        }
        sent_outbox = pending.len();
        if last_ping.elapsed() > Duration::from_secs(15) {
            link.send(&Frame::new(json!({"t": "ping", "n": format!("{:08x}", now() as u32)})))?;
            last_ping = std::time::Instant::now();
        }
        let f = match link.recv() {
            Ok(f) => f,
            Err(LinkError::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e),
        };
        match f.kind() {
            "ping" => {
                let n: String = f.head["n"].as_str().unwrap_or("").chars().take(16).collect();
                link.send(&Frame::new(json!({"t": "pong", "n": n})))?;
            }
            "ix" if f.head["op"] == "tip" => {
                if let Some(t) = f.head["nets"].get(node.params.key) {
                    node.mesh_tips.lock().unwrap().insert(
                        addr.to_string(),
                        MeshTip {
                            height: t["h"].as_u64().unwrap_or(0),
                            names_digest: t["n"].as_str().map(String::from),
                            from: link.peer_id.clone(),
                        },
                    );
                }
            }
            "wl" => crate::faucet::on_welcome(node, &f.head),
            _ => {} // unknown kinds are ignored, as the existing arcade does
        }
    }
}
