//! Asking the mesh for test coins (welcome protocol) and noticing when they are paid.

use crate::node::Node;
use arcade_app::{FaucetRequest, FaucetStatus};
use serde_json::Value;

/// Queue a signed welcome ask for `addr` (with the tag we want) to every mesh link.
pub fn ask(node: &Node, addr: &str, tag: &str) -> Result<FaucetRequest, String> {
    let rec = arcade_mesh::welcome::ask(&node.identity, addr, tag, arcade_app::now());
    let id = hex::encode(node.identity.verifying_key().to_bytes());
    node.mesh_outbox.lock().unwrap().push(serde_json::json!({"t": "wl", "recs": [rec]}));
    let req = FaucetRequest {
        id: format!("{}:{addr}", node.params.key),
        chain: node.params.chain_no,
        address: addr.into(),
        status: FaucetStatus::Asked,
        paid_by: None,
        txid: None,
        created: arcade_app::now(),
    };
    node.app.put_faucet(&req).map_err(|e| e.to_string())?;
    node.app.event("faucet", &format!("asked the mesh for test coins to {addr} (ask {})", &id[..16])).ok();
    Ok(req)
}

/// A `wl` frame arrived: if someone paid our ask, record it.
pub fn on_welcome(node: &Node, head: &Value) {
    let me = hex::encode(node.identity.verifying_key().to_bytes());
    let Some(recs) = head["recs"].as_array() else { return };
    for r in recs {
        if r["k"] == "sent" && r["ask"] == me.as_str() && arcade_mesh::welcome::verify(r) {
            let txid = r["txid"].as_str().unwrap_or("").to_string();
            let paid_by = r["a"].as_str().unwrap_or("").to_string();
            if let Ok(list) = node.app.faucet_requests() {
                for mut f in list.into_iter().filter(|f| f.status == FaucetStatus::Asked) {
                    f.status = FaucetStatus::Paid;
                    f.txid = Some(txid.clone());
                    f.paid_by = Some(paid_by.clone());
                    let _ = node.app.put_faucet(&f);
                }
            }
            node.app.event("faucet", &format!("test coins sent by {} in {txid}", &paid_by[..16.min(paid_by.len())])).ok();
        }
    }
}
