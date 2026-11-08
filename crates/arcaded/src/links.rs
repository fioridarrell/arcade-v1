//! Wallets linked to an @tag, with proof (V1.0).
//!
//! The existing arcades keep imported wallets in sealed per-account records, so another arcade
//! can only take their word for it. A V1.0 link carries two signatures instead:
//!   - the imported wallet's key signs the existing arcades' import message
//!     ("DogecoinArcade wallet import\n" owner pubkey "\n" net "\n" address "\n" wallet pubkey),
//!     so only someone holding that wallet can attach it;
//!   - the @tag's own key signs the whole record, so nobody can attach wallets to someone else's tag.
//! Both are ECDSA over sha256d, low-S DER. A record with "on": false (signed by the tag's key
//! alone) removes a link; the newer record wins. Records travel in a V1.0 "lk" frame (existing
//! arcades ignore unknown kinds) and every arcade checks them before keeping them.

use crate::node::Node;
use arcade_chain::params::{by_key, by_net, ChainParams};
use arcade_ledger::address::{base58check, hash160};
use arcade_wallet::keys::{from_wif, sign_message, verify_message};
use arcade_wallet::secp256k1::{PublicKey, SECP256K1};
use serde_json::{json, Value};

const PREFIX: &str = "link/";

fn wallet_msg(r: &Value) -> String {
    format!("DogecoinArcade wallet import\n{}\n{}\n{}\n{}", s(r, "opk"), s(r, "net"), s(r, "a"), s(r, "pk"))
}

fn owner_msg(r: &Value) -> String {
    format!(
        "Arcade V1.0 wallet link\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        s(r, "home"),
        s(r, "tag"),
        s(r, "net"),
        s(r, "a"),
        s(r, "pk"),
        r["at"].as_u64().unwrap_or(0),
        if r["on"] == true { "on" } else { "off" }
    )
}

fn s<'a>(r: &'a Value, k: &str) -> &'a str {
    r[k].as_str().unwrap_or("")
}

fn address_of_pubkey(pk_hex: &str, p: &ChainParams) -> Option<String> {
    let b = hex::decode(pk_hex).ok()?;
    PublicKey::from_slice(&b).ok()?;
    Some(base58check(p.p2pkh_version, &hash160(&b)))
}

/// Check a record against this arcade's chain: both signatures, the address, and that the
/// owner key still holds the @tag.
pub fn verify(node: &Node, r: &Value) -> Result<(), String> {
    let home = node.params;
    let row = arcade_ledger::index::tag_row(&node.chain, home.chain_no, s(r, "tag"))
        .map_err(|e| e.to_string())?
        .ok_or("the @tag is not claimed")?;
    check(r, home, &row.address)
}

/// The checks that need no database: `owner` is the address the @tag row names.
fn check(r: &Value, home: &ChainParams, owner: &str) -> Result<(), String> {
    if s(r, "home") != home.net {
        return Err("a link for another home chain".into());
    }
    let p = by_net(s(r, "net")).ok_or("unknown chain")?;
    if r["at"].as_u64().unwrap_or(0) > arcade_app::now() + 600 {
        return Err("dated in the future".into());
    }
    if address_of_pubkey(s(r, "pk"), p).as_deref() != Some(s(r, "a")) {
        return Err("the address is not the wallet key's".into());
    }
    if address_of_pubkey(s(r, "opk"), home).as_deref() != Some(owner) {
        return Err("not signed by the @tag's key".into());
    }
    if !verify_message(s(r, "opk"), owner_msg(r).as_bytes(), s(r, "os")) {
        return Err("the @tag's signature does not check".into());
    }
    if r["on"] == true && !verify_message(s(r, "pk"), wallet_msg(r).as_bytes(), s(r, "ws")) {
        return Err("the wallet's signature does not check".into());
    }
    Ok(())
}

fn key_of(r: &Value) -> String {
    format!("{PREFIX}{}/{}/{}", s(r, "tag"), s(r, "net"), s(r, "a"))
}

fn stored(node: &Node, key: &str) -> Option<Value> {
    node.app.store.read(|rd| rd.get(arcade_app::META, key.as_bytes())).ok().flatten().and_then(|b| serde_json::from_slice(&b).ok())
}

