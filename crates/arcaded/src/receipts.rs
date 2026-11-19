//! Receipts for sends (ARCADE #25): what this arcade signed, kept by txid so `tx.status` can tell "never signed here"
//! from "signed and sent" after a caller timed out, and an optional `client_ref` so a retry returns the first send's
//! answer instead of signing a second transaction.
//!
//! Kept in the app database (META, `rcpt/<chain>/<txid>` and `rcptref/<client_ref>`); a few thousand at most, the
//! oldest let go first. Local bookkeeping only: nothing here is on a chain or changes what a send does.

use crate::node::Node;
use serde_json::{json, Value};

/// Sends that take `client_ref` and leave a receipt.
pub const SENDS: &[&str] = &["ledger.item_open", "ledger.item_comment", "ledger.send", "wallet.send"];

/// Receipts kept; past this the oldest go.
const KEEP: usize = 5000;
/// Longest client_ref taken.
const MAX_REF: usize = 128;
/// A receipt whose transaction is in no pool and no block this long after signing is called dropped.
const DROPPED_AFTER: u64 = 30 * 60;

fn key_rcpt(chain: &str, txid: &str) -> Vec<u8> {
    format!("rcpt/{chain}/{txid}").into_bytes()
}

fn key_ref(r: &str) -> Vec<u8> {
    format!("rcptref/{r}").into_bytes()
}

fn get(node: &Node, k: &[u8]) -> Option<Value> {
    node.app.store.read(|r| r.get(arcade_app::META, k)).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok())
}

/// The receipt for `txid` on `chain`, if this arcade signed it.
pub fn receipt(node: &Node, chain: &str, txid: &str) -> Option<Value> {
    get(node, &key_rcpt(chain, &txid.to_ascii_lowercase()))
}

/// The receipt a client_ref names.
pub fn by_ref(node: &Node, r: &str) -> Option<Value> {
    let at = get(node, &key_ref(r))?;
    receipt(node, at["chain"].as_str()?, at["txid"].as_str()?)
}

/// The client_ref of a request, checked: None when it has none.
pub fn client_ref(req: &Value) -> Result<Option<String>, String> {
    match &req["client_ref"] {
        Value::Null => Ok(None),
        Value::String(s) if !s.is_empty() && s.len() <= MAX_REF && s.bytes().all(|b| b.is_ascii_graphic()) => Ok(Some(s.clone())),
        _ => Err(format!("client_ref: 1 to {MAX_REF} printable characters, no spaces")),
    }
}

/// Requests with a client_ref still running: a second one with the same key waits its turn, then finds its receipt.
fn running() -> std::sync::MutexGuard<'static, std::collections::BTreeSet<String>> {
    static R: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeSet<String>>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default).lock().unwrap()
}

/// The txids a send's answer names.
fn txids_of(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if let Some(t) = v["txid"].as_str() {
        out.push(t.to_string());
    }
    for t in v["txids"].as_array().into_iter().flatten().filter_map(|t| t.as_str()) {
        if !out.iter().any(|x| x == t) {
            out.push(t.to_string());
        }
    }
    out
}

/// The chain a send went out on.
fn chain_of(node: &Node, cmd: &str, req: &Value, v: &Value) -> String {
    if let Some(c) = v["chain"].as_str() {
        return c.to_string();
    }
    if cmd.starts_with("ledger.") {
        return crate::arcsvc::usable_chain().map(|p| p.key).unwrap_or(node.params.key).to_string();
    }
    req["chain"].as_str().and_then(crate::chains::find).map(|p| p.key).unwrap_or(node.params.key).to_string()
}

/// Whether this request signs something (wallet.send only with send: true; otherwise it is a quote).
fn signs(cmd: &str, req: &Value) -> bool {
    SENDS.contains(&cmd) && (cmd != "wallet.send" || req["send"] == true)
}

