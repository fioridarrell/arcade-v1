//! DigiDollars for the home chain's coin across chains, by hash-and-time locks (docs/MARKET.md steps 5 and 7),
//! V1.0 with V1.0. The maker sells dollars on a DigiByte chain for the coin of this arcade's home chain.
//!
//!   1. The taker asks the maker's arcade for its terms and keys (`xquote`), makes a secret, and locks the price
//!      on the home chain for LOCK_TAKER (htlc.rs: the maker claims with the secret, the taker refunds after).
//!   2. It tells the maker (`xlocked`). The maker's arcade reads that lock on its own copy of the chain (mined,
//!      paying the price to the lock it computes itself, time long enough), and locks the dollars for LOCK_MAKER
//!      (ddlock.rs: the taker claims with the same secret, the maker refunds after).
//!   3. The taker's arcade checks the dollar lock on its own copy of DigiByte (the address it computes, the record's
//!      amount, mined) and claims it, which puts the secret on DigiByte.
//!   4. The maker's arcade reads the secret off that claim and claims the price.
//! If either side stops, each refunds after its own time. Each arcade answers for its own side with its own key;
//! nothing here is anybody else's to sign. The secret is believed only when its SHA-256 is the lock's hash.

use crate::node::Node;
use arcade_chain::params::{by_key, ChainParams};
use arcade_chain::{Hash256, OutPoint, Tx};
use arcade_wallet::ddlock::{self, Lock};
use arcade_wallet::htlc::{self, Htlc};
use arcade_wallet::secp256k1::{PublicKey, XOnlyPublicKey, SECP256K1};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// The taker locks first and longest; the maker's lock ends sooner (MARKET.md: 8 h and 4 h, because a block may be
/// stamped up to 2 h ahead on these chains).
const LOCK_TAKER: u32 = 8 * 3600;
const LOCK_MAKER: u32 = 4 * 3600;
/// The maker refuses a taker's lock with less time left than this when it is told.
const LEAST_LEFT: u32 = 6 * 3600;

fn now() -> u32 {
    arcade_app::now() as u32
}

fn key(kind: &str, id: &str) -> Vec<u8> {
    format!("{kind}:{id}").into_bytes()
}

fn get(node: &Node, kind: &str, id: &str) -> Option<Value> {
    node.app.store.read(|r| r.get(arcade_app::META, &key(kind, id))).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok())
}

fn put(node: &Node, kind: &str, v: &Value) {
    let _ = node.app.store.write(|w| w.put(arcade_app::META, &key(kind, v["id"].as_str().unwrap_or("")), v.to_string().as_bytes()));
}

fn list(node: &Node, kind: &str) -> Vec<Value> {
    node.app
        .store
        .read(|r| r.scan_prefix(arcade_app::META, format!("{kind}:").as_bytes()))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_slice(&v).ok())
        .collect()
}

fn dd_chain(k: &str) -> Result<&'static ChainParams, String> {
    by_key(k).filter(|p| matches!(p.key, "dgb" | "dgbtest")).ok_or_else(|| "dollars live on dgb or dgbtest".to_string())
}

/// This wallet's key on a chain, and its public key.
fn wallet_key(node: &Node, p: &ChainParams) -> Result<(arcade_wallet::SecretKey, PublicKey), String> {
    let sk = node.app.tag_key(p, 0).map_err(|e| e.to_string())?.1;
    Ok((sk, PublicKey::from_secret_key(SECP256K1, &sk)))
}

/// A mined transaction's depth on a chain this arcade keeps (0: in the mempool or unknown).
fn depth(node: &Node, p: &ChainParams, txid: &Hash256) -> u32 {
    let Ok(Some((b, _))) = node.chain.tx_location(p, txid) else { return 0 };
    let Some((_, h, _)) = node.chain.block_at(p, b).ok().flatten() else { return 0 };
    let tip = node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height).unwrap_or(0);
    tip.saturating_sub(h) + 1
}

