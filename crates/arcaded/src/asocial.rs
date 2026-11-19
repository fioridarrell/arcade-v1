//! A:// ops 8 to 10 on the ARCADE chain (arcade_ledger::aproto; docs/APROTO.md): reacts, replies and tips on any
//! transaction of any chain (Martin 2026-11-18: "make sure the stuff is app agnostic so anybody can build with it").
//!
//! Reads come from the ARCADE chain's ledger: this arcade's own copy, or (borrowed, or not caught up) an arcade that
//! keeps it, through the explorer (`explorer.a_reactions`, `explorer.a_replies`, forwarded like every ledger read).
//! Writes are A:// records sent from the wallet's key on ARCADE (crate::arcledger::send); a tip pays in the same
//! transaction.

use crate::node::Node;
use arcade_ledger::aproto::{self, Target};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

/// The height on each ARCADE network from which ops 8 to 10 are known (the same on every arcade that runs it). None
/// until a release names it (Martin picks the height). A closed test network (--arc-local) knows them from
/// `ARCADE_A_SOCIAL_FROM` (default 0: from its start).
pub fn known_from(net: &str) -> Option<u64> {
    if crate::arcsvc::local() {
        return Some(std::env::var("ARCADE_A_SOCIAL_FROM").ok().and_then(|v| v.parse().ok()).unwrap_or(0));
    }
    match net {
        "arctest" => ARCTEST_FROM,
        _ => None,
    }
}

/// ARCADE testnet's height for ops 8 to 10 (Martin 2026-11-18: "sooner if possible"; arcades update within ~40 min).
const ARCTEST_FROM: Option<u64> = Some(2120);

const MAX_TARGETS: usize = 100;

fn target_of(chain: &Value, txid: &Value) -> Result<Target, String> {
    let (c, t) = (chain.as_str().unwrap_or(""), txid.as_str().unwrap_or(""));
    Target::new(c, t).ok_or_else(|| format!("a target is a chain key (1 to 12 lowercase letters and digits) and a txid (64 hex): got {c:?} {t:?}"))
}

fn arc_key() -> Result<&'static str, String> {
    crate::arcsvc::defined().map(|p| p.key).ok_or_else(|| "this arcade knows no ARCADE chain".to_string())
}

// ---------------------------------------------------------------- the explorer's reads (any arcade answers them)

/// `explorer.a_reactions {chain: the ARCADE chain, targets: [{chain, txid}], me?}`: per target its likes, dislikes,
/// replies and tips (total, count, per recipient), and `me`'s react and tips.
pub fn ledger_reactions(node: &Node, c: u16, req: &Value) -> Result<Value, String> {
    let targets = req["targets"].as_array().ok_or("targets: [{chain, txid}, ...]")?;
    if targets.is_empty() || targets.len() > MAX_TARGETS {
        return Err(format!("targets: 1 to {MAX_TARGETS}"));
    }
    let me = req["me"].as_str().filter(|m| !m.is_empty());
    let store = node.chain.store(c);
    let mut out = Vec::new();
    for t in targets {
        let t = target_of(&t["chain"], &t["txid"])?;
        let tk = t.key();
        let get = |table: &str, key: &[u8]| -> Option<Value> {
            store.read(|r| r.get(table, key)).ok().flatten().and_then(|v| serde_json::from_slice::<Value>(&v).ok())
        };
        let count = get(arcade_ledger::engine::A_COUNT, &arcade_store::KeyBuf::new().u16(c).bytes(&tk).done()).unwrap_or(Value::Null);
        let pre = arcade_store::KeyBuf::new().u16(c).bytes(&tk).u8(0).done();
        let mut to = Vec::new();
        let (mut total, mut n) = (0u64, 0u64);
        for (k, v) in store.read(|r| r.scan_prefix(arcade_ledger::engine::A_TIP, &pre)).unwrap_or_default() {
            let Ok(v) = serde_json::from_slice::<Value>(&v) else { continue };
            let address = String::from_utf8_lossy(&k[pre.len()..]).to_string();
            let s = v["s"].as_u64().unwrap_or(0);
            total += s;
            to.push(json!({"address": address, "sats": s, "count": v["n"]}));
        }
        // A tip with two recipients counts once in `count`: the tippers' rows say how many tips there were.
        for (_, v) in store.read(|r| r.scan_prefix(arcade_ledger::engine::A_TIPPER, &pre)).unwrap_or_default() {
            n += serde_json::from_slice::<Value>(&v).ok().and_then(|v| v["n"].as_u64()).unwrap_or(0);
        }
        let mine = me.map(|m| {
            let react = get(arcade_ledger::engine::A_REACT, &arcade_store::KeyBuf::new().u16(c).bytes(&tk).u8(0).bytes(m.as_bytes()).done())
                .and_then(|v| v["k"].as_u64())
                .map_or(Value::Null, |k| json!(if k == aproto::LIKE as u64 { "like" } else { "dislike" }));
            let tipped = get(arcade_ledger::engine::A_TIPPER, &arcade_store::KeyBuf::new().u16(c).bytes(&tk).u8(0).bytes(m.as_bytes()).done())
                .and_then(|v| v["s"].as_u64()).unwrap_or(0);
            json!({"react": react, "tipped": tipped})
        });
        out.push(json!({"chain": t.chain, "txid": t.txid_hex(), "likes": count["l"].as_u64().unwrap_or(0), "dislikes": count["d"].as_u64().unwrap_or(0),
                        "replies": count["r"].as_u64().unwrap_or(0), "tips": {"total": total, "count": n, "to": to}, "mine": mine}));
    }
    Ok(json!({"targets": out}))
}

