//! Mesh thread per seed: keep a link to an existing arcade, answer pings, learn index tips
//! and peers, and send queued frames (welcome asks).

use crate::node::{LinkInfo, MeshTip, Node};
use arcade_mesh::link::{Frame, Link, LinkError};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

pub const NET: &str = "arcade-main"; // one mesh for every chain (verified live)

pub const SEEDS: &[&str] = &[
    "wss://app.dogecoinarcade.com/mesh",
    "wss://pepecoinarcade.com/mesh",
    "156.67.28.112:8421",
    "207.244.232.43:8421",
    "147.124.204.26:8421",
    "192.168.1.25:8421",
];

fn now() -> u64 {
    arcade_app::now()
}

/// Most mesh links we dial out to (the existing arcade links "everyone" up to 48).
pub const MAX_OUTBOUND: usize = 24;

/// Start a dialing thread for a mesh address unless one exists.
pub fn dial(node: &Arc<Node>, addr: String) {
    {
        let mut d = node.dialing.lock().unwrap();
        if d.contains(&addr) || d.len() >= MAX_OUTBOUND {
            return;
        }
        d.insert(addr.clone());
    }
    let n = node.clone();
    std::thread::Builder::new().name(format!("mesh {addr}")).spawn(move || run(n, addr)).unwrap();
}

/// What we share on every link: arcades we know and coin peers that work.
fn share(node: &Node, link: &mut Link) -> Result<(), LinkError> {
    let peers = node.app.peers().unwrap_or_default();
    let a: Vec<Value> = peers.iter().take(50).map(|p| json!({"h": p.host, "p": p.port, "id": p.id})).collect();
    if !a.is_empty() {
        link.send(&Frame::new(json!({"t": "addrs", "a": a})))?;
    }
    let nets: serde_json::Map<String, Value> = node
        .coin_peers
        .lock()
        .unwrap()
        .iter()
        .map(|(k, v)| (k.clone(), json!(v.iter().take(30).collect::<Vec<_>>())))
        .collect();
    if !nets.is_empty() {
        link.send(&Frame::new(json!({"t": "pp", "nets": nets})))?;
    }
    Ok(())
}

fn on_addrs(node: &Node, h: &Value) {
    let Some(list) = h["a"].as_array() else { return };
    let me = hex::encode(node.identity.verifying_key().to_bytes());
    for a in list.iter().take(50) {
        let (Some(host), Some(port), id) = (a["h"].as_str(), a["p"].as_u64(), a["id"].as_str().unwrap_or("")) else { continue };
        if id == me || port == 0 || port > 65535 || host.len() > 255 {
            continue;
        }
        let addr = if host.starts_with("wss://") {
            host.to_string()
        } else if host.contains('/') {
            continue;
        } else if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let known = node.app.peers().unwrap_or_default().iter().any(|p| p.id == id && !id.is_empty());
        if !known {
            let _ = node.app.put_peer(&arcade_app::Peer {
                id: if id.is_empty() { addr.clone() } else { id.to_string() },
                host: host.into(),
                port: port as u16,
                source: "gossip".into(),
                last_seen: 0,
                failures: 0,
            });
        }
        if let Some(n) = node.me.get().and_then(|w| w.upgrade()) {
            dial(&n, addr);
        }
    }
}

fn on_coin_peers(node: &Node, h: &Value) {
    let Some(nets) = h["nets"].as_object() else { return };
    for (key, list) in nets {
        if arcade_chain::params::by_key(key).is_none() {
            continue;
        }
        for a in list.as_array().into_iter().flatten().take(30) {
            if let Some(s) = a.as_str() {
                if s.parse::<std::net::SocketAddr>().is_ok() {
                    node.note_coin_peer(key, s);
                }
            }
        }
    }
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
                    host: if addr.starts_with("wss://") { addr.clone() } else { addr.rsplit_once(':').map(|x| x.0.trim_matches(|c| c == '[' || c == ']')).unwrap_or(&addr).into() },
                    port: if addr.starts_with("wss://") { 443 } else { addr.rsplit_once(':').and_then(|x| x.1.parse().ok()).unwrap_or(8421) },
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

/// Accept inbound mesh links (other V1.0 arcades, and existing arcades dialing us).
pub fn listen(node: Arc<Node>, port: u16) {
    let l = match std::net::TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(e) => {
            let _ = node.app.event("mesh", &format!("cannot listen on {port}: {e}"));
            return;
        }
    };
    for s in l.incoming().flatten() {
        let node = node.clone();
        std::thread::spawn(move || {
            let peer_ip = s.peer_addr().map(|a| a.ip().to_string()).unwrap_or_default();
            let Ok(mut link) = Link::handshake(s, NET, &node.identity, false, node.mesh_port) else { return };
            let addr = format!("{peer_ip}:{}", link.peer_listen.unwrap_or(0));
            node.links.lock().unwrap().insert(addr.clone(), LinkInfo { addr: addr.clone(), node_id: link.peer_id.clone(), since: now() });
            let _ = serve(&node, &mut link, &addr);
            node.links.lock().unwrap().remove(&addr);
        });
    }
}