fn tx_of(node: &Node, p: &ChainParams, txid: &Hash256) -> Option<Tx> {
    if let Ok(Some((raw, _))) = node.chain.tx_raw(p, txid) {
        return Tx::parse_with(&raw, &p.format).ok();
    }
    node.chain.mempool(p.chain_no).ok()?.into_iter().filter_map(|(_, raw)| Tx::parse_with(&raw, &p.format).ok()).find(|t| &t.txid == txid)
}

/// Who spent an outpoint, in a block or waiting in the mempool.
fn spender(node: &Node, p: &ChainParams, op: &OutPoint) -> Option<Tx> {
    if let Ok(Some((b, pos, _))) = node.chain.spender(p, op) {
        let txid = node.chain.txid_at(p, b, pos).ok().flatten()?;
        return tx_of(node, p, &txid);
    }
    let txid = node.chain.mempool_spender(p.chain_no, op).ok().flatten()?;
    tx_of(node, p, &txid)
}

fn broadcast(node: &Node, p: &ChainParams, raw: &[u8]) -> Result<String, String> {
    let v = crate::sitesvc::broadcast(node, &json!({"hex": hex::encode(raw), "chain": p.key}))?;
    Ok(v["txid"].as_str().unwrap_or("").to_string())
}

// ---------------- orders (the maker's) ----------------

/// Place a cross-chain order: sell `cents` dollars on `dd` for `price` units of this arcade's home coin.
pub fn place(node: &Node, dd: &str, cents: i64, price: u64) -> Result<Value, String> {
    let p = dd_chain(dd)?;
    if cents < 100 || cents % 100 != 0 {
        return Err("DigiDollars trade in whole dollars, $1 or more".into());
    }
    if price < htlc::DUST {
        return Err("that price is under the dust line".into());
    }
    let (_, held, ..) = crate::ddsvc::trade_coins(node, p)?;
    if held.iter().map(|h| h.cents).sum::<i64>() < cents {
        return Err("this wallet doesn't hold that many dollars".into());
    }
    let id = arcade_mesh::random::hex(8);
    let o = json!({"id": id, "dd": p.key, "pay": node.params.key, "side": "sell", "cents": cents, "left": cents, "price": price,
                   "status": "open", "created": arcade_app::now(), "node": node.id_hex()});
    put(node, "xorder", &o);
    Ok(o)
}

fn price_of(o: &Value, cents: i64) -> u64 {
    // Rounded against the taker, who pays.
    let (c, pr) = (o["cents"].as_i64().unwrap_or(1).max(1) as u128, o["price"].as_u64().unwrap_or(0) as u128);
    (pr * cents as u128).div_ceil(c) as u64
}

/// The open cross-chain orders on this arcade and every linked one.
pub fn book(node: &Node) -> Vec<Value> {
    let mut all: Vec<Value> = list(node, "xorder").into_iter().filter(|o| o["status"] == "open").map(|mut o| { o["mine"] = json!(true); o }).collect();
    for peer in crate::shares::linked_ids(node) {
        if let Ok(a) = crate::shares::ask_linked(&peer, json!({"what": "mk", "net": node.params.net, "arg": json!({"op": "xbook", "chain": "dgbtest"}).to_string()})) {
            for mut o in a["orders"].as_array().cloned().unwrap_or_default() {
                o["node"] = json!(peer);
                all.push(o);
            }
        }
    }
    all
}

// ---------------- the maker answering (ddmarket::answer passes x* questions here) ----------------