/// `explorer.a_replies {chain: the ARCADE chain, target: {chain, txid}, offset?, limit?}`: oldest first.
pub fn ledger_replies(node: &Node, c: u16, req: &Value) -> Result<Value, String> {
    let t = target_of(&req["target"]["chain"], &req["target"]["txid"])?;
    let pre = arcade_store::KeyBuf::new().u16(c).bytes(&t.key()).u8(0).done();
    let rows = node.chain.store(c).read(|r| r.scan_prefix(arcade_ledger::engine::A_REPLY, &pre)).unwrap_or_default();
    let total = rows.len();
    let offset = req["offset"].as_u64().unwrap_or(0) as usize;
    let limit = req["limit"].as_u64().unwrap_or(50).clamp(1, 200) as usize;
    let replies: Vec<Value> = rows.into_iter().skip(offset).take(limit).filter_map(|(k, v)| {
        let v: Value = serde_json::from_slice(&v).ok()?;
        let h = k.get(pre.len()..pre.len() + 4).and_then(|b| b.try_into().ok()).map(u32::from_be_bytes).unwrap_or(0);
        let a = v["a"].as_str().unwrap_or("").to_string();
        let tag = arcade_ledger::index::tag_of(&node.chain, c, &a).ok().flatten();
        Some(json!({"txid": v["txid"], "author": {"address": a, "tag": tag}, "text": v["t"], "height": h}))
    }).collect();
    Ok(json!({"chain": t.chain, "txid": t.txid_hex(), "replies": replies, "total": total, "offset": offset}))
}

// ---------------------------------------------------------------- authors

/// Authors found (target -> the ARCADE address of the key that signed the target's first input): a transaction's
/// inputs never change, so a find is kept.
static AUTHORS: Mutex<Option<HashMap<(String, String), String>>> = Mutex::new(None);

/// The ARCADE address of a target's author, where it can be found: the key that signed the transaction's first input,
/// as an ARCADE address (one key, one person on every chain). None when the transaction can't be read here or on the
/// mesh, or its first input isn't a plain key's address.
pub fn author_of(node: &Node, t: &Target) -> Option<String> {
    let id = (t.chain.clone(), t.txid_hex());
    if let Some(a) = AUTHORS.lock().unwrap().get_or_insert_with(HashMap::new).get(&id) {
        return Some(a.clone());
    }
    let p = crate::arcsvc::defined()?;
    let tx = crate::explorer::handle(node, "explorer.tx", &json!({"chain": t.chain, "txid": id.1})).ok()?;
    let first = tx["inputs"].as_array()?.first()?["address"].as_str()?.to_string();
    let (_, h) = arcade_ledger::address::decode_base58check(&first).filter(|(_, h)| h.len() == 20)?;
    // Only a key's address (P2PKH): a script hash is no key.
    let src = crate::chains::find(&t.chain)?;
    let h20: [u8; 20] = h.as_slice().try_into().ok()?;
    if arcade_ledger::address::script_for_address(src, &first)? != arcade_ledger::address::p2pkh_script(&h20) {
        return None;
    }
    let a = arcade_ledger::address::base58check(p.p2pkh_version, &h);
    AUTHORS.lock().unwrap().get_or_insert_with(HashMap::new).insert(id, a.clone());
    Some(a)
}

// ---------------------------------------------------------------- the commands