/// Keep a checked record if it is newer than what we have. True when it was new.
fn keep(node: &Node, r: &Value) -> bool {
    let key = key_of(r);
    if let Some(old) = stored(node, &key) {
        if old["at"].as_u64() >= r["at"].as_u64() {
            return false;
        }
    }
    node.app.store.write(|w| w.put(arcade_app::META, key.as_bytes(), r.to_string().as_bytes())).is_ok()
}

/// Every record we hold (links and removals).
pub fn all(node: &Node) -> Vec<Value> {
    node.app
        .store
        .read(|rd| rd.scan_prefix(arcade_app::META, PREFIX.as_bytes()))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_slice(&v).ok())
        .collect()
}

/// Checked links of a tag on one chain (by mesh name).
pub fn for_tag(node: &Node, tag: &str, net: &str) -> Vec<Value> {
    let pre = format!("{PREFIX}{tag}/{net}/");
    node.app
        .store
        .read(|rd| rd.scan_prefix(arcade_app::META, pre.as_bytes()))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_slice::<Value>(&v).ok())
        .filter(|r| r["on"] == true && verify(node, r).is_ok())
        .collect()
}

/// Our own tag key for `tag` (the slot whose address the tag row names).
fn owner_key(node: &Node, tag: &str) -> Result<(String, arcade_wallet::SecretKey), String> {
    let home = node.params;
    let row = arcade_ledger::index::tag_row(&node.chain, home.chain_no, tag)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("@{tag} is not claimed"))?;
    for k in node.app.tag_keys(home.chain_no).map_err(|e| e.to_string())? {
        if k.address == row.address {
            let (k, sk) = node.app.tag_key(home, k.index).map_err(|e| e.to_string())?;
            return Ok((k.pubkey, sk));
        }
    }
    Err(format!("@{tag} is not this wallet's"))
}

fn send(node: &Node, recs: Vec<Value>) {
    node.mesh_outbox.lock().unwrap().push(json!({"t": "lk", "v": 1, "recs": recs}));
}

/// Link a wallet (its WIF) to one of our tags on a chain.
pub fn link(node: &Node, chain: &str, wif: &str, tag: &str) -> Result<Value, String> {
    let p = by_key(chain).ok_or_else(|| format!("unknown chain {chain}"))?;
    let tag = tag.trim_start_matches('@').to_lowercase();
    let (version, sk, compressed) = from_wif(wif).ok_or("that is not a WIF key")?;
    if version != p.wif_version {
        return Err(format!("that key is not a {} key", p.name));
    }
    let pk = PublicKey::from_secret_key(SECP256K1, &sk);
    let pk_hex = hex::encode(if compressed { pk.serialize().to_vec() } else { pk.serialize_uncompressed().to_vec() });
    let a = address_of_pubkey(&pk_hex, p).ok_or("bad key")?;
    let (opk, osk) = owner_key(node, &tag)?;
    let mut r = json!({"k": "link", "v": 1, "home": node.params.net, "tag": tag, "net": p.net, "a": a, "pk": pk_hex,
                       "opk": opk, "at": arcade_app::now(), "on": true});
    r["ws"] = json!(sign_message(&sk, wallet_msg(&r).as_bytes()));
    r["os"] = json!(sign_message(&osk, owner_msg(&r).as_bytes()));
    verify(node, &r)?;
    node.app.put_imported_key(&format!("{}/{}", p.net, a), wif.trim().as_bytes()).map_err(|e| e.to_string())?;
    keep(node, &r);
    send(node, vec![r.clone()]);
    let _ = node.app.event("wallet", &format!("linked {} wallet {a} to @{tag}", p.name));
    Ok(r)
}

/// Remove a link (a record signed by the tag's key) and forget the imported key.
pub fn unlink(node: &Node, chain: &str, address: &str, tag: &str) -> Result<Value, String> {
    let p = by_key(chain).ok_or_else(|| format!("unknown chain {chain}"))?;
    let tag = tag.trim_start_matches('@').to_lowercase();
    let old = stored(node, &format!("{PREFIX}{tag}/{}/{address}", p.net)).ok_or("no such link")?;
    let (opk, osk) = owner_key(node, &tag)?;
    let mut r = json!({"k": "link", "v": 1, "home": node.params.net, "tag": tag, "net": p.net, "a": address, "pk": old["pk"],
                       "opk": opk, "at": arcade_app::now().max(old["at"].as_u64().unwrap_or(0) + 1), "on": false});
    r["os"] = json!(sign_message(&osk, owner_msg(&r).as_bytes()));
    verify(node, &r)?;
    keep(node, &r);
    let _ = node.app.drop_imported_key(&format!("{}/{address}", p.net));
    send(node, vec![r.clone()]);
    let _ = node.app.event("wallet", &format!("unlinked {} wallet {address} from @{tag}", p.name));
    Ok(r)
}

