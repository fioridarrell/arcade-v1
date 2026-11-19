//! Wallet proofs on profiles (Martin 2026-11-18: "all of the addresses that are associated with that profile, all coin
//! types, including all imported wallets, each sign ... and have them published automatically when the profile is
//! published"): every address of this wallet's, on every coin this arcade knows, and every imported wallet sign that
//! they belong to its ARCADE address (A:// op 7, arcade_ledger::aproto), so a card shows each as verifiably theirs.
//!
//! Published by itself, no card (Martin: the ARCADE it costs came from the faucet): with the profile, and again
//! whenever the set changes (a wallet imported or removed). The newest set wins; a removed wallet's proof stays in
//! the chain's history but leaves every card. A set goes out in records of 7 proofs, each with the set's number
//! (higher than the chain's last for this owner), so its records count in whatever order a block holds them.

use crate::node::Node;
use arcade_ledger::aproto::{self, WalletProof};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How often the set is looked at, and how long a set sent waits for a block before it is sent again.
const LOOK_EVERY: Duration = Duration::from_secs(10 * 60);
const RESEND_AFTER: Duration = Duration::from_secs(60 * 60);

/// The height on each ARCADE network from which op 7 is known (the same on every arcade that runs it).
pub fn known_from(net: &str) -> Option<u64> {
    if crate::arcsvc::local() {
        return Some(0); // a closed test network (--arc-local): from its start
    }
    match net {
        "arctest" => Some(2_000),
        _ => None,
    }
}

/// What was sent last, and when: (the set, at).
static SENT: Mutex<Option<(BTreeSet<(String, String)>, Instant)>> = Mutex::new(None);

/// Every address to prove: (coin, address, key, compressed). This wallet's own on each coin (the key holding a tag
/// there, else its first), then each imported wallet's; the ARCADE chain itself is the owner, not proven.
fn signers(node: &Node) -> Vec<(String, String, arcade_wallet::SecretKey, bool)> {
    let mut out: Vec<(String, String, arcade_wallet::SecretKey, bool)> = Vec::new();
    if !node.app.has_wallet().unwrap_or(false) {
        return out;
    }
    for cp in arcade_chain::params::ALL_CHAINS.iter().copied() {
        if crate::arcsvc::runs(cp) {
            continue;
        }
        let k = node.app.tag_keys(cp.chain_no).ok().and_then(|ks| ks.into_iter().find(|k| k.tag.is_some()));
        let got = match k {
            Some(k) => node.app.tag_key(cp, k.index).ok(),
            None => node.app.tag_key(cp, 0).ok(),
        };
        if let Some((k, sk)) = got {
            out.push((cp.key.to_string(), k.address, sk, true));
        }
    }
    for e in crate::importsvc::list_raw(node) {
        let Some((sk, p, compressed)) = crate::importsvc::key_for(node, &e) else { continue };
        let Some(addr) = e["address"].as_str() else { continue };
        if arcade_chain::params::by_key(p.key).is_none() || out.iter().any(|(c, a, ..)| c == p.key && a == addr) {
            continue; // an ARCADE key (a publisher's), or an address already here
        }
        out.push((p.key.to_string(), addr.to_string(), sk, compressed));
    }
    out
}

/// The set the chain holds for `owner`: (coin, address) -> the block it was proven in.
pub fn proven(node: &Node, c: u16, owner: &str) -> Vec<(String, String, u64)> {
    let pre = arcade_store::KeyBuf::new().u16(c).bytes(owner.as_bytes()).u8(0).done();
    node.chain.store(c).read(|r| r.scan_prefix(arcade_ledger::engine::A_WALLET, &pre)).unwrap_or_default().into_iter()
        .filter_map(|(k, v)| {
            let rest = k.get(pre.len()..)?;
            let i = rest.iter().position(|b| *b == 0)?;
            let coin = String::from_utf8(rest[..i].to_vec()).ok()?;
            let address = String::from_utf8(rest[i + 1..].to_vec()).ok()?;
            let h = serde_json::from_slice::<Value>(&v).ok()?["h"].as_u64().unwrap_or(0);
            Some((coin, address, h))
        })
        .collect()
}