/// `a.reactions {targets: [{chain, txid}] (up to 100), me?}`: per target likes, dislikes, replies and tips (total,
/// count, per recipient, with `author` marking the target's author's address where it is known, and `to_author`), plus
/// `mine` (`me`: true for this wallet, or an ARCADE address).
pub fn reactions(node: &Node, req: &Value) -> Result<Value, String> {
    let mut q = json!({"chain": arc_key()?, "targets": req["targets"]});
    match &req["me"] {
        Value::Bool(true) => q["me"] = json!(crate::arcledger::own_address(node).ok()),
        Value::String(m) if !m.is_empty() => q["me"] = json!(m),
        _ => {}
    }
    let mut v = crate::explorer::handle(node, "explorer.a_reactions", &q)?;
    for t in v["targets"].as_array_mut().into_iter().flatten() {
        let Some(target) = Target::new(t["chain"].as_str().unwrap_or(""), t["txid"].as_str().unwrap_or("")) else { continue };
        let author = author_of(node, &target);
        let mut to_author = 0u64;
        for r in t["tips"]["to"].as_array_mut().into_iter().flatten() {
            let is = author.as_deref().is_some_and(|a| r["address"].as_str() == Some(a));
            if is {
                to_author += r["sats"].as_u64().unwrap_or(0);
            }
            r["author"] = json!(is);
        }
        t["tips"]["to_author"] = json!(to_author);
        t["author"] = author.map_or(Value::Null, |a| json!({"address": a}));
    }
    Ok(v)
}

/// `a.replies {chain, txid, offset?, limit?}`: a target's replies, oldest first.
pub fn replies(node: &Node, req: &Value) -> Result<Value, String> {
    crate::explorer::handle(node, "explorer.a_replies", &json!({"chain": arc_key()?, "target": {"chain": req["chain"], "txid": req["txid"]},
                                                                "offset": req["offset"], "limit": req["limit"]}))
}

/// Ops 8 to 10 are written only once the next block knows them.
fn known_now(node: &Node) -> Result<&'static arcade_chain::params::ChainParams, String> {
    let p = crate::arcledger::chain()?;
    let tip = node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height as u64).unwrap_or(0);
    if !aproto::social_known(p.chain_no, tip + 1) {
        let from = known_from(p.net).map_or("not yet on this network".to_string(), |h| format!("from block {h}"));
        return Err(format!("reacts, replies and tips on {} are known {from}; the chain is at {tip}", p.name));
    }
    Ok(p)
}

/// `a.react {chain, txid, kind: like | unlike | dislike | undislike}`.
pub fn react(node: &Node, req: &Value) -> Result<Value, String> {
    known_now(node)?;
    let t = target_of(&req["chain"], &req["txid"])?;
    let kind = match req["kind"].as_str().unwrap_or("") {
        "like" => aproto::LIKE,
        "unlike" => aproto::UNLIKE,
        "dislike" => aproto::DISLIKE,
        "undislike" => aproto::UNDISLIKE,
        k => return Err(format!("kind: like, unlike, dislike or undislike (not {k:?})")),
    };
    let (txid, fee) = crate::arcledger::send(node, &aproto::payload(aproto::OP_REACT, &aproto::react_body(&t, kind)))?;
    Ok(json!({"txid": txid, "fee": fee, "chain": t.chain, "target": t.txid_hex(), "kind": req["kind"]}))
}

/// `a.reply {chain, txid, text}`: 1 to 500 bytes.
pub fn reply(node: &Node, req: &Value) -> Result<Value, String> {
    known_now(node)?;
    let t = target_of(&req["chain"], &req["txid"])?;
    let text = req["text"].as_str().unwrap_or("").trim();
    if text.is_empty() || text.len() > aproto::MAX_REPLY {
        return Err(format!("a reply is 1 to {} bytes", aproto::MAX_REPLY));
    }
    let (txid, fee) = crate::arcledger::send(node, &aproto::payload(aproto::OP_REPLY, &aproto::text_body(&t, text)))?;
    Ok(json!({"txid": txid, "fee": fee, "chain": t.chain, "target": t.txid_hex()}))
}