pub fn answer(node: &Node, q: &Value) -> Result<Value, String> {
    match q["op"].as_str().unwrap_or("") {
        "xbook" => Ok(json!({"orders": list(node, "xorder").into_iter().filter(|o| o["status"] == "open").collect::<Vec<_>>()})),
        "xquote" => {
            let o = get(node, "xorder", q["order"].as_str().unwrap_or("")).filter(|o| o["status"] == "open").ok_or("that order is not open")?;
            let cents = q["cents"].as_i64().ok_or("give cents")?;
            if cents < 100 || cents % 100 != 0 || cents > o["left"].as_i64().unwrap_or(0) {
                return Err("whole dollars, at most what is left".into());
            }
            let pd = dd_chain(o["dd"].as_str().unwrap_or(""))?;
            let (_, pep_pub) = wallet_key(node, node.params)?;
            let (_, dd_pub) = wallet_key(node, pd)?;
            Ok(json!({"order": o["id"], "cents": cents, "price": price_of(&o, cents), "pay": node.params.key, "dd": pd.key,
                      "maker_pay_key": hex::encode(pep_pub.serialize()), "maker_dd_key": hex::encode(dd_pub.x_only_public_key().0.serialize()),
                      "taker_lock_hours": LOCK_TAKER / 3600, "maker_lock_hours": LOCK_MAKER / 3600}))
        }
        "xlocked" => maker_lock(node, q),
        _ => Err("unknown swap question".into()),
    }
}

/// The taker says its lock is on the chain: check it here, then lock the dollars.
fn maker_lock(node: &Node, q: &Value) -> Result<Value, String> {
    let hash_hex = q["hash"].as_str().unwrap_or("");
    if let Some(s) = get(node, "xswap", hash_hex) {
        return Ok(json!({"dd_lock": s["dd_lock"], "dd_time": s["dd_time"], "already": true}));
    }
    let mut o = get(node, "xorder", q["order"].as_str().unwrap_or("")).filter(|o| o["status"] == "open").ok_or("that order is not open")?;
    let cents = q["cents"].as_i64().ok_or("give cents")?;
    if cents < 100 || cents % 100 != 0 || cents > o["left"].as_i64().unwrap_or(0) {
        return Err("whole dollars, at most what is left".into());
    }
    let price = price_of(&o, cents);
    let pd = dd_chain(o["dd"].as_str().unwrap_or(""))?;
    let hash: [u8; 32] = hex::decode(hash_hex).ok().and_then(|b| b.try_into().ok()).ok_or("bad hash")?;
    let (_, my_pay) = wallet_key(node, node.params)?;
    let (dd_sk, my_dd) = wallet_key(node, pd)?;
    let taker_pay = hex::decode(q["taker_pay_key"].as_str().unwrap_or("")).map_err(|_| "bad key")?;
    let taker_dd = XOnlyPublicKey::from_slice(&hex::decode(q["taker_dd_key"].as_str().unwrap_or("")).map_err(|_| "bad key")?).map_err(|_| "bad key")?;
    let time = q["time"].as_u64().ok_or("give the lock time")? as u32;
    if time < now() + LEAST_LEFT {
        return Err("that lock ends too soon".into());
    }
    // The taker's lock, as this arcade computes it, read off its own copy of the chain.
    let lock = Htlc { hash, claimer: my_pay.serialize().to_vec(), refunder: taker_pay.clone(), time };
    let txid = Hash256::from_hex(q["pay_lock"].as_str().unwrap_or("")).map_err(|_| "bad txid")?;
    let tx = tx_of(node, node.params, &txid).ok_or("this arcade doesn't see that lock yet")?;
    let vout = tx.outputs.iter().position(|x| x.script_pubkey == lock.script()).ok_or("that transaction doesn't pay this lock")?;
    if tx.outputs[vout].value < price + htlc::SPEND_FEE {
        return Err(format!("the lock holds {} and the price is {} plus the claim's fee", tx.outputs[vout].value, price));
    }
    if depth(node, node.params, &txid) < 1 {
        return Err("wait: the lock is not in a block yet".into());
    }
    if spender(node, node.params, &OutPoint { txid, vout: vout as u32 }).is_some() {
        return Err("that lock is already spent".into());
    }
    // Lock the dollars: the taker claims with the same secret, this arcade refunds after LOCK_MAKER.
    let dd_time = now() + LOCK_MAKER;
    let dlock = Lock { hash, claimer: taker_dd, refunder: my_dd.x_only_public_key().0, time: dd_time };
    let (_, held, dgb, _, change) = crate::ddsvc::trade_coins(node, pd)?;
    let dtx = ddlock::build_lock(pd, &dd_sk, &held, &dgb, &change, &dlock, cents)?;
    let dd_txid = broadcast(node, pd, &dtx.raw)?;
    o["left"] = json!(o["left"].as_i64().unwrap_or(0) - cents);
    if o["left"] == 0 {
        o["status"] = json!("filled");
    }
    put(node, "xorder", &o);
    let s = json!({"id": hash_hex, "role": "maker", "order": o["id"], "cents": cents, "price": price, "dd": pd.key, "pay": node.params.key,
                   "pay_lock": txid.to_hex(), "pay_vout": vout, "pay_value": tx.outputs[vout].value, "pay_time": time,
                   "taker_pay_key": hex::encode(&taker_pay), "taker_dd_key": hex::encode(taker_dd.serialize()),
                   "dd_lock": dd_txid, "dd_time": dd_time, "step": "dd_locked", "at": arcade_app::now()});
    put(node, "xswap", &s);
    let _ = node.app.event("wallet", &format!("swap {}: ${:.2} locked for {} (dd {dd_txid})", &hash_hex[..12], cents as f64 / 100.0, price));
    Ok(json!({"dd_lock": dd_txid, "dd_time": dd_time}))
}