/// Publish this wallet's proofs when they differ from what the chain holds (or always, with `force`). The records'
/// txids, or why nothing was sent.
pub fn publish(node: &Node, force: bool) -> Result<Value, String> {
    let p = crate::arcledger::write_chain()?;
    let tip = crate::arcledger::tip_height(node, p);
    if !aproto::wallets_known(p.chain_no, tip + 1) {
        let from = known_from(p.net).map_or("never on this network".to_string(), |h| format!("from block {h}"));
        return Ok(json!({"sent": false, "why": format!("wallet proofs are known {from}; the chain is at {tip}")}));
    }
    let owner = crate::arcledger::sender(node, p)?.0;
    let want = signers(node);
    let want_set: BTreeSet<(String, String)> = want.iter().map(|(c, a, ..)| (c.clone(), a.clone())).collect();
    // What the chain holds: this arcade's own tables, or a keeper's while ARCADE is borrowed here.
    let remote = crate::arcledger::borrowed_wallet_state(node, &owner)?;
    let have: BTreeSet<(String, String)> = match &remote {
        Some(v) => v["proven"].as_array().into_iter().flatten().filter_map(|x| Some((x[0].as_str()?.to_string(), x[1].as_str()?.to_string()))).collect(),
        None => proven(node, p.chain_no, &owner).into_iter().map(|(c, a, _)| (c, a)).collect(),
    };
    if !force && want_set == have {
        return Ok(json!({"sent": false, "why": "the chain already holds these proofs", "proofs": want_set.len()}));
    }
    if !force {
        if let Some((set, at)) = SENT.lock().unwrap().as_ref() {
            if *set == want_set && at.elapsed() < RESEND_AFTER {
                return Ok(json!({"sent": false, "why": "sent already; waiting for a block", "proofs": want_set.len()}));
            }
        }
    }
    // Every record of the set, or none: a set cut short by coins running out would leave the card with some of them.
    let records = want.len().div_ceil(aproto::MAX_WALLETS).max(1) as u64;
    let need = records * (300_000 + arcade_arc::params::DUST);
    let (_, _, sk) = crate::arcledger::sender(node, p)?;
    let script = arcade_wallet::keys::p2pkh_script_for(&arcade_wallet::secp256k1::PublicKey::from_secret_key(arcade_wallet::secp256k1::SECP256K1, &sk));
    let have: u64 = crate::claims::arc_coins(node, p, &script).iter().map(|u| u.value).sum::<u64>()
        + crate::arcsvc::waiting_outputs(&script).iter().map(|o| o.1).sum::<u64>();
    if have < need {
        return Ok(json!({"sent": false, "why": format!("waiting for ARCADE: {records} records need about {} ARCADE, the wallet has {}", need as f64 / 1e8, have as f64 / 1e8), "proofs": want_set.len()}));
    }
    let proofs: Vec<WalletProof> = want
        .iter()
        .map(|(coin, address, sk, compressed)| WalletProof { coin: coin.clone(), address: address.clone(), sig: aproto::sign_wallet_proof(sk, *compressed, &owner, coin, address) })
        .collect();
    let mut txids = Vec::new();
    let mut fees = 0u64;
    let chunks: Vec<&[WalletProof]> = if proofs.is_empty() { vec![&[][..]] } else { proofs.chunks(aproto::MAX_WALLETS).collect() };
    let last = match &remote {
        Some(v) => v["last_set"].as_u64().unwrap_or(0),
        None => node.chain.store(p.chain_no).read(|r| r.get(arcade_ledger::engine::A_WALLET, &arcade_store::KeyBuf::new().u16(p.chain_no).bytes(owner.as_bytes()).u8(1).done()))
            .ok().flatten().and_then(|v| serde_json::from_slice::<Value>(&v).ok()).and_then(|v| v["set"].as_u64()).unwrap_or(0),
    };
    let set = arcade_app::now().max(last + 1);
    for part in chunks {
        let body = aproto::wallets_body(set, part);
        let (txid, fee) = crate::arcledger::send(node, &aproto::payload(aproto::OP_WALLETS, &body))
            .map_err(|e| if txids.is_empty() { e } else { format!("{e} (after {} of the records went out)", txids.len()) })?;
        txids.push(txid);
        fees += fee;
    }
    *SENT.lock().unwrap() = Some((want_set.clone(), Instant::now()));
    let _ = node.app.event("profile", &format!("published proofs of your {} wallet addresses on ARCADE ({} records, fee {})", want_set.len(), txids.len(), fees as f64 / 1e8));
    Ok(json!({"sent": true, "proofs": want_set.len(), "txids": txids, "fee": fees}))
}