/// Our links (with a note whether we hold the key) for the Wallet tab.
pub fn mine(node: &Node, chain: &str) -> Result<Vec<Value>, String> {
    let p = by_key(chain).ok_or_else(|| format!("unknown chain {chain}"))?;
    let installed = p.chain_no == node.params.chain_no || node.extra.lock().unwrap().contains_key(p.key);
    Ok(all(node)
        .into_iter()
        .filter(|r| r["on"] == true && r["net"] == p.net && owner_key(node, s(r, "tag")).is_ok())
        .map(|r| {
            let bal = if installed {
                hex::decode(s(&r, "pk")).ok().and_then(|b| {
                    let sc = arcade_ledger::address::p2pkh_script(&hash160(&b));
                    node.chain.balance_for_script(p.chain_no, &sc).ok()
                })
            } else {
                None
            };
            let held = node.app.imported_key(&format!("{}/{}", p.net, s(&r, "a"))).ok().flatten().is_some();
            json!({"tag": r["tag"], "address": r["a"], "at": r["at"], "balance": bal, "key_here": held})
        })
        .collect())
}

/// Every record we hold, for a new link.
pub fn frame(node: &Node) -> Option<Value> {
    let recs: Vec<Value> = all(node).into_iter().take(500).collect();
    (!recs.is_empty()).then(|| json!({"t": "lk", "v": 1, "recs": recs}))
}

/// Records from another arcade: keep the ones that check and are new, and pass those on.
pub fn on_lk(node: &Node, head: &Value) {
    let Some(recs) = head["recs"].as_array() else { return };
    let mut fresh = Vec::new();
    for r in recs.iter().take(500) {
        if verify(node, r).is_ok() && keep(node, r) {
            fresh.push(r.clone());
        }
    }
    if !fresh.is_empty() {
        send(node, fresh);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcade_wallet::SecretKey;

    fn rec(home: &ChainParams, p: &ChainParams, wk: &SecretKey, ok: &SecretKey, on: bool) -> Value {
        let pk = hex::encode(PublicKey::from_secret_key(SECP256K1, wk).serialize());
        let opk = hex::encode(PublicKey::from_secret_key(SECP256K1, ok).serialize());
        let mut r = json!({"k": "link", "v": 1, "home": home.net, "tag": "apple", "net": p.net, "a": address_of_pubkey(&pk, p).unwrap(),
                           "pk": pk, "opk": opk, "at": 1000, "on": on});
        r["ws"] = json!(sign_message(wk, wallet_msg(&r).as_bytes()));
        r["os"] = json!(sign_message(ok, owner_msg(&r).as_bytes()));
        r
    }

    #[test]
    fn link_records() {
        let home = by_key("peptest").unwrap();
        let p = by_key("flop").unwrap();
        let (wk, ok, other) = (SecretKey::from_slice(&[3; 32]).unwrap(), SecretKey::from_slice(&[4; 32]).unwrap(), SecretKey::from_slice(&[5; 32]).unwrap());
        let owner = address_of_pubkey(&hex::encode(PublicKey::from_secret_key(SECP256K1, &ok).serialize()), home).unwrap();
        let r = rec(home, p, &wk, &ok, true);
        assert_eq!(check(&r, home, &owner), Ok(()));
        // Someone else's key can't sign for the tag.
        let bad = rec(home, p, &wk, &other, true);
        assert!(check(&bad, home, &owner).is_err());
        // Tampering with the address, chain, time or on/off breaks a signature.
        for (k, v) in [("at", json!(1001)), ("on", json!(false)), ("tag", json!("pear"))] {
            let mut t = r.clone();
            t[k] = v;
            assert!(check(&t, home, &owner).is_err(), "{k}");
        }
        let mut t = r.clone();
        t["a"] = json!(address_of_pubkey(&hex::encode(PublicKey::from_secret_key(SECP256K1, &other).serialize()), p).unwrap());
        assert!(check(&t, home, &owner).is_err());
        // A removal needs only the tag's signature.
        let mut off = rec(home, p, &wk, &ok, false);
        off.as_object_mut().unwrap().remove("ws");
        assert_eq!(check(&off, home, &owner), Ok(()));
    }
}
