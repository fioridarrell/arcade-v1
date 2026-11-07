//! Local API: one JSON request per line on a Unix socket, one JSON answer per line.
//! Used by the `arcade` CLI and the desktop app.

use crate::node::Node;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;

pub fn serve(node: Arc<Node>, path: std::path::PathBuf) {
    let _ = std::fs::remove_file(&path);
    let l = UnixListener::bind(&path).expect("bind API socket");
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o660) /* service user + arcade group */);
    for s in l.incoming().flatten() {
        let node = node.clone();
        std::thread::spawn(move || handle(node, s));
    }
}

fn handle(node: Arc<Node>, s: UnixStream) {
    let mut w = s.try_clone().unwrap();
    for line in BufReader::new(s).lines().map_while(Result::ok) {
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(req) => match dispatch(&node, &req) {
                Ok(v) => json!({"ok": true, "result": v}),
                Err(e) => json!({"ok": false, "error": e}),
            },
            Err(e) => json!({"ok": false, "error": format!("bad request: {e}")}),
        };
        if writeln!(w, "{reply}").is_err() {
            break;
        }
    }
}

fn dispatch(node: &Node, req: &Value) -> Result<Value, String> {
    let s = |k: &str| req[k].as_str().map(String::from);
    let p = node.params;
    let c = p.chain_no;
    match req["cmd"].as_str().unwrap_or("") {
        "status" => {
            let tip = node.chain.tip(c).map_err(|e| e.to_string())?;
            let idx = node.tags.lock().unwrap();
            let tips = node.mesh_tips.lock().unwrap();
            let mesh: Vec<Value> = tips
                .iter()
                .map(|(a, t)| json!({"peer": a, "height": t.height, "names_digest": t.names_digest}))
                .collect();
            Ok(json!({
                "chain": p.name,
                "sync": *node.sync_state.lock().unwrap(),
                "tip": tip.map(|t| json!({"height": t.height, "hash": t.hash.to_hex()})),
                "tags": {"indexed_to": idx.height, "count": idx.tags.by_tag.len(), "names_digest": idx.tags.digest()},
                "mesh": {"links": node.links.lock().unwrap().len(), "tips": mesh},
                "node_id": hex::encode(node.identity.verifying_key().to_bytes()),
                "wallet": node.app.has_wallet().unwrap_or(false),
            }))
        }
        "peers" => Ok(json!(node.links.lock().unwrap().values().map(|l| json!({"addr": l.addr, "node_id": l.node_id, "since": l.since})).collect::<Vec<_>>())),
        "wallet.create" => {
            let phrase = node.app.create_wallet(s("phrase").as_deref()).map_err(|e| e.to_string())?;
            let restored = s("phrase").is_some();
            Ok(json!({"phrase": if restored { Value::Null } else { json!(phrase) }, "restored": restored}))
        }
        "wallet.phrase" => Ok(json!({"phrase": node.app.phrase().map_err(|e| e.to_string())?})),
        "wallet.keys" => {
            let keys = node.app.tag_keys(c).map_err(|e| e.to_string())?;
            Ok(json!(keys.iter().map(|k| {
                let script = arcade_wallet::keys::p2pkh_script_for(&arcade_wallet::secp256k1::PublicKey::from_slice(&hex::decode(&k.pubkey).unwrap()).unwrap());
                json!({"index": k.index, "address": k.address, "tag": k.tag, "balance": node.chain.balance_for_script(c, &script).unwrap_or(0)})
            }).collect::<Vec<_>>()))
        }
        "tag.check" => Ok(json!(crate::claims::check(node, &s("tag").ok_or("tag required")?))),
        "tag.claim" => Ok(json!(crate::claims::start(node, &s("tag").ok_or("tag required")?)?)),
        "tag.list" => Ok(json!(node.app.claims().map_err(|e| e.to_string())?)),
        "faucet.ask" => {
            let (k, _) = node.app.tag_key(p, s("index").and_then(|i| i.parse().ok()).unwrap_or(0)).map_err(|e| e.to_string())?;
            Ok(json!(crate::faucet::ask(node, &k.address, &s("tag").unwrap_or_default())?))
        }
        "mempool" => {
            let pool = node.chain.mempool(c).map_err(|e| e.to_string())?;
            let txs: Vec<Value> = pool
                .iter()
                .filter_map(|(seen, raw)| arcade_chain::Tx::parse_with(raw, &p.format).ok().map(|t| (seen, t)))
                .map(|(seen, t)| json!({"txid": t.txid.to_hex(), "seen": seen, "size": t.raw.len(), "outputs": t.outputs.iter().map(|o| o.value).sum::<u64>()}))
                .collect();
            Ok(json!({"count": txs.len(), "txs": txs, "pending_tags": crate::claims::pending_tags(node)}))
        }
        "events" => Ok(json!(node.app.recent_events(30).map_err(|e| e.to_string())?)),
        other => Err(format!("unknown command {other:?}")),
    }
}
