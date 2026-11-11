//! The chain an app page's reads are on (an app inscribed on a chain other than the home chain).
//!
//! An app frame's address names its chain (`/app/<chain>/<txid>`), and its requests carry that
//! address as their Referer. The HTTP server answers a page's reads on that chain: it sets the
//! chain here for the request's own thread while it runs the read, and the read routes ask
//! `params` instead of the home chain. Writes (the wallet's sends) stay on the home chain.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use std::cell::Cell;

thread_local! {
    static CHAIN: Cell<Option<&'static ChainParams>> = const { Cell::new(None) };
}

/// The chain a page read is on: the one set for this request, else the home chain.
pub fn params(node: &Node) -> &'static ChainParams {
    CHAIN.with(|c| c.get()).unwrap_or(node.params)
}

/// Run `f` with page reads on chain `p`.
pub fn with<R>(p: &'static ChainParams, f: impl FnOnce() -> R) -> R {
    let before = CHAIN.with(|c| c.replace(Some(p)));
    let r = f();
    CHAIN.with(|c| c.set(before));
    r
}

/// The chain an app frame's address names (a Referer like http://127.0.0.1:8420/app/pep/<txid>).
pub fn of_referer(referer: &str) -> Option<&'static ChainParams> {
    let path = referer.split('?').next()?;
    let at = path.find("/app/")?;
    let mut parts = path[at + 5..].split('/');
    let (chain, txid) = (parts.next()?, parts.next()?);
    if txid.len() != 64 || !txid.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    arcade_chain::params::by_key(chain)
}

/// Whether this arcade keeps chain `p`'s arcade ledger itself (else its reads come from the mesh).
pub fn keeps_ledger(node: &Node, p: &ChainParams) -> bool {
    if p.chain_no == node.params.chain_no {
        return true;
    }
    node.extra.lock().unwrap().get(p.key).is_some_and(|r| r.indexes.iter().any(|i| i == "ledger")) && !crate::borrow::is_borrowed(p.key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frames_address_names_its_chain() {
        let t = "a".repeat(64);
        assert_eq!(of_referer(&format!("http://127.0.0.1:8420/app/pep/{t}")).map(|p| p.key), Some("pep"));
        assert_eq!(of_referer(&format!("http://127.0.0.1:8420/app/peptest/{t}?x=1")).map(|p| p.key), Some("peptest"));
        assert!(of_referer("http://127.0.0.1:8420/app/pep/short").is_none());
        assert!(of_referer(&format!("http://127.0.0.1:8420/app/nosuchchain/{t}")).is_none());
        assert!(of_referer("http://127.0.0.1:8420/index.html").is_none());
    }
}

// ---------------- an inscription's bytes on a chain ----------------

use serde_json::{json, Value};
use std::sync::Mutex;

/// What an app page loads, by (chain, txid or number): kept in memory, since inscriptions do
/// not change (up to 64 MB, the oldest let go first).
static KEPT: Mutex<Vec<(String, String, std::sync::Arc<(String, String, Vec<u8>)>)>> = Mutex::new(Vec::new());
const KEPT_MOST: usize = 64 << 20;

/// An arcade inscription's (content type, sha256, bytes) on chain `p`: from this arcade's ledger
/// when it keeps it, else from an arcade on the mesh that does (checked against its sha256).
pub fn content(node: &Node, p: &'static ChainParams, id: &str) -> Result<std::sync::Arc<(String, String, Vec<u8>)>, String> {
    let id = id.trim().trim_start_matches('#').to_lowercase();
    if keeps_ledger(node, p) {
        let c = arcade_ledger::content::content(&node.chain, p, &id)?;
        return Ok(std::sync::Arc::new((c.content_type, c.sha256, c.bytes)));
    }
    if let Some(hit) = KEPT.lock().unwrap().iter().find(|(k, i, _)| k == p.key && *i == id).map(|x| x.2.clone()) {
        return Ok(hit);
    }
    let got = crate::explorer::handle(node, "explorer.ledger_bytes", &json!({"chain": p.key, "id": id}))?;
    let bytes = {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.decode(got["b64"].as_str().unwrap_or("")).map_err(|_| "the arcade that keeps it sent no bytes")?
    };
    let sha = got["sha256"].as_str().unwrap_or("").to_string();
    {
        use sha2::{Digest, Sha256};
        if hex::encode(Sha256::digest(&bytes)) != sha {
            return Err("the bytes another arcade sent do not match their hash".into());
        }
    }
    let item = std::sync::Arc::new((got["content_type"].as_str().unwrap_or("").to_string(), sha, bytes));
    let mut k = KEPT.lock().unwrap();
    // Kept under both names a page may use (its txid and what it asked by).
    let txid = got["txid"].as_str().unwrap_or("").to_lowercase();
    for name in [id, txid].into_iter().filter(|n| !n.is_empty()) {
        k.push((p.key.to_string(), name, item.clone()));
    }
    while k.iter().map(|x| x.2 .2.len()).sum::<usize>() > KEPT_MOST && k.len() > 1 {
        k.remove(0);
    }
    Ok(item)
}

/// A page read (GET /r/...) on chain `p`: this arcade's own when it keeps the chain's ledger,
/// else answered by an arcade on the mesh that does. (status, JSON text).
pub fn page_read(node: &Node, p: &'static ChainParams, path: &str, query: &serde_json::Map<String, Value>) -> (u16, String) {
    if keeps_ledger(node, p) {
        return with(p, || crate::walletsvc::route(node, path, query));
    }
    match crate::explorer::handle(node, "explorer.page", &json!({"chain": p.key, "path": path, "query": query})) {
        Ok(v) => (v["status"].as_u64().unwrap_or(200) as u16, v["body"].as_str().unwrap_or("null").to_string()),
        Err(e) => (503, json!({"error": format!("this app is on {}, which this arcade borrows: {e}", p.name)}).to_string()),
    }
}
