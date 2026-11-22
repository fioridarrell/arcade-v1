//! Receipts for sends (ARCADE #25): what this arcade signed, kept by txid so `tx.status` can tell "never signed here"
//! from "signed and sent" after a caller timed out, and an optional `client_ref` so a retry returns the first send's
//! answer instead of signing a second transaction.
//!
//! Kept in the app database (META, `rcpt/<chain>/<txid>` and `rcptref/<client_ref>`); a few thousand at most, the
//! oldest let go first. Local bookkeeping only: nothing here is on a chain or changes what a send does.

use crate::node::Node;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

/// Whether `cmd` is a send: it takes client_ref and leaves a receipt (crate::commands::is_send: one table).
pub fn is_send(cmd: &str) -> bool {
    crate::commands::find(cmd).is_some_and(crate::commands::is_send)
}

/// Sends that resume themselves under one client_ref (crate::apppub's journal) rather than replay their first answer.
const RESUMES: &[&str] = &["apps.publish", "apps.withdraw", "apps.transfer"];

/// Receipts kept; past this the oldest go.
const KEEP: usize = 5000;
/// Longest client_ref taken.
const MAX_REF: usize = 128;
/// A receipt whose transaction is in no pool and no block this long after signing is called dropped.
const DROPPED_AFTER: u64 = 30 * 60;
/// Broadcast outcomes kept in memory (txids); past this they are let go.
const OUTCOMES: usize = 4096;

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
        _ => Err(format!("bad request: client_ref must be 1 to {MAX_REF} printable characters, no spaces")),
    }
}

/// Requests with a client_ref still running: a second one with the same key waits its turn, then finds its receipt.
fn running() -> std::sync::MutexGuard<'static, std::collections::BTreeSet<String>> {
    static R: std::sync::OnceLock<Mutex<std::collections::BTreeSet<String>>> = std::sync::OnceLock::new();
    R.get_or_init(Default::default).lock().unwrap()
}

/// The send this thread is running (set by [`run`]): its command, client_ref and the transactions signed so far, as
/// (chain, txid). A signing place calls [`signed`] right after it signs, before any network step.
struct Ctx {
    cmd: String,
    client_ref: Option<String>,
    at: u64,
    signed: Vec<(String, String)>,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

/// A transaction of this arcade's own was just signed on chain `chain` (a key) and is about to go out: while a send
/// command runs on this thread, its receipt is written now, so `tx.status {client_ref | txid}` answers `signed` even
/// when the caller's connection drops or the broadcast hangs. Outside a send (a background resend), nothing.
pub fn signed(node: &Node, chain: &str, txid: &arcade_chain::Hash256) {
    let txid = txid.to_hex();
    let rec = CTX.with(|c| {
        let mut c = c.borrow_mut();
        let ctx = c.as_mut()?;
        if ctx.signed.iter().any(|(ch, t)| ch == chain && *t == txid) {
            return None;
        }
        ctx.signed.push((chain.to_string(), txid.clone()));
        let txids: Vec<&String> = ctx.signed.iter().filter(|(ch, _)| ch == chain).map(|(_, t)| t).collect();
        Some((json!({"chain": chain, "txids": txids, "cmd": ctx.cmd, "client_ref": ctx.client_ref, "at": ctx.at, "signed_at": arcade_app::now(),
                     "state": "signed", "answer": null}), ctx.client_ref.clone(), ctx.signed.len() == 1))
    });
    let Some((rec, r, first)) = rec else { return };
    let bytes = serde_json::to_vec(&rec).unwrap_or_default();
    let _ = node.app.store.write(|w| {
        // Earlier transactions of the same send carry the whole list too.
        for t in rec["txids"].as_array().into_iter().flatten().filter_map(|t| t.as_str()) {
            w.put(arcade_app::META, &key_rcpt(chain, t), &bytes)?;
        }
        if let (Some(r), true) = (&r, first) {
            w.put(arcade_app::META, &key_ref(r), json!({"chain": chain, "txid": txid}).to_string().as_bytes())?;
        }
        Ok(())
    });
}

// ---------------- broadcast outcomes per peer ----------------

/// What each peer said of a transaction: txid (hex) -> peer -> (ok, code, error, at).
type Said = HashMap<String, BTreeMap<String, (bool, String, String, u64)>>;

fn outcomes() -> std::sync::MutexGuard<'static, Said> {
    static O: std::sync::OnceLock<Mutex<Said>> = std::sync::OnceLock::new();
    O.get_or_init(Default::default).lock().unwrap()
}