fn our_tip(node: &Node) -> u64 {
    node.chain.tip(node.params.chain_no).ok().flatten().map(|t| t.height as u64).unwrap_or(0)
}

fn serve(node: &Node, link: &mut Link, addr: &str) -> Result<(), LinkError> {
    link.set_read_timeout(Duration::from_secs(5))?;
    // Tell V1.0 peers what we can serve (existing arcades ignore this kind).
    link.send(&arcade_mesh::bk::hi(json!({ node.params.key: {"h": our_tip(node)} }), node.mesh_port))?;
    // Every arcade is a seed: share the arcades and coin peers we know.
    share(node, link)?;
    // Offer our chain to arcades that borrow one (existing arcades' coreshare).
    if let Some(o) = crate::coreshare::offer(node) {
        link.send(&o)?;
    }
    // V1.0: what each installed chain and index costs on disk here (existing arcades ignore it).
    let mut last_sz = std::time::Instant::now();
    if let Some(sz) = crate::sizes::frame(node) {
        link.send(&Frame::new(sz))?;
    }
    let mut cr_reqs = crate::coreshare::Requests::default();
    let mut last_ping = std::time::Instant::now();
    let mut sent_outbox = 0usize;
    loop {
        // Queued frames: every link sends every frame once.
        let pending: Vec<Value> = node.mesh_outbox.lock().unwrap().clone();
        for f in pending.iter().skip(sent_outbox) {
            link.send(&Frame::new(f.clone()))?;
        }
        sent_outbox = pending.len();
        if last_sz.elapsed() > Duration::from_secs(3600) {
            if let Some(sz) = crate::sizes::frame(node) {
                link.send(&Frame::new(sz))?;
            }
            last_sz = std::time::Instant::now();
        }
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
                            at: t["at"].as_u64().unwrap_or(0),
                            d: t["d"].as_str().map(String::from),
                            s: t["s"].as_str().map(String::from),
                            from: link.peer_id.clone(),
                        },
                    );
                }
            }
            "wl" => crate::faucet::on_welcome(node, &f.head),
            "addrs" => on_addrs(node, &f.head),
            "pp" => on_coin_peers(node, &f.head),
            "sz" => crate::sizes::on_sz(node, &link.peer_id, &f.head),
            "bk" => serve_bk(node, link, addr, &f.head)?,
            "cr" => crate::coreshare::handle(node, link, &mut cr_reqs, &f)?,
            _ => {} // unknown kinds are ignored, as the existing arcade does
        }
    }
}

fn serve_bk(node: &Node, link: &mut Link, addr: &str, h: &Value) -> Result<(), LinkError> {
    use arcade_chain::Hash256;
    use arcade_mesh::bk;
    let p = node.params;
    let hashes = |k: &str| -> Vec<Hash256> {
        h[k].as_array().map(|a| a.iter().filter_map(|x| x.as_str().and_then(|s| Hash256::from_hex(s).ok())).collect()).unwrap_or_default()
    };
    let id = h["id"].as_u64().unwrap_or(0);
    match h["op"].as_str().unwrap_or("") {
        "hi" => {
            if let (Some(t), Some(port)) = (h["nets"][p.key]["h"].as_u64(), h["port"].as_u64()) {
                let host = addr.rsplit_once(':').map(|x| x.0).unwrap_or(addr);
                node.bk_peers.lock().unwrap().insert(format!("{host}:{port}"), t);
            }
        }
        "hq" if h["net"] == p.key => {
            let r = node.chain.headers_payload(p, &hashes("loc"), 2000).map_err(|e| e.to_string());
            bk::send_answer(link, id, r)?;
        }
        "bq" if h["net"] == p.key => {
            let want = hashes("h");
            let r = if want.len() > bk::MAX_BLOCKS {
                Err("too many blocks asked".to_string())
            } else {
                let mut out = Vec::new();
                let mut err = None;
                for x in &want {
                    match node.chain.block_raw(p.chain_no, x) {
                        Ok(Some(b)) => out.push(b),
                        _ => {
                            err = Some(format!("do not have {x}"));
                            break;
                        }
                    }
                }
                match err {
                    Some(e) => Err(e),
                    None => Ok(bk::join_blocks(&out)),
                }
            };
            bk::send_answer(link, id, r)?;
        }
        _ => {}
    }
    Ok(())
}