/// Run send `f` for `req` (cmd in SENDS): with a client_ref seen before, its first answer comes back (with
/// "replayed": true) and nothing is signed again; otherwise the answer is kept as a receipt per txid.
pub fn run(node: &Node, cmd: &str, req: &Value, f: impl FnOnce() -> Result<Value, String>) -> Result<Value, String> {
    if !signs(cmd, req) {
        return f();
    }
    let r = client_ref(req)?;
    if let Some(r) = &r {
        // One at a time per key: a retry sent while the first is still signing waits for it.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            if running().insert(r.clone()) {
                break;
            }
            if std::time::Instant::now() > until {
                return Err(format!("a send with client_ref {r} is still running"));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if let Some(old) = by_ref(node, r) {
            running().remove(r);
            let mut v = old["answer"].clone();
            if v.is_object() {
                v["replayed"] = json!(true);
                v["receipt"] = summary(&old);
            }
            return Ok(v);
        }
    }
    let out = f();
    if let Ok(v) = &out {
        keep(node, cmd, req, v, r.as_deref());
    }
    if let Some(r) = &r {
        running().remove(r);
    }
    out.map(|mut v| {
        if v.is_object() && !txids_of(&v).is_empty() {
            v["receipt"] = json!({"chain": chain_of(node, cmd, req, &v), "txids": txids_of(&v), "client_ref": r, "state": "signed",
                                  "status": "tx.status chain txid (or client_ref) follows it; tx.wait waits for a state"});
        }
        v
    })
}

fn summary(rec: &Value) -> Value {
    json!({"chain": rec["chain"], "txids": rec["txids"], "client_ref": rec["client_ref"], "signed_at": rec["at"], "cmd": rec["cmd"]})
}

fn keep(node: &Node, cmd: &str, req: &Value, v: &Value, r: Option<&str>) {
    let txids = txids_of(v);
    if txids.is_empty() {
        return;
    }
    let chain = chain_of(node, cmd, req, v);
    let rec = json!({"chain": chain, "txids": txids, "cmd": cmd, "client_ref": r, "at": arcade_app::now(), "answer": v});
    let bytes = serde_json::to_vec(&rec).unwrap_or_default();
    let _ = node.app.store.write(|w| {
        for t in &txids {
            w.put(arcade_app::META, &key_rcpt(&chain, &t.to_ascii_lowercase()), &bytes)?;
        }
        if let Some(r) = r {
            w.put(arcade_app::META, &key_ref(r), json!({"chain": chain, "txid": txids[0].to_ascii_lowercase()}).to_string().as_bytes())?;
        }
        Ok(())
    });
    prune(node);
}

/// Keep at most KEEP receipts: the oldest go (with their client_ref).
fn prune(node: &Node) {
    let rows = node.app.store.read(|r| r.scan_prefix(arcade_app::META, b"rcpt/")).unwrap_or_default();
    if rows.len() <= KEEP {
        return;
    }
    let mut by_age: Vec<(u64, Vec<u8>, Option<String>)> = rows
        .into_iter()
        .map(|(k, v)| {
            let rec: Value = serde_json::from_slice(&v).unwrap_or(Value::Null);
            (rec["at"].as_u64().unwrap_or(0), k, rec["client_ref"].as_str().map(String::from))
        })
        .collect();
    by_age.sort_by_key(|x| x.0);
    let n = by_age.len() - KEEP;
    let _ = node.app.store.write(|w| {
        for (_, k, r) in by_age.iter().take(n) {
            w.del(arcade_app::META, k)?;
            if let Some(r) = r {
                w.del(arcade_app::META, &key_ref(r))?;
            }
        }
        Ok(())
    });
}