/// A peer's answer to a transaction offered to it (ARCADE pool gossip's "txr", a lender's sendrawtransaction, this
/// arcade's own pool as peer "local"): taken (ok) or refused with a code ([`refusal_code`]) and its words.
pub fn outcome(txid: &str, peer: &str, ok: bool, error: &str) {
    let mut o = outcomes();
    if o.len() >= OUTCOMES && !o.contains_key(txid) {
        o.clear();
    }
    let code = if ok { String::new() } else { refusal_code(error).to_string() };
    o.entry(txid.to_ascii_lowercase()).or_default().insert(peer.to_string(), (ok, code, error.chars().take(300).collect(), arcade_app::now()));
}

/// The outcomes heard for a transaction: (peers that took it, refusals [{peer, code, error}]).
pub fn outcomes_of(txid: &str) -> (Vec<String>, Vec<Value>) {
    let o = outcomes();
    let Some(m) = o.get(&txid.to_ascii_lowercase()) else { return (Vec::new(), Vec::new()) };
    let acked = m.iter().filter(|(_, v)| v.0).map(|(p, _)| p.clone()).collect();
    let refused = m.iter().filter(|(_, v)| !v.0).map(|(p, v)| json!({"peer": p, "code": v.1, "error": v.2, "at": v.3})).collect();
    (acked, refused)
}

/// Every refusal code tx.status may show.
pub const REFUSALS: &[&str] = &[
    "fee_too_low", "conflict", "missing_coin", "dust", "not_yet_valid", "blob_missing", "pool_full", "rules", "invalid",
    "refused_recently", "unreachable",
];

/// A stable code for why a peer (or this arcade's pool, or a coin node) refused a transaction.
pub fn refusal_code(e: &str) -> &'static str {
    // A peer's own code first ("txr" frames come as "code: words").
    if let Some(c) = e.split(':').next().and_then(|c| REFUSALS.iter().find(|r| **r == c.trim())) {
        return c;
    }
    let l = e.to_ascii_lowercase();
    if l.contains("fee") && (l.contains("low") || l.contains("under") || l.contains("not met") || l.contains("insufficient") || l.contains("doesn't cover")) {
        "fee_too_low"
    } else if l.contains("conflict") || l.contains("already spends") || l.contains("txn-mempool-conflict") || l.contains("same coin") {
        "conflict"
    } else if l.contains("missingorspent") || l.contains("missing-inputs") || l.contains("doesn't exist or is spent") || l.contains("missing_coin") || l.contains("missing inputs") {
        "missing_coin"
    } else if l.contains("dust") {
        "dust"
    } else if l.contains("non-final") || l.contains("lock height") || l.contains("premature") || l.contains("before it is 100 blocks") || l.contains("immature") {
        "not_yet_valid"
    } else if l.contains("blob") && (l.contains("isn't here") || l.contains("missing")) {
        "blob_missing"
    } else if l.contains("pool is full") || l.contains("mempool full") {
        "pool_full"
    } else if l.contains("rules") || l.contains("version") || l.contains("not allowed") {
        "rules"
    } else if l.contains("timed out") || l.contains("connect") || l.contains("unreachable") || l.contains("no lender") && l.contains("[]") {
        "unreachable"
    } else {
        "invalid"
    }
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
    if let Some(c) = v["chain"].as_str().and_then(crate::chains::find) {
        return c.key.to_string();
    }
    if cmd.starts_with("ledger.") || cmd.starts_with("a.") || cmd.starts_with("arc.") {
        return crate::arcsvc::usable_chain().map(|p| p.key).unwrap_or(node.params.key).to_string();
    }
    req["chain"].as_str().and_then(crate::chains::find).map(|p| p.key).unwrap_or(node.params.key).to_string()
}

/// Whether this request signs something (a command with `send` only with send: true; otherwise it is a quote).
fn signs(cmd: &str, req: &Value) -> bool {
    crate::commands::find(cmd).is_some_and(|c| crate::commands::is_send(c) && (!crate::commands::signs_only_with_send(c) || req["send"] == true))
}

