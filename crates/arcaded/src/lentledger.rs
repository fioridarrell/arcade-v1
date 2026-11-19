//! Arcade ledger rows a send needs, on a chain only borrowed here (borrowed = installed, batch 6).
//!
//! A send this arcade builds names a token (its name and divisibility for the card), an inscription (its owner, the
//! txid an INSC transfer names) or a prize pool's signed lots. On a chain kept here they come from its own ledger. On
//! a chain borrowed here (not installed, or still catching up, so our copy may lack the newest rows) they come from an
//! arcade keeping that chain's ledger, through the same Explorer reads the explorer forwards (explorer.ledger_token,
//! explorer.ledger_inscription, explorer.ledger_bytes). Our own copy answers when no such arcade does.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use base64::Engine;
use serde_json::{json, Value};

/// Asked of an arcade keeping `p`'s ledger, answered from its own tables.
fn ask(node: &Node, p: &'static ChainParams, cmd: &str, mut q: Value) -> Option<Value> {
    if !crate::borrow::is_borrowed(p.key) {
        return None;
    }
    q["chain"] = json!(p.key);
    q["local"] = json!(true);
    crate::explorer::mesh_ask(node, p, "ledger", cmd, &q).ok().map(|(v, _)| v).filter(|v| v.is_object())
}

/// An arcade token's property record (name, type, total, issuer, ...) on `p`: our own copy's when it has the token
/// (a name and a divisibility never change), else a keeper's.
pub fn property(node: &Node, p: &'static ChainParams, pid: u32) -> Option<Value> {
    arcade_ledger::engine::property(&node.chain, p.chain_no, pid).ok().flatten()
        .or_else(|| ask(node, p, "explorer.ledger_token", json!({"id": pid})).map(property_shape))
}

/// An explorer.ledger_token answer as the property record: without its holder page and the Explorer's extras.
fn property_shape(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        for k in ["top", "holders", "total_shown", "issuer_tag", "creation_time", "_mesh"] {
            o.remove(k);
        }
    }
    v
}

/// An arcade inscription's row (txid, number, owner, creator, ...) on `p`, by txid or number: a keeper's while `p` is
/// borrowed (its owner may have changed since our copy's tip), else our own.
pub fn inscription(node: &Node, p: &'static ChainParams, id: &str) -> Option<Value> {
    let id = id.trim().trim_start_matches('#');
    if let Some(v) = ask(node, p, "explorer.ledger_inscription", json!({"id": id})).map(inscription_shape) {
        return Some(v);
    }
    arcade_ledger::content::inscription(&node.chain, p.chain_no, id)
}

/// An explorer.ledger_inscription answer as the row: without its content preview and the Explorer's extras.
fn inscription_shape(mut v: Value) -> Value {
    if let Some(o) = v.as_object_mut() {
        for k in ["content", "owner_tag", "creator_tag", "time", "_mesh"] {
            o.remove(k);
        }
    }
    v
}

/// An arcade inscription's whole bytes on `p` (checked against the sha256 its answer names).
pub fn bytes(node: &Node, p: &'static ChainParams, id: &str) -> Result<Vec<u8>, String> {
    let id = id.trim().trim_start_matches('#');
    if let Some(v) = ask(node, p, "explorer.ledger_bytes", json!({"id": id})) {
        if let Ok(b) = bytes_of(&v) {
            return Ok(b);
        }
    }
    arcade_ledger::content::content(&node.chain, p, id).map(|c| c.bytes)
}

/// The bytes of an explorer.ledger_bytes answer, when they hash to its sha256.
fn bytes_of(v: &Value) -> Result<Vec<u8>, String> {
    use sha2::{Digest, Sha256};
    let b = base64::engine::general_purpose::STANDARD.decode(v["b64"].as_str().ok_or("no bytes")?).map_err(|e| e.to_string())?;
    if v["sha256"].as_str() != Some(hex::encode(Sha256::digest(&b)).as_str()) {
        return Err("the bytes an arcade sent don't match their hash".into());
    }
    Ok(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rows from another arcade come back in the shape our own ledger gives.
    #[test]
    fn lent_rows_keep_the_ledgers_shape() {
        let p = property_shape(json!({"propertyid": 3, "name": "X", "type": 2, "total": 5, "top": [], "holders": 1, "total_shown": "5", "_mesh": {}}));
        assert_eq!(p, json!({"propertyid": 3, "name": "X", "type": 2, "total": 5}));
        assert!(arcade_ledger::engine::divisible(&p));
        let i = inscription_shape(json!({"txid": "ab", "number": 7, "owner": "P1", "content": {"kind": "image"}, "owner_tag": "x", "time": 1}));
        assert_eq!(i, json!({"txid": "ab", "number": 7, "owner": "P1"}));
    }

    #[test]
    fn lent_bytes_must_match_their_hash() {
        use sha2::{Digest, Sha256};
        let b = b"{\"prizepool\":{}}".to_vec();
        let good = json!({"b64": base64::engine::general_purpose::STANDARD.encode(&b), "sha256": hex::encode(Sha256::digest(&b))});
        assert_eq!(bytes_of(&good).unwrap(), b);
        let bad = json!({"b64": base64::engine::general_purpose::STANDARD.encode(b"other"), "sha256": hex::encode(Sha256::digest(&b))});
        assert!(bytes_of(&bad).is_err());
    }
}