/// `tx.status {chain, txid}` or `{client_ref}`: where a transaction stands, as far as this arcade can tell.
/// state: confirmed {height, block, confirmations}; accepted_local (in this arcade's pool) with what it knows of the
/// broadcast (ARCADE chain: peers_sent, peers_holding); signed (signed here moments ago, in no pool or block yet);
/// dropped (signed here, but in no pool or block DROPPED_AFTER later); unknown (with ever_signed_here).
pub fn status(node: &Node, req: &Value) -> Result<Value, String> {
    let (chain, txid) = match (req["chain"].as_str(), req["txid"].as_str(), req["client_ref"].as_str()) {
        (c, Some(t), _) => {
            let p = match c {
                Some(k) => crate::chains::find(k).ok_or_else(|| format!("unknown chain {k}"))?,
                None => return Err("chain required (a chain key such as arctest): a txid alone could be on any chain".into()),
            };
            (p.key.to_string(), t.trim().to_ascii_lowercase())
        }
        (_, None, Some(r)) => {
            let rec = by_ref(node, r).ok_or_else(|| format!("not_found: no send with client_ref {r} was signed here"))?;
            (rec["chain"].as_str().unwrap_or("").to_string(), rec["txids"][0].as_str().unwrap_or("").to_string())
        }
        _ => return Err("txid and chain required (or client_ref)".into()),
    };
    let id = arcade_chain::Hash256::from_hex(&txid).map_err(|_| format!("{txid} is not a txid"))?;
    let rec = receipt(node, &chain, &txid);
    let mut out = json!({"chain": chain, "txid": txid, "ever_signed_here": rec.is_some()});
    if let Some(r) = &rec {
        out["signed_at"] = r["at"].clone();
        out["client_ref"] = r["client_ref"].clone();
        out["cmd"] = r["cmd"].clone();
    }
    let p = crate::chains::find(&chain).ok_or("unknown chain")?;
    let relay = crate::arcsvc::runs(p).then(|| crate::arcsvc::relay_status(&id)).flatten();
    // In a block or a pool, as this arcade's Explorer finds it (its own copy or the borrowed one; not the mesh).
    let found = crate::explorer::handle(node, "explorer.tx", &json!({"chain": chain, "txid": txid, "local": true}));
    match found {
        Ok(v) if !v["height"].is_null() => {
            out["state"] = json!("confirmed");
            out["confirmed"] = json!({"height": v["height"], "block": v["block"], "confirmations": v["confirmations"]});
            out["as_of"] = v["as_of"].clone();
        }
        Ok(v) => {
            out["state"] = json!("accepted_local");
            out["seen"] = v["seen"].clone();
            if let Some(r) = relay {
                out["broadcast"] = r;
            } else {
                out["broadcast"] = json!({"note": "pooled here and handed to the chain's network; this arcade can't tell which peers took it"});
            }
        }
        Err(_) => {
            let age = rec.as_ref().and_then(|r| r["at"].as_u64()).map(|t| arcade_app::now().saturating_sub(t));
            out["state"] = json!(match age {
                None => "unknown",
                Some(a) if a < DROPPED_AFTER => "signed",
                Some(_) => "dropped",
            });
            if rec.is_none() {
                out["code"] = json!("not_found");
            }
        }
    }
    Ok(out)
}

/// The order states come in, for `tx.wait until`: a later one satisfies an earlier one.
fn rank(state: &str) -> u8 {
    match state {
        "signed" => 1,
        "accepted_local" => 2,
        "broadcast_ok" => 3,
        "confirmed" => 4,
        _ => 0,
    }
}

/// Whether `st` (a tx.status answer) has reached `until`.
fn reached(st: &Value, until: &str) -> bool {
    let s = st["state"].as_str().unwrap_or("");
    match until {
        // Offered to at least one linked arcade (ARCADE chain), or held by one.
        "broadcast_ok" => s == "confirmed" || st["broadcast"]["peers_sent"].as_u64().unwrap_or(0) > 0 || st["broadcast"]["peers_holding"].as_u64().unwrap_or(0) > 0,
        "seen_in_peer_mempool" => s == "confirmed" || st["broadcast"]["peers_holding"].as_u64().unwrap_or(0) > 0,
        u => rank(s) >= rank(u) && rank(u) > 0 || s == "dropped",
    }
}