/// The answer a client_ref seen before gets: the first send's answer (replayed), what it signed while it still runs
/// (in_progress), or the error it ended in after signing (client_ref_used).
fn replay(old: &Value, r: &str, running: bool) -> Result<Value, String> {
    let txids = old["txids"].clone();
    let first = txids[0].as_str().unwrap_or("").to_string();
    if let Some(e) = old["error"].as_str() {
        return Err(format!("client_ref {r} already signed {first} (tx.status client_ref={r} tells where it stands), and that send answered: {e}. Use a new client_ref to send again."));
    }
    let mut v = old["answer"].clone();
    if !v.is_object() {
        v = json!({"txid": first, "txids": txids, "chain": old["chain"]});
    }
    v["replayed"] = json!(true);
    if running || old["answer"].is_null() {
        v["in_progress"] = json!(true);
    }
    v["receipt"] = summary(old);
    Ok(v)
}

/// Run send `f` for `req`: with a client_ref seen before, its first answer comes back (with "replayed": true) and
/// nothing is signed again; otherwise every transaction it signs gets a receipt at once ([`signed`]), and the answer
/// is added to it at the end.
pub fn run(node: &Node, cmd: &str, req: &Value, f: impl FnOnce() -> Result<Value, String>) -> Result<Value, String> {
    if !signs(cmd, req) {
        return f();
    }
    let r = client_ref(req)?;
    // A send that finishes itself when asked again with the same client_ref (its own journal, never paying twice) is run
    // again rather than answered from its receipt (2026-11-21: apps.publish, whose pieces may wait for a block).
    let resumes = RESUMES.contains(&cmd);
    if let Some(r) = &r {
        // One at a time per key: a retry sent while the first still runs answers what it has signed so far (or,
        // before it signed anything, waits for it).
        let until = std::time::Instant::now() + std::time::Duration::from_secs(120);
        loop {
            if running().insert(r.clone()) {
                break;
            }
            if let Some(old) = by_ref(node, r).filter(|_| !resumes) {
                return replay(&old, r, true);
            }
            if std::time::Instant::now() > until {
                return Err(format!("a send with client_ref {r} is still running"));
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if let Some(old) = by_ref(node, r).filter(|_| !resumes) {
            running().remove(r);
            return replay(&old, r, false);
        }
    }
    let before = CTX.with(|c| c.replace(Some(Ctx { cmd: cmd.to_string(), client_ref: r.clone(), at: arcade_app::now(), signed: Vec::new() })));
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
    let ctx = CTX.with(|c| c.replace(before));
    let signed = ctx.map(|c| c.signed).unwrap_or_default();
    let out = match out {
        Ok(o) => o,
        Err(p) => {
            if let Some(r) = &r {
                running().remove(r);
            }
            std::panic::resume_unwind(p)
        }
    };
    match &out {
        Ok(v) => keep(node, cmd, req, v, r.as_deref(), &signed),
        Err(e) if !signed.is_empty() => failed(node, cmd, e, r.as_deref(), &signed),
        Err(_) => {}
    }
    if let Some(r) = &r {
        running().remove(r);
    }
    out.map(|mut v| {
        let mut txids: Vec<String> = signed.iter().map(|(_, t)| t.clone()).collect();
        for t in txids_of(&v) {
            if !txids.contains(&t) {
                txids.push(t);
            }
        }
        if v.is_object() && !txids.is_empty() {
            let chain = signed.first().map(|(c, _)| c.clone()).unwrap_or_else(|| chain_of(node, cmd, req, &v));
            v["receipt"] = json!({"chain": chain, "txids": txids, "client_ref": r, "state": "signed",
                                  "status": "tx.status chain txid (or client_ref) follows it; tx.wait waits for a state"});
            if v["txid"].is_null() {
                v["txid"] = json!(txids.last());
            }
        }
        v
    })
}

fn summary(rec: &Value) -> Value {
    json!({"chain": rec["chain"], "txids": rec["txids"], "client_ref": rec["client_ref"], "signed_at": rec["signed_at"].as_u64().or(rec["at"].as_u64()), "cmd": rec["cmd"]})
}

fn write(node: &Node, chain: &str, txids: &[String], rec: &Value, r: Option<&str>) {
    let bytes = serde_json::to_vec(rec).unwrap_or_default();
    let _ = node.app.store.write(|w| {
        for t in txids {
            w.put(arcade_app::META, &key_rcpt(chain, &t.to_ascii_lowercase()), &bytes)?;
        }
        if let (Some(r), Some(first)) = (r, txids.first()) {
            w.put(arcade_app::META, &key_ref(r), json!({"chain": chain, "txid": first.to_ascii_lowercase()}).to_string().as_bytes())?;
        }
        Ok(())
    });
    prune(node);
}

fn keep(node: &Node, cmd: &str, req: &Value, v: &Value, r: Option<&str>, signed: &[(String, String)]) {
    let mut txids: Vec<String> = signed.iter().map(|(_, t)| t.clone()).collect();
    for t in txids_of(v) {
        if !txids.contains(&t) {
            txids.push(t);
        }
    }
    if txids.is_empty() {
        return;
    }
    let chain = signed.first().map(|(c, _)| c.clone()).unwrap_or_else(|| chain_of(node, cmd, req, v));
    let signed_at = signed.first().and_then(|(c, t)| receipt(node, c, t)).and_then(|x| x["signed_at"].as_u64()).unwrap_or_else(arcade_app::now);
    let rec = json!({"chain": chain, "txids": txids, "cmd": cmd, "client_ref": r, "at": arcade_app::now(), "signed_at": signed_at, "answer": v});
    write(node, &chain, &txids, &rec, r);
}

/// A send that signed and then failed (refused, or the broadcast failed): its receipt keeps the error.
fn failed(node: &Node, cmd: &str, e: &str, r: Option<&str>, signed: &[(String, String)]) {
    let chain = signed[0].0.clone();
    let txids: Vec<String> = signed.iter().filter(|(c, _)| *c == chain).map(|(_, t)| t.clone()).collect();
    let signed_at = receipt(node, &chain, &txids[0]).and_then(|x| x["signed_at"].as_u64()).unwrap_or_else(arcade_app::now);
    let rec = json!({"chain": chain, "txids": txids, "cmd": cmd, "client_ref": r, "at": arcade_app::now(), "signed_at": signed_at, "answer": null,
                     "error": e, "error_code": crate::api::error_code(e), "refusal": refusal_code(e)});
    write(node, &chain, &txids, &rec, r);
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
/// broadcast (ARCADE chain: peers_sent, peers_holding, peers_acked, peers_refused); broadcast_failed (refused here, or
/// by every peer that answered and none holds it: broadcast.refusals); signed (signed here, in no pool or block yet);
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
            // The send's last transaction: the one that carries what it did (a record after its text's blob).
            let last = rec["txids"].as_array().and_then(|a| a.last()).and_then(|t| t.as_str()).unwrap_or("").to_string();
            (rec["chain"].as_str().unwrap_or("").to_string(), last)
        }
        _ => return Err("txid and chain required (or client_ref)".into()),
    };
    let id = arcade_chain::Hash256::from_hex(&txid).map_err(|_| format!("{txid} is not a txid"))?;
    let rec = receipt(node, &chain, &txid);
    let mut out = json!({"chain": chain, "txid": txid, "ever_signed_here": rec.is_some()});
    if let Some(r) = &rec {
        out["signed_at"] = json!(r["signed_at"].as_u64().or(r["at"].as_u64()));
        out["client_ref"] = r["client_ref"].clone();
        out["cmd"] = r["cmd"].clone();
        out["txids"] = r["txids"].clone();
        if r["error"].is_string() {
            out["error"] = r["error"].clone();
            out["error_code"] = r["error_code"].clone();
        }
    }
    let p = crate::chains::find(&chain).ok_or("unknown chain")?;
    let relay = crate::arcsvc::runs(p).then(|| crate::arcsvc::relay_status(&id)).flatten();
    let (acked, refused) = outcomes_of(&txid);
    let broadcast = |relay: Option<Value>| -> Value {
        let mut b = relay.unwrap_or_else(|| json!({"note": "pooled here and handed to the chain's network; this arcade can tell only what the peers it offered it to answered"}));
        let holding = b["peers_holding"].as_u64().unwrap_or(0) as usize;
        b["peers_acked"] = json!(acked.iter().filter(|p| *p != "local").count().max(holding));
        b["acked_by"] = json!(acked);
        b["peers_refused"] = json!(refused.iter().filter(|r| r["peer"] != "local").count());
        b["refusals"] = json!(refused);
        b
    };
    // Hard refusals (not "a coin it spends hasn't come yet").
    let hard: Vec<&Value> = refused.iter().filter(|r| !matches!(r["code"].as_str(), Some("missing_coin") | Some("unreachable"))).collect();
    // In a block or a pool, as this arcade's Explorer finds it (its own copy or the borrowed one; not the mesh).
    let found = crate::explorer::handle(node, "explorer.tx", &json!({"chain": chain, "txid": txid, "local": true}));
    match found {
        Ok(v) if !v["height"].is_null() => {
            out["state"] = json!("confirmed");
            out["confirmed"] = json!({"height": v["height"], "block": v["block"], "confirmations": v["confirmations"]});
            out["as_of"] = v["as_of"].clone();
        }
        Ok(v) => {
            out["seen"] = v["seen"].clone();
            let b = broadcast(relay);
            // Every peer that answered refused it and none holds it: it goes nowhere from here.
            let linked = b["peers_linked"].as_u64().unwrap_or(0) as usize;
            let failed = b["peers_acked"] == 0 && !hard.is_empty() && hard.len() >= linked.max(1) && b["peers_holding"].as_u64().unwrap_or(0) == 0;
            out["state"] = json!(if failed { "broadcast_failed" } else { "accepted_local" });
            out["broadcast"] = b;
        }
        Err(_) => {
            let age = rec.as_ref().and_then(|r| r["signed_at"].as_u64().or(r["at"].as_u64())).map(|t| arcade_app::now().saturating_sub(t));
            let failed_here = rec.as_ref().is_some_and(|r| r["error"].is_string()) || !hard.is_empty();
            out["state"] = json!(match age {
                None => "unknown",
                Some(_) if failed_here => "broadcast_failed",
                Some(a) if a < DROPPED_AFTER => "signed",
                Some(_) => "dropped",
            });
            if out["state"] == "broadcast_failed" {
                let mut b = broadcast(relay);
                if hard.is_empty() {
                    if let Some(r) = &rec {
                        let e = r["error"].as_str().unwrap_or("");
                        b["refusals"] = json!([{"peer": "local", "code": refusal_code(e), "error": e}]);
                    }
                }
                out["broadcast"] = b;
            }
            if rec.is_none() {
                out["code"] = json!("not_found");
            }
        }
    }
    with_milestones(&mut out);
    Ok(out)
}