// ---------------- the taker ----------------

fn ask(node: &Node, maker: &str, q: Value) -> Result<Value, String> {
    if maker == node.id_hex() {
        return answer(node, &q);
    }
    crate::shares::ask_linked(maker, json!({"what": "mk", "net": node.params.net, "arg": q.to_string()}))
}

/// Start a swap: take `cents` of the maker's order, paying with this arcade's home coin. Locks the price now; the
/// rest runs by itself (tick).
pub fn take(node: &Node, maker: &str, order: &str, cents: i64) -> Result<Value, String> {
    let quote = ask(node, maker, json!({"op": "xquote", "chain": "dgbtest", "order": order, "cents": cents}))?;
    let pd = dd_chain(quote["dd"].as_str().unwrap_or(""))?;
    if quote["pay"] != node.params.key {
        return Err("that order is paid on another chain".into());
    }
    let price = quote["price"].as_u64().ok_or("no price")?;
    let maker_pay = hex::decode(quote["maker_pay_key"].as_str().unwrap_or("")).map_err(|_| "bad key")?;
    let maker_dd = quote["maker_dd_key"].as_str().unwrap_or("").to_string();
    let secret: [u8; 32] = arcade_mesh::random::bytes();
    let hash: [u8; 32] = Sha256::digest(secret).into();
    let (pay_sk, pay_pub) = wallet_key(node, node.params)?;
    let (_, dd_pub) = wallet_key(node, pd)?;
    let time = now() + LOCK_TAKER;
    let lock = Htlc { hash, claimer: maker_pay, refunder: pay_pub.serialize().to_vec(), time };
    // Lock the price (and the claim's fee) from this wallet's coins.
    let from_script = arcade_ledger::address::p2pkh_script(&arcade_ledger::address::hash160(&pay_pub.serialize()));
    let coins = crate::claims::spendable(node, &from_script);
    let tx = arcade_wallet::tx::build_send(node.params, &lock.script(), price + htlc::SPEND_FEE, &pay_sk, &coins, crate::claims::FEE_PER_KB, crate::claims::DUST, now()).map_err(|e| e.to_string())?;
    if !crate::mempoolshare::add(node, node.params, &tx) {
        return Err("the lock was refused: its coins are spent by another waiting transaction".into());
    }
    crate::claims::mark_spent(node, &tx);
    node.tx_outbox.lock().unwrap().push(tx.raw.clone());
    let vout = tx.outputs.iter().position(|o| o.script_pubkey == lock.script()).unwrap_or(0);
    let id = hex::encode(hash);
    let s = json!({"id": id, "role": "taker", "maker": maker, "order": order, "cents": cents, "price": price, "dd": pd.key, "pay": node.params.key,
                   "secret": hex::encode(secret), "pay_lock": tx.txid.to_hex(), "pay_vout": vout, "pay_value": price + htlc::SPEND_FEE, "pay_time": time,
                   "maker_pay_key": quote["maker_pay_key"], "maker_dd_key": maker_dd, "taker_pay_key": hex::encode(pay_pub.serialize()),
                   "taker_dd_key": hex::encode(dd_pub.x_only_public_key().0.serialize()), "step": "pay_locked", "at": arcade_app::now()});
    put(node, "xswap", &s);
    let _ = node.app.event("wallet", &format!("swap {}: {} locked for ${:.2} ({})", &id[..12], price, cents as f64 / 100.0, tx.txid.to_hex()));
    Ok(public(&s))
}