/// Keeps the proofs current by itself: once this wallet has a published A:// profile, its proofs follow every change.
pub fn run(node: std::sync::Arc<Node>) {
    let _ = std::thread::Builder::new().name("wallet proofs".into()).spawn(move || loop {
        std::thread::sleep(LOOK_EVERY);
        let Ok(p) = crate::arcledger::write_chain() else { continue };
        let Ok((owner, ..)) = crate::arcledger::sender(&node, p) else { continue };
        let has_profile = match crate::arcledger::borrowed_wallet_state(&node, &owner) {
            Ok(Some(v)) => v["has_profile"] == true,
            Ok(None) => {
                let key = arcade_store::KeyBuf::new().u16(p.chain_no).bytes(owner.as_bytes()).done();
                node.chain.store(p.chain_no).read(|r| r.get(arcade_ledger::engine::A_PROFILE, &key)).ok().flatten().is_some()
            }
            Err(_) => false,
        };
        if !has_profile {
            continue;
        }
        if let Err(e) = publish(&node, false) {
            eprintln!("profile: wallet proofs not sent: {e}");
        }
    });
}

/// `a.wallets {tag | address}`: the addresses the chain holds proofs of for someone (this wallet's when neither),
/// and for this wallet also what would be proven now.
pub fn a_wallets(node: &Node, req: &Value) -> Result<Value, String> {
    let p = crate::arcledger::write_chain()?;
    let mine = crate::arcledger::sender(node, p).map(|s| s.0).ok();
    let mut v = if crate::arcledger::chain().is_ok() {
        let a = crate::arcledger::who(node, req)?;
        json!({"address": a, "wallets": cards(node, p.chain_no, &a), "known_from": known_from(p.net)})
    } else {
        // Borrowed: the keeper's copy of the proofs, for the address asked about (this wallet's when none is named).
        let a = match (req["address"].as_str().filter(|s| !s.is_empty()), req["tag"].as_str().filter(|s| !s.is_empty())) {
            (Some(a), _) => a.to_string(),
            (None, Some(t)) => crate::hometag::owner_on(node, p, t).map(|(a, _)| a).ok_or_else(|| format!("@{} isn't claimed on the ARCADE chain", t.trim_start_matches('@')))?,
            _ => mine.clone().ok_or("this wallet has no ARCADE key yet")?,
        };
        let st = crate::arcledger::borrowed_wallet_state(node, &a)?.ok_or("the ARCADE chain isn't usable here")?;
        let wallets: Vec<Value> = st["proven"].as_array().into_iter().flatten()
            .map(|x| json!({"coin": x[0], "address": x[1], "height": x[2], "verified": true})).collect();
        json!({"address": a, "wallets": wallets, "known_from": st["known_from"]})
    };
    let a = v["address"].as_str().unwrap_or("").to_string();
    if mine.as_deref() == Some(a.as_str()) {
        v["here"] = json!(signers(node).into_iter().map(|(c, a, ..)| json!({"coin": c, "address": a})).collect::<Vec<_>>());
    }
    Ok(v)
}

/// The proven addresses on a card: [{coin, address, height, verified}].
pub fn cards(node: &Node, c: u16, owner: &str) -> Vec<Value> {
    proven(node, c, owner).into_iter().map(|(coin, address, h)| json!({"coin": coin, "address": address, "height": h, "verified": true})).collect()
}