/// The lifecycle in order, each with the field that proves it (ARCADE #13, 2026-11-21: `tx.wait` took milestones
/// `tx.status` never showed, so a client couldn't tell where it was): `milestones` [{name, reached, by}] and
/// `milestone`, the furthest one reached. `state` stays as it was.
fn with_milestones(out: &mut Value) {
    let order: [(&str, &str); 5] = [
        ("signed", "state signed or later"),
        ("accepted_local", "this arcade's pool holds it (state accepted_local or later)"),
        ("broadcast_ok", "broadcast.peers_sent >= 1 (offered to a linked arcade)"),
        ("seen_in_peer_mempool", "broadcast.peers_holding >= 1 or broadcast.peers_acked >= 1"),
        ("confirmed", "state confirmed (confirmed.height)"),
    ];
    let s = out["state"].as_str().unwrap_or("").to_string();
    let ok = |name: &str| -> bool {
        match name {
            "signed" => matches!(s.as_str(), "signed" | "accepted_local" | "confirmed"),
            "accepted_local" => matches!(s.as_str(), "accepted_local" | "confirmed"),
            "broadcast_ok" | "seen_in_peer_mempool" => s == "confirmed" || milestone_reached(out, name),
            "confirmed" => s == "confirmed",
            _ => false,
        }
    };
    let list: Vec<Value> = order.iter().map(|(n, by)| json!({"name": n, "reached": ok(n), "by": by})).collect();
    let furthest = order.iter().rev().find(|(n, _)| ok(n)).map(|(n, _)| *n);
    out["milestones"] = json!(list);
    out["milestone"] = json!(furthest);
}