/// `tx.wait {chain, txid | client_ref, until?, timeout?}`: tx.status, answered once the transaction reaches `until`
/// (accepted_local, broadcast_ok, seen_in_peer_mempool or confirmed; default confirmed) or is dropped, or after
/// `timeout` seconds (default 60, at most 600) with "timed_out": true.
pub fn wait(node: &Node, req: &Value) -> Result<Value, String> {
    let until = req["until"].as_str().unwrap_or("confirmed");
    if !matches!(until, "accepted_local" | "broadcast_ok" | "seen_in_peer_mempool" | "confirmed") {
        return Err(format!("until: accepted_local, broadcast_ok, seen_in_peer_mempool or confirmed, not {until}"));
    }
    let timeout = req["timeout"].as_u64().or_else(|| req["timeout"].as_str().and_then(|t| t.parse().ok())).unwrap_or(60).min(600);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    loop {
        let mut st = status(node, req)?;
        if reached(&st, until) {
            st["timed_out"] = json!(false);
            return Ok(st);
        }
        if std::time::Instant::now() >= end || crate::STOP_ASKED.load(std::sync::atomic::Ordering::Relaxed) {
            st["timed_out"] = json!(true);
            return Ok(st);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_are_reached_in_order() {
        let st = |s: &str| json!({"state": s});
        assert!(reached(&st("confirmed"), "accepted_local") && reached(&st("confirmed"), "confirmed"));
        assert!(!reached(&st("accepted_local"), "confirmed") && reached(&st("accepted_local"), "accepted_local"));
        assert!(reached(&st("dropped"), "confirmed"), "a dropped transaction ends the wait");
        assert!(!reached(&st("unknown"), "accepted_local"));
        let sent = json!({"state": "accepted_local", "broadcast": {"peers_sent": 2, "peers_holding": 0}});
        assert!(reached(&sent, "broadcast_ok") && !reached(&sent, "seen_in_peer_mempool"));
        let held = json!({"state": "accepted_local", "broadcast": {"peers_sent": 2, "peers_holding": 1}});
        assert!(reached(&held, "seen_in_peer_mempool"));
    }

    #[test]
    fn client_refs_are_checked() {
        assert_eq!(client_ref(&json!({})).unwrap(), None);
        assert_eq!(client_ref(&json!({"client_ref": "post-25-a"})).unwrap().as_deref(), Some("post-25-a"));
        assert!(client_ref(&json!({"client_ref": "has space"})).is_err());
        assert!(client_ref(&json!({"client_ref": ""})).is_err());
        assert!(client_ref(&json!({"client_ref": 5})).is_err());
        assert!(signs("wallet.send", &json!({"send": true})) && !signs("wallet.send", &json!({})), "a quote signs nothing");
        assert_eq!(txids_of(&json!({"txid": "a", "txids": ["a", "b"]})), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_retry_with_the_same_client_ref_returns_the_first_receipt() {
        let (_g, t) = crate::http::tests::node();
        let n = std::cell::Cell::new(0);
        let send = || {
            n.set(n.get() + 1);
            Ok(json!({"txid": format!("{:064x}", n.get()), "fee": 100}))
        };
        let req = json!({"cmd": "ledger.send", "hex": "00", "client_ref": "retry-test-1"});
        let a = run(&t.node, "ledger.send", &req, send).unwrap();
        let b = run(&t.node, "ledger.send", &req, send).unwrap();
        assert_eq!(n.get(), 1, "signed once");
        assert_eq!(a["txid"], b["txid"]);
        assert_eq!(b["replayed"], true);
        let other = run(&t.node, "ledger.send", &json!({"cmd": "ledger.send", "hex": "00", "client_ref": "retry-test-2"}), send).unwrap();
        assert_ne!(other["txid"], a["txid"], "another key signs again");
        let chain = a["receipt"]["chain"].as_str().unwrap().to_string();
        assert!(receipt(&t.node, &chain, a["txid"].as_str().unwrap()).is_some(), "kept by txid");
        assert_eq!(by_ref(&t.node, "retry-test-1").unwrap()["txids"][0], a["txid"]);
    }
}