/// `a.tip {chain, txid, amount, to?, memo?}`: pay `amount` (sats of ARCADE) to `to` (an ARCADE address; default: the
/// target's author, where one can be found), with the tip record in the same transaction.
pub fn tip(node: &Node, req: &Value) -> Result<Value, String> {
    let p = known_now(node)?;
    let t = target_of(&req["chain"], &req["txid"])?;
    let amount = req["amount"].as_u64().ok_or("amount: sats of ARCADE")?;
    if amount < arcade_arc::params::DUST {
        return Err(format!("a tip is at least {} sats", arcade_arc::params::DUST));
    }
    let memo = req["memo"].as_str().unwrap_or("").trim();
    if memo.len() > aproto::MAX_TIP_MEMO {
        return Err(format!("a tip's memo is {} bytes at most", aproto::MAX_TIP_MEMO));
    }
    let to = match req["to"].as_str().filter(|s| !s.is_empty()) {
        Some(a) => a.to_string(),
        None => author_of(node, &t).ok_or("no author found for that transaction: say who the tip is for (to: an ARCADE address)")?,
    };
    if arcade_ledger::address::script_for_address(p, &to).is_none() {
        return Err(format!("{to} isn't an ARCADE address"));
    }
    let me = crate::arcledger::own_address(node)?;
    if to == me {
        return Err("a tip goes to someone else".into());
    }
    let (txid, fee) = crate::arcledger::send_paying(node, &aproto::payload(aproto::OP_TIP, &aproto::text_body(&t, memo)), &to, amount)?;
    Ok(json!({"txid": txid, "fee": fee, "chain": t.chain, "target": t.txid_hex(), "to": to, "amount": amount}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(t: &crate::sitesvc::tests::T, c: u16, table: &str, key: Vec<u8>, v: Value) {
        t.node.chain.store(c).write(|w| w.put(table, &key, v.to_string().as_bytes())).unwrap();
    }

    /// The explorer's reads give every arcade the ledger's counts as they are: likes, dislikes, replies, tips per
    /// recipient (a two-recipient tip counted once), `me`'s react and tips.
    #[test]
    fn reads_give_the_ledgers_counts() {
        use arcade_ledger::engine::{A_COUNT, A_REACT, A_REPLY, A_TIP, A_TIPPER};
        use arcade_store::KeyBuf;
        let t = crate::sitesvc::tests::setup();
        let c = t.node.params.chain_no;
        let _ = t.node.chain.add_indexer(c, std::sync::Arc::new(arcade_ledger::engine::LedgerIndexer));
        let tg = Target::new("peptest", &"cd".repeat(32)).unwrap();
        let tk = tg.key();
        let at = |who: &str| KeyBuf::new().u16(c).bytes(&tk).u8(0).bytes(who.as_bytes()).done();
        put(&t, c, A_COUNT, KeyBuf::new().u16(c).bytes(&tk).done(), json!({"l": 2, "d": 1, "r": 2}));
        put(&t, c, A_REACT, at("aMe"), json!({"k": 1, "h": 9}));
        put(&t, c, A_TIP, at("aBob"), json!({"s": 6000, "n": 2}));
        put(&t, c, A_TIP, at("aAnn"), json!({"s": 700, "n": 1}));
        put(&t, c, A_TIPPER, at("aMe"), json!({"s": 5700, "n": 1}));
        put(&t, c, A_TIPPER, at("aZed"), json!({"s": 1000, "n": 1}));
        for (h, txt) in [(10u32, "first"), (12, "second")] {
            put(&t, c, A_REPLY, KeyBuf::new().u16(c).bytes(&tk).u8(0).u32(h).u32(1).done(), json!({"a": "aAnn", "t": txt, "txid": "ee".repeat(32)}));
        }
        let r = ledger_reactions(&t.node, c, &json!({"targets": [{"chain": "peptest", "txid": "cd".repeat(32)}, {"chain": "doge", "txid": "00".repeat(32)}], "me": "aMe"})).unwrap();
        let a = &r["targets"][0];
        assert_eq!((a["likes"].as_u64(), a["dislikes"].as_u64(), a["replies"].as_u64()), (Some(2), Some(1), Some(2)), "{a}");
        assert_eq!(a["tips"]["total"], json!(6700));
        assert_eq!(a["tips"]["count"], json!(2), "tips counted once each, from the tippers' rows");
        assert_eq!(a["tips"]["to"].as_array().unwrap().len(), 2);
        assert_eq!(a["mine"], json!({"react": "like", "tipped": 5700}));
        let b = &r["targets"][1];
        assert_eq!((b["likes"].as_u64(), b["replies"].as_u64(), b["tips"]["total"].as_u64()), (Some(0), Some(0), Some(0)), "an untouched target is all zero");
        let rp = ledger_replies(&t.node, c, &json!({"target": {"chain": "peptest", "txid": "cd".repeat(32)}, "offset": 1, "limit": 5})).unwrap();
        assert_eq!(rp["total"], json!(2));
        assert_eq!(rp["replies"][0]["text"], json!("second"));
        assert_eq!(rp["replies"][0]["height"], json!(12));
        // Bad questions are named.
        assert!(ledger_reactions(&t.node, c, &json!({"targets": []})).is_err());
        assert!(ledger_reactions(&t.node, c, &json!({"targets": vec![json!({"chain": "peptest", "txid": "cd".repeat(32)}); 101]})).is_err());
        assert!(ledger_reactions(&t.node, c, &json!({"targets": [{"chain": "Pep-Test", "txid": "cd".repeat(32)}]})).is_err());
        assert!(ledger_replies(&t.node, c, &json!({"target": {"chain": "peptest", "txid": "zz"}})).is_err());
    }

    /// Writes wait for the ops' height, and say when it is.
    #[test]
    fn writes_wait_for_their_height() {
        assert_eq!(known_from("arctest"), ARCTEST_FROM);
        assert_eq!(known_from("arc"), None);
    }
}