fn milestone_reached(st: &Value, name: &str) -> bool {
    let b = &st["broadcast"];
    let n = |k: &str| b[k].as_u64().unwrap_or(0);
    match name {
        "broadcast_ok" => n("peers_sent") > 0 || n("peers_holding") > 0 || n("peers_acked") > 0,
        "seen_in_peer_mempool" => n("peers_holding") > 0 || n("peers_acked") > 0,
        _ => false,
    }
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
        "broadcast_ok" => {
            matches!(s, "confirmed" | "broadcast_failed" | "dropped")
                || st["broadcast"]["peers_sent"].as_u64().unwrap_or(0) > 0
                || st["broadcast"]["peers_holding"].as_u64().unwrap_or(0) > 0
                || st["broadcast"]["peers_acked"].as_u64().unwrap_or(0) > 0
        }
        "seen_in_peer_mempool" => {
            matches!(s, "confirmed" | "broadcast_failed" | "dropped")
                || st["broadcast"]["peers_holding"].as_u64().unwrap_or(0) > 0
                || st["broadcast"]["peers_acked"].as_u64().unwrap_or(0) > 0
        }
        u => rank(s) >= rank(u) && rank(u) > 0 || s == "dropped" || s == "broadcast_failed",
    }
}

/// The longest one tx.wait waits, in seconds.
pub const WAIT_MOST: u64 = 300;