/// What a swap shows (never its secret).
pub fn public(s: &Value) -> Value {
    let mut v = s.clone();
    if let Some(m) = v.as_object_mut() {
        m.remove("secret");
    }
    v
}

pub fn swaps(node: &Node) -> Vec<Value> {
    list(node, "xswap").iter().map(public).collect()
}

// ---------------- moving every swap along ----------------

fn step(node: &Node, s: &mut Value) -> Result<(), String> {
    let pd = dd_chain(s["dd"].as_str().unwrap_or(""))?;
    let hash: [u8; 32] = hex::decode(s["id"].as_str().unwrap_or("")).ok().and_then(|b| b.try_into().ok()).ok_or("bad id")?;
    // The price's lock: the maker's key claims it (the maker's own key on its side, the quoted one on the taker's).
    let maker_pay = if s["role"] == "maker" {
        wallet_key(node, node.params)?.1.serialize().to_vec()
    } else {
        hex::decode(s["maker_pay_key"].as_str().unwrap_or("")).map_err(|_| "bad key")?
    };
    let pay_lock = Htlc {
        hash,
        claimer: maker_pay,
        refunder: hex::decode(s["taker_pay_key"].as_str().unwrap_or("")).unwrap_or_default(),
        time: s["pay_time"].as_u64().unwrap_or(0) as u32,
    };
    let pay_op = OutPoint { txid: Hash256::from_hex(s["pay_lock"].as_str().unwrap_or("")).map_err(|_| "bad txid")?, vout: s["pay_vout"].as_u64().unwrap_or(0) as u32 };
    let role = s["role"].as_str().unwrap_or("").to_string();
    let st = s["step"].as_str().unwrap_or("").to_string();
    if role == "taker" {
        // The price comes back after the taker's own time if the swap never finished.
        if matches!(st.as_str(), "pay_locked" | "dd_locked") && now() > pay_lock.time + 600 && spender(node, node.params, &pay_op).is_none() {
            let (sk, pk) = wallet_key(node, node.params)?;
            let to = arcade_ledger::address::p2pkh_script(&arcade_ledger::address::hash160(&pk.serialize()));
            let outs = htlc::pay_out(&[(pay_op, s["pay_value"].as_u64().unwrap_or(0))], &to, htlc::SPEND_FEE)?;
            let raw = htlc::refund(node.params, &pay_lock, &[(pay_op, s["pay_value"].as_u64().unwrap_or(0))], outs, &sk)?;
            s["refund_tx"] = json!(broadcast(node, node.params, &raw)?);
            s["step"] = json!("refunded");
            return Ok(());
        }
        match st.as_str() {
            "pay_locked" => {
                if depth(node, node.params, &pay_op.txid) < 1 {
                    return Ok(());
                }
                let q = json!({"op": "xlocked", "chain": "dgbtest", "order": s["order"], "cents": s["cents"], "hash": s["id"], "pay_lock": s["pay_lock"],
                               "time": s["pay_time"], "taker_pay_key": s["taker_pay_key"], "taker_dd_key": s["taker_dd_key"]});
                match ask(node, s["maker"].as_str().unwrap_or(""), q) {
                    Ok(a) => {
                        s["dd_lock"] = a["dd_lock"].clone();
                        s["dd_time"] = a["dd_time"].clone();
                        s["step"] = json!("dd_locked");
                    }
                    Err(e) => s["last"] = json!(e),
                }
            }
            "dd_locked" => {
                let dtxid = Hash256::from_hex(s["dd_lock"].as_str().unwrap_or("")).map_err(|_| "bad txid")?;
                if depth(node, pd, &dtxid) < 1 {
                    return Ok(());
                }
                // The dollar lock as this arcade computes it: its own key claims, the maker's refunds.
                let dlock = Lock {
                    hash,
                    claimer: XOnlyPublicKey::from_slice(&hex::decode(s["taker_dd_key"].as_str().unwrap_or("")).unwrap_or_default()).map_err(|_| "bad key")?,
                    refunder: XOnlyPublicKey::from_slice(&hex::decode(s["maker_dd_key"].as_str().unwrap_or("")).unwrap_or_default()).map_err(|_| "bad key")?,
                    time: s["dd_time"].as_u64().unwrap_or(0) as u32,
                };
                let dtx = tx_of(node, pd, &dtxid).ok_or("the dollar lock isn't here yet")?;
                let rec = arcade_ledger::digidollar::read(&dtx).ok_or("that is not a DigiDollar transaction")?;
                let cents = s["cents"].as_i64().unwrap_or(0);
                let paid: i64 = rec.outputs.iter().filter(|o| dtx.outputs.get(o.0 as usize).is_some_and(|x| x.script_pubkey == dlock.script())).map(|o| o.2).sum();
                if paid != cents {
                    s["last"] = json!(format!("the dollar lock holds {paid} cents, not {cents}: not claimed"));
                    return Ok(());
                }
                let dv = rec.outputs.iter().find(|o| dtx.outputs.get(o.0 as usize).is_some_and(|x| x.script_pubkey == dlock.script())).map(|o| o.0).unwrap_or(0);
                let rv = dtx.outputs.iter().position(|x| x.value > 0 && x.script_pubkey == dlock.script()).ok_or("the lock has no reserve")? as u32;
                if now() + 1800 > dlock.time {
                    s["last"] = json!("the dollar lock ends too soon to claim safely");
                    return Ok(());
                }
                let secret = hex::decode(s["secret"].as_str().unwrap_or("")).map_err(|_| "no secret")?;
                let secret: [u8; 32] = secret.try_into().map_err(|_| "bad secret")?;
                let (dsk, dpk) = wallet_key(node, pd)?;
                let to = arcade_wallet::taproot::p2tr_script(&arcade_wallet::taproot::output_key(&dpk).ok_or("tweak")?);
                let dgb_to = arcade_ledger::address::p2wpkh_script(&arcade_ledger::address::hash160(&dpk.serialize()));
                let dollars = arcade_wallet::tx::Utxo { outpoint: OutPoint { txid: dtxid, vout: dv }, value: 0, script: dlock.script() };
                let reserve = arcade_wallet::tx::Utxo { outpoint: OutPoint { txid: dtxid, vout: rv }, value: dtx.outputs[rv as usize].value, script: dlock.script() };
                let ctx = ddlock::build_spend(pd, &dsk, &dlock, &dollars, cents, &reserve, &to, &dgb_to, Some(&secret))?;
                s["claim_tx"] = json!(broadcast(node, pd, &ctx.raw)?);
                s["step"] = json!("claimed");
            }
            _ => {}
        }
        return Ok(());
    }
    // The maker.
    if st != "dd_locked" {
        return Ok(());
    }
    let dtxid = Hash256::from_hex(s["dd_lock"].as_str().unwrap_or("")).map_err(|_| "bad txid")?;
    let (_, my_dd) = wallet_key(node, pd)?;
    let dlock = Lock {
        hash,
        claimer: XOnlyPublicKey::from_slice(&hex::decode(s["taker_dd_key"].as_str().unwrap_or("")).unwrap_or_default()).map_err(|_| "bad key")?,
        refunder: my_dd.x_only_public_key().0,
        time: s["dd_time"].as_u64().unwrap_or(0) as u32,
    };
    let dtx = tx_of(node, pd, &dtxid).ok_or("the dollar lock isn't here")?;
    let dv = dtx.outputs.iter().position(|x| x.value == 0 && x.script_pubkey == dlock.script()).ok_or("no dollar output")? as u32;
    let rv = dtx.outputs.iter().position(|x| x.value > 0 && x.script_pubkey == dlock.script()).ok_or("no reserve")? as u32;
    if let Some(spent) = spender(node, pd, &OutPoint { txid: dtxid, vout: dv }) {
        // The taker claimed: its witness carries the secret, believed only when it opens the hash.
        let secret = spent.inputs.iter().filter(|i| i.prevout == OutPoint { txid: dtxid, vout: dv }).flat_map(|i| i.witness.iter()).find(|w| <[u8; 32]>::from(Sha256::digest(w)) == hash).cloned();
        let Some(secret) = secret else {
            s["step"] = json!("dd_refunded");
            return Ok(());
        };
        let (sk, pk) = wallet_key(node, node.params)?;
        let to = arcade_ledger::address::p2pkh_script(&arcade_ledger::address::hash160(&pk.serialize()));
        let value = s["pay_value"].as_u64().unwrap_or(0);
        let outs = htlc::pay_out(&[(pay_op, value)], &to, htlc::SPEND_FEE)?;
        let raw = htlc::claim(node.params, &pay_lock, &[(pay_op, value)], outs, &secret, &sk)?;
        s["claim_tx"] = json!(broadcast(node, node.params, &raw)?);
        s["step"] = json!("claimed");
        let _ = node.app.event("wallet", &format!("swap {}: secret read off the dollar claim; price claimed", &s["id"].as_str().unwrap_or("")[..12]));
    } else if now() > dlock.time + 600 {
        let (dsk, dpk) = wallet_key(node, pd)?;
        let to = arcade_wallet::taproot::p2tr_script(&arcade_wallet::taproot::output_key(&dpk).ok_or("tweak")?);
        let dgb_to = arcade_ledger::address::p2wpkh_script(&arcade_ledger::address::hash160(&dpk.serialize()));
        let dollars = arcade_wallet::tx::Utxo { outpoint: OutPoint { txid: dtxid, vout: dv }, value: 0, script: dlock.script() };
        let reserve = arcade_wallet::tx::Utxo { outpoint: OutPoint { txid: dtxid, vout: rv }, value: dtx.outputs[rv as usize].value, script: dlock.script() };
        let rtx = ddlock::build_spend(pd, &dsk, &dlock, &dollars, s["cents"].as_i64().unwrap_or(0), &reserve, &to, &dgb_to, None)?;
        s["refund_tx"] = json!(broadcast(node, pd, &rtx.raw)?);
        s["step"] = json!("refunded");
    }
    Ok(())
}

/// Every 15 s: move each running swap one step if it can.
pub fn start(node: Arc<Node>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(15));
        for mut s in list(&node, "xswap") {
            if matches!(s["step"].as_str(), Some("claimed" | "refunded" | "dd_refunded" | "failed")) {
                continue;
            }
            let before = s.clone();
            if let Err(e) = step(&node, &mut s) {
                s["last"] = json!(e);
            }
            if s != before {
                put(&node, "xswap", &s);
            }
        }
    });
}