/// `tx.wait {chain, txid | client_ref, until?, timeout?}`: tx.status, answered once the transaction reaches `until`
/// (accepted_local, broadcast_ok, seen_in_peer_mempool or confirmed; default confirmed) or is dropped, or after
/// `timeout` seconds (default 60, at most WAIT_MOST) with "timed_out": true, `reached` (the milestone itself, not a failure first) and `ended` (until, broadcast_failed, dropped or timeout); every answer carries `milestones` with "timed_out": true.
pub fn wait(node: &Node, req: &Value) -> Result<Value, String> {
    let until = req["until"].as_str().unwrap_or("confirmed");
    if !matches!(until, "accepted_local" | "broadcast_ok" | "seen_in_peer_mempool" | "confirmed") {
        return Err(format!("until: accepted_local, broadcast_ok, seen_in_peer_mempool or confirmed, not {until}"));
    }
    // At most WAIT_MOST: keep `timeout` under your client's socket timeout (ARCADE #13: a wait of 540 s outlasted one).
    let timeout = req["timeout"].as_u64().or_else(|| req["timeout"].as_str().and_then(|t| t.parse().ok())).unwrap_or(60).min(WAIT_MOST);
    let end = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
    loop {
        let mut st = status(node, req)?;
        if reached(&st, until) {
            st["timed_out"] = json!(false);
            // Why the wait ended (ARCADE #13): the milestone itself, or the transaction failing or being dropped
            // first (then `reached` is false).
            let failed = matches!(st["state"].as_str(), Some("broadcast_failed" | "dropped"));
            st["reached"] = json!(!failed);
            st["ended"] = json!(if failed { st["state"].as_str().unwrap_or("").to_string() } else { until.to_string() });
            return Ok(st);
        }
        if std::time::Instant::now() >= end || crate::STOP_ASKED.load(std::sync::atomic::Ordering::Relaxed) {
            st["timed_out"] = json!(true);
            st["reached"] = json!(false);
            st["ended"] = json!("timeout");
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

    #[test]
    fn the_receipt_is_written_at_signing_before_the_answer() {
        let (_g, t) = crate::http::tests::node();
        let id = arcade_chain::Hash256([7u8; 32]);
        let req = json!({"cmd": "ledger.send", "hex": "00", "client_ref": "sign-first-1"});
        // A send that signs, then fails on the way out (its broadcast refused).
        let seen_mid = std::cell::Cell::new(false);
        let out = run(&t.node, "ledger.send", &req, || {
            signed(&t.node, "arctest", &id);
            // Already on record while the send still runs (a caller that timed out asks now).
            let mid = by_ref(&t.node, "sign-first-1").unwrap();
            seen_mid.set(mid["state"] == "signed" && mid["txids"][0] == id.to_hex());
            outcome(&id.to_hex(), "local", false, "the fee is under 0.001 ARCADE per 1,000 bytes");
            Err("the fee is under 0.001 ARCADE per 1,000 bytes".to_string())
        });
        assert!(out.is_err() && seen_mid.get(), "the receipt was there before the send answered");
        // A retry never signs again: it gets client_ref_used and the txid.
        let again = run(&t.node, "ledger.send", &req, || panic!("signed twice"));
        let e = again.unwrap_err();
        assert_eq!(crate::api::error_code(&e), "client_ref_used", "{e}");
        assert!(e.contains(&id.to_hex()));
        assert_eq!(refusal_code("fee_too_low: whatever"), "fee_too_low");
        assert_eq!(refusal_code("txn-mempool-conflict"), "conflict");
        assert_eq!(refusal_code("bad-txns-inputs-missingorspent"), "missing_coin");
        let (acked, refused) = outcomes_of(&id.to_hex());
        assert!(acked.is_empty() && refused[0]["code"] == "fee_too_low");
        // Signed outside a send (a background resend): no receipt.
        let other = arcade_chain::Hash256([8u8; 32]);
        signed(&t.node, "arctest", &other);
        assert!(receipt(&t.node, "arctest", &other.to_hex()).is_none());
    }

    #[test]
    fn a_retry_while_the_first_still_runs_gets_what_it_signed() {
        let (_g, t) = crate::http::tests::node();
        let id = arcade_chain::Hash256([9u8; 32]);
        let req = json!({"cmd": "ledger.send", "hex": "00", "client_ref": "slow-send-1"});
        let node = &t.node;
        std::thread::scope(|sc| {
            let (tx, rx) = std::sync::mpsc::channel();
            let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
            let first = sc.spawn(move || {
                run(node, "ledger.send", &req, || {
                    signed(node, "arctest", &id);
                    tx.send(()).unwrap();
                    // The broadcast hangs until the retry has its answer.
                    go_rx.recv().unwrap();
                    Ok(json!({"txid": id.to_hex(), "fee": 100}))
                })
            });
            rx.recv().unwrap();
            let retry = run(node, "ledger.send", &json!({"cmd": "ledger.send", "hex": "00", "client_ref": "slow-send-1"}), || panic!("signed twice")).unwrap();
            assert_eq!(retry["txid"], id.to_hex());
            assert_eq!(retry["in_progress"], true);
            go_tx.send(()).unwrap();
            let a = first.join().unwrap().unwrap();
            assert_eq!(a["receipt"]["txids"][0], id.to_hex());
        });
        let after = run(&t.node, "ledger.send", &json!({"cmd": "ledger.send", "hex": "00", "client_ref": "slow-send-1"}), || panic!("signed twice")).unwrap();
        assert_eq!(after["replayed"], true);
        assert!(after.get("in_progress").is_none(), "finished: the first answer itself");
    }

    #[test]
    fn a_refusal_by_every_peer_is_broadcast_failed() {
        let st = |s: &str| json!({"state": s});
        assert!(reached(&st("broadcast_failed"), "confirmed"), "a failed broadcast ends the wait");
        assert!(reached(&st("broadcast_failed"), "broadcast_ok"));
        let acked = json!({"state": "accepted_local", "broadcast": {"peers_sent": 0, "peers_holding": 0, "peers_acked": 1}});
        assert!(reached(&acked, "seen_in_peer_mempool"));
    }
}

#[cfg(test)]
mod milestone_tests {
    use super::*;
    #[test]
    fn milestones_follow_the_broadcast_counts() {
        let mut v = json!({"state": "accepted_local", "broadcast": {"peers_sent": 1, "peers_holding": 0, "peers_acked": 0}});
        with_milestones(&mut v);
        assert_eq!(v["milestone"], "broadcast_ok");
        let mut v = json!({"state": "accepted_local", "broadcast": {"peers_sent": 0, "peers_holding": 0, "peers_acked": 1}});
        with_milestones(&mut v);
        assert_eq!(v["milestone"], "seen_in_peer_mempool");
        let mut v = json!({"state": "confirmed"});
        with_milestones(&mut v);
        assert_eq!(v["milestone"], "confirmed");
        assert!(v["milestones"].as_array().unwrap().iter().all(|m| m["reached"] == true));
        let mut v = json!({"state": "unknown"});
        with_milestones(&mut v);
        assert!(v["milestone"].is_null());
    }
}
