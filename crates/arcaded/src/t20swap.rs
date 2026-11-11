//! A PRC-20 / DRC-20 token on this arcade's home chain for DGB, by hash-and-time locks (docs/MARKET.md step 7),
//! V1.0 with V1.0. The maker sells the token; the taker pays DGB.
//!
//! A transfer note credits its tokens on its first move, so it can't be sent into a lock (the original's
//! token20_swap.py, the same three steps): the maker inscribes note 1 (the amount) on its own address; one
//! transaction moves note 1 to the lock (the lock's address now holds the tokens) and pays note 2's commit; note 2's
//! reveal, the same amount, lands on the lock itself, its coin output 0 and a reserve output 1. The claim spends both
//! with the secret, note 2's coin whole to the claimer as output 0 (its first move: the tokens to the claimer).
//!
//!   1. The taker asks the maker's arcade for its terms and keys (`tquote`), makes a secret and locks the price in
//!      DGB for LOCK_TAKER (htlc.rs on the DigiByte chain).
//!   2. It tells the maker (`tlocked`). The maker's arcade checks that lock on its own copy of DigiByte, then walks
//!      the three token steps, a block between each (its tick).
//!   3. The taker's arcade asks for the token lock (`tstatus`) and counts it only when its own token index reads note
//!      2 as a live note of exactly the tick and amount, owned by the lock it computes; then it claims it.
//!   4. The maker's arcade reads the secret off that claim and claims the DGB. Each refunds after its own time.

use crate::node::Node;
use arcade_chain::params::{by_key, ChainParams};
use arcade_chain::{Hash256, OutPoint, Tx};
use arcade_wallet::htlc::{self, Htlc};
use arcade_wallet::secp256k1::{PublicKey, SECP256K1};
use arcade_wallet::tx::{TxOutSpec, Utxo};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

const LOCK_TAKER: u32 = 8 * 3600;
const LOCK_MAKER: u32 = 4 * 3600;
const LEAST_LEFT: u32 = 6 * 3600;
/// A note's coin (token20_send.POSTAGE), and the reserve beside it in the lock (htlc.DOGINAL_RESERVE).
const POSTAGE: u64 = 100_000;
const RESERVE: u64 = htlc::SPEND_FEE + htlc::DUST;
/// What each of the maker's token steps leaves the network (soft-dust outputs pay their own 0.01).
const STEP_FEE: u64 = 3_000_000;
const CTYPE: &str = "text/plain;charset=utf-8";

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
    node.app.store.read(|r| r.scan_prefix(arcade_app::META, format!("{kind}:").as_bytes())).unwrap_or_default().into_iter().filter_map(|(_, v)| serde_json::from_slice(&v).ok()).collect()
}
fn dgb(k: &str) -> Result<&'static ChainParams, String> {
    by_key(k).filter(|p| matches!(p.key, "dgb" | "dgbtest")).ok_or_else(|| "DGB is on dgb or dgbtest".to_string())
}
fn wallet_key(node: &Node, p: &ChainParams) -> Result<(arcade_wallet::SecretKey, PublicKey), String> {
    let sk = node.app.tag_key(p, 0).map_err(|e| e.to_string())?.1;
    Ok((sk, PublicKey::from_secret_key(SECP256K1, &sk)))
}
fn p2pkh_of(pk: &PublicKey) -> Vec<u8> {
    arcade_ledger::address::p2pkh_script(&arcade_ledger::address::hash160(&pk.serialize()))
}
fn depth(node: &Node, p: &ChainParams, txid: &Hash256) -> u32 {
    let Ok(Some((b, _))) = node.chain.tx_location(p, txid) else { return 0 };
    let Some((_, h, _)) = node.chain.block_at(p, b).ok().flatten() else { return 0 };
    node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height).unwrap_or(0).saturating_sub(h) + 1
}
fn tx_of(node: &Node, p: &ChainParams, txid: &Hash256) -> Option<Tx> {
    if let Ok(Some((raw, _))) = node.chain.tx_raw(p, txid) {
        return Tx::parse_with(&raw, &p.format).ok();
    }
    node.chain.mempool(p.chain_no).ok()?.into_iter().filter_map(|(_, raw)| Tx::parse_with(&raw, &p.format).ok()).find(|t| &t.txid == txid)
}
fn spender(node: &Node, p: &ChainParams, op: &OutPoint) -> Option<Tx> {
    if let Ok(Some((b, pos, _))) = node.chain.spender(p, op) {
        return tx_of(node, p, &node.chain.txid_at(p, b, pos).ok().flatten()?);
    }
    tx_of(node, p, &node.chain.mempool_spender(p.chain_no, op).ok().flatten()?)
}
fn broadcast(node: &Node, p: &ChainParams, raw: &[u8]) -> Result<String, String> {
    let v = crate::sitesvc::broadcast(node, &json!({"hex": hex::encode(raw), "chain": p.key}))?;
    Ok(v["txid"].as_str().unwrap_or("").to_string())
}
fn hash_of(s: &Value) -> Result<[u8; 32], String> {
    hex::decode(s["id"].as_str().unwrap_or("")).ok().and_then(|b| b.try_into().ok()).ok_or_else(|| "bad id".to_string())
}

/// This chain's token protocol ("prc-20", "drc-20"), or an error.
fn protocol(node: &Node) -> Result<&'static str, String> {
    arcade_ledger::tokens::protocol(node.params).map(|x| x.0).ok_or_else(|| format!("{} has no DRC-20 / PRC-20", node.params.name))
}

// ---------------- the maker's orders and answers ----------------

/// Sell `amount` (whole tokens, a decimal string) of `tick` for `price` DGB satoshis on `dgb`.
pub fn place(node: &Node, tick: &str, amount: &str, dgb_key: &str, price: u64) -> Result<Value, String> {
    let proto = protocol(node)?;
    let pd = dgb(dgb_key)?;
    let tick = tick.to_lowercase();
    arcade_ledger::tokens::tick(&node.chain, node.params.chain_no, &tick).ok_or("no such token here")?;
    let (_, pk) = wallet_key(node, node.params)?;
    let me = arcade_ledger::address::base58check(node.params.p2pkh_version, &arcade_ledger::address::hash160(&pk.serialize()));
    let held = arcade_ledger::tokens::balances(&node.chain, node.params.chain_no, &me).into_iter().find(|(t, ..)| t == &tick);
    if held.is_none() {
        return Err(format!("this wallet holds no {tick}"));
    }
    let id = arcade_mesh::random::hex(8);
    let o = json!({"id": id, "proto": proto, "tick": tick, "amount": amount, "chain": node.params.key, "dgb": pd.key, "price": price,
                   "status": "open", "created": arcade_app::now(), "node": node.id_hex()});
    put(node, "torder", &o);
    Ok(o)
}

pub fn book(node: &Node) -> Vec<Value> {
    let mut all: Vec<Value> = list(node, "torder").into_iter().filter(|o| o["status"] == "open").collect();
    for peer in crate::shares::linked_ids(node) {
        if let Ok(a) = crate::shares::ask_linked(&peer, json!({"what": "mk", "net": node.params.net, "arg": json!({"op": "tbook", "chain": "dgbtest"}).to_string()})) {
            for mut o in a["orders"].as_array().cloned().unwrap_or_default() {
                o["node"] = json!(peer);
                all.push(o);
            }
        }
    }
    all
}

pub fn answer(node: &Node, q: &Value) -> Result<Value, String> {
    match q["op"].as_str().unwrap_or("") {
        "tbook" => Ok(json!({"orders": list(node, "torder").into_iter().filter(|o| o["status"] == "open").collect::<Vec<_>>()})),
        "tquote" => {
            let o = get(node, "torder", q["order"].as_str().unwrap_or("")).filter(|o| o["status"] == "open").ok_or("that order is not open")?;
            let pd = dgb(o["dgb"].as_str().unwrap_or(""))?;
            let (_, home) = wallet_key(node, node.params)?;
            let (_, d) = wallet_key(node, pd)?;
            Ok(json!({"order": o["id"], "tick": o["tick"], "amount": o["amount"], "price": o["price"], "dgb": pd.key, "chain": node.params.key,
                      "maker_token_key": hex::encode(home.serialize()), "maker_dgb_key": hex::encode(d.serialize())}))
        }
        "tlocked" => maker_start(node, q),
        "tstatus" => {
            let s = get(node, "tswap", q["hash"].as_str().unwrap_or("")).ok_or("no such swap here")?;
            Ok(json!({"step": s["step"], "note2": s["note2"], "lock_time": s["lock_time"], "last": s["last"]}))
        }
        _ => Err("unknown token swap question".into()),
    }
}

fn token_lock(s: &Value, node: &Node) -> Result<Htlc, String> {
    Ok(Htlc {
        hash: hash_of(s)?,
        claimer: hex::decode(s["taker_token_key"].as_str().unwrap_or("")).map_err(|_| "bad key")?,
        refunder: if s["role"] == "maker" { wallet_key(node, node.params)?.1.serialize().to_vec() } else { hex::decode(s["maker_token_key"].as_str().unwrap_or("")).map_err(|_| "bad key")? },
        time: s["lock_time"].as_u64().unwrap_or(0) as u32,
    })
}

fn dgb_lock(s: &Value, node: &Node, pd: &ChainParams) -> Result<Htlc, String> {
    Ok(Htlc {
        hash: hash_of(s)?,
        claimer: if s["role"] == "maker" { wallet_key(node, pd)?.1.serialize().to_vec() } else { hex::decode(s["maker_dgb_key"].as_str().unwrap_or("")).map_err(|_| "bad key")? },
        refunder: hex::decode(s["taker_dgb_key"].as_str().unwrap_or("")).map_err(|_| "bad key")?,
        time: s["pay_time"].as_u64().unwrap_or(0) as u32,
    })
}

/// The taker's DGB lock is on the chain: check it on this arcade's copy, then start the token steps.
fn maker_start(node: &Node, q: &Value) -> Result<Value, String> {
    let hash_hex = q["hash"].as_str().unwrap_or("");
    if let Some(s) = get(node, "tswap", hash_hex) {
        return Ok(json!({"step": s["step"]}));
    }
    let mut o = get(node, "torder", q["order"].as_str().unwrap_or("")).filter(|o| o["status"] == "open").ok_or("that order is not open")?;
    let pd = dgb(o["dgb"].as_str().unwrap_or(""))?;
    let time = q["time"].as_u64().ok_or("give the lock time")? as u32;
    if time < now() + LEAST_LEFT {
        return Err("that lock ends too soon".into());
    }
    let mut s = json!({"id": hash_hex, "role": "maker", "order": o["id"], "tick": o["tick"], "amount": o["amount"], "price": o["price"], "dgb": pd.key,
                       "pay_lock": q["pay_lock"], "pay_time": time, "taker_dgb_key": q["taker_dgb_key"], "taker_token_key": q["taker_token_key"],
                       "lock_time": now() + LOCK_MAKER, "step": "start", "at": arcade_app::now()});
    let lock = dgb_lock(&s, node, pd)?;
    let txid = Hash256::from_hex(q["pay_lock"].as_str().unwrap_or("")).map_err(|_| "bad txid")?;
    let tx = tx_of(node, pd, &txid).ok_or("this arcade doesn't see that lock yet")?;
    let vout = tx.outputs.iter().position(|x| x.script_pubkey == lock.script()).ok_or("that transaction doesn't pay this lock")?;
    let price = o["price"].as_u64().unwrap_or(0);
    if tx.outputs[vout].value < price + htlc::SPEND_FEE {
        return Err("the DGB lock holds less than the price and its claim's fee".into());
    }
    if depth(node, pd, &txid) < 1 {
        return Err("wait: the DGB lock is not in a block yet".into());
    }
    s["pay_vout"] = json!(vout);
    s["pay_value"] = json!(tx.outputs[vout].value);
    o["status"] = json!("filled");
    put(node, "torder", &o);
    put(node, "tswap", &s);
    Ok(json!({"step": "start"}))
}

// ---------------- the taker ----------------

fn ask(node: &Node, maker: &str, q: Value) -> Result<Value, String> {
    if maker == node.id_hex() {
        return answer(node, &q);
    }
    crate::shares::ask_linked(maker, json!({"what": "mk", "net": node.params.net, "arg": q.to_string()}))
}

/// Take the maker's whole order: lock the price in DGB now; the rest runs by itself.
pub fn take(node: &Node, maker: &str, order: &str) -> Result<Value, String> {
    let quote = ask(node, maker, json!({"op": "tquote", "chain": "dgbtest", "order": order}))?;
    if quote["chain"] != node.params.key {
        return Err("that token is on another chain".into());
    }
    let pd = dgb(quote["dgb"].as_str().unwrap_or(""))?;
    let price = quote["price"].as_u64().ok_or("no price")?;
    let secret: [u8; 32] = arcade_mesh::random::bytes();
    let hash: [u8; 32] = Sha256::digest(secret).into();
    let (dsk, dpk) = wallet_key(node, pd)?;
    let (_, hpk) = wallet_key(node, node.params)?;
    let time = now() + LOCK_TAKER;
    let lock = Htlc { hash, claimer: hex::decode(quote["maker_dgb_key"].as_str().unwrap_or("")).map_err(|_| "bad key")?, refunder: dpk.serialize().to_vec(), time };
    let (_, _, coins, _, change) = crate::ddsvc::trade_coins(node, pd)?;
    let fee = crate::coinsend::fee_policy(pd);
    let tx = arcade_wallet::tx::build_send_keyhash(pd, &lock.script(), price + htlc::SPEND_FEE, &dsk, &coins, &change, fee.fallback_per_kb.max(fee.floor_per_kb), fee.dust, now())?;
    let txid = broadcast(node, pd, &tx.raw)?;
    let vout = tx.outputs.iter().position(|o| o.script_pubkey == lock.script()).unwrap_or(0);
    let id = hex::encode(hash);
    let s = json!({"id": id, "role": "taker", "maker": maker, "order": order, "tick": quote["tick"], "amount": quote["amount"], "price": price,
                   "dgb": pd.key, "secret": hex::encode(secret), "pay_lock": txid, "pay_vout": vout, "pay_value": price + htlc::SPEND_FEE, "pay_time": time,
                   "maker_dgb_key": quote["maker_dgb_key"], "maker_token_key": quote["maker_token_key"], "taker_dgb_key": hex::encode(dpk.serialize()),
                   "taker_token_key": hex::encode(hpk.serialize()), "step": "pay_locked", "at": arcade_app::now()});
    put(node, "tswap", &s);
    Ok(public(&s))
}

pub fn public(s: &Value) -> Value {
    let mut v = s.clone();
    if let Some(m) = v.as_object_mut() {
        m.remove("secret");
    }
    v
}

pub fn swaps(node: &Node) -> Vec<Value> {
    list(node, "tswap").iter().map(public).collect()
}

// ---------------- moving every swap along ----------------

/// The home chain's coins of this wallet, none spent in the mempool, apart from `keep`.
fn home_coins(node: &Node, pk: &PublicKey, keep: &[OutPoint]) -> Vec<Utxo> {
    crate::claims::spendable(node, &p2pkh_of(pk)).into_iter().filter(|u| !keep.contains(&u.outpoint)).collect()
}

fn send_home(node: &Node, tx: &Tx) -> Result<String, String> {
    if !crate::mempoolshare::add(node, node.params, tx) {
        return Err("refused: its coins are spent by another waiting transaction".into());
    }
    crate::claims::mark_spent(node, tx);
    node.tx_outbox.lock().unwrap().push(tx.raw.clone());
    Ok(tx.txid.to_hex())
}

fn maker_step(node: &Node, s: &mut Value) -> Result<(), String> {
    let p = node.params;
    let pd = dgb(s["dgb"].as_str().unwrap_or(""))?;
    let (sk, pk) = wallet_key(node, p)?;
    let own = p2pkh_of(&pk);
    let tick = s["tick"].as_str().unwrap_or("").to_string();
    let amount = s["amount"].as_str().unwrap_or("").to_string();
    let proto = protocol(node)?;
    let note = format!(r#"{{"p":"{proto}","op":"transfer","tick":"{tick}","amt":"{amount}"}}"#);
    let tlock = token_lock(s, node)?;
    let t = now();
    let ready = |key: &str| -> bool { s[key].as_str().and_then(|x| Hash256::from_hex(x).ok()).is_some_and(|h| depth(node, p, &h) >= 1) };
    match s["step"].as_str().unwrap_or("") {
        "start" => {
            // Note 1 on this wallet's own address.
            let coins = home_coins(node, &pk, &[]);
            let i = arcade_wallet::ordwrite::inscribe(p, &sk, &coins, CTYPE, note.as_bytes(), &own, POSTAGE, STEP_FEE, crate::claims::FEE_PER_KB, crate::claims::DUST, t)?;
            send_home(node, &i.commit)?;
            let rtx = Tx::parse_with(&i.reveal, &p.format).map_err(|e| e.to_string())?;
            s["note1"] = json!(send_home(node, &rtx)?);
            s["step"] = json!("note1");
        }
        "note1" if ready("note1") => {
            // Fund: note 1's coin to the lock (input 0 to output 0: its first move), and note 2's commit.
            let note1 = OutPoint { txid: Hash256::from_hex(s["note1"].as_str().unwrap_or("")).map_err(|_| "bad txid")?, vout: 0 };
            let (commit_script, _) = arcade_wallet::ordwrite::commit_lock(&pk, CTYPE, note.as_bytes());
            let commit_value = POSTAGE + RESERVE + STEP_FEE;
            let mut ins = vec![Utxo { outpoint: note1, value: POSTAGE, script: own.clone() }];
            let mut have = POSTAGE;
            let need = POSTAGE + commit_value + STEP_FEE;
            for c in home_coins(node, &pk, &[note1]) {
                if have >= need + crate::claims::DUST {
                    break;
                }
                have += c.value;
                ins.push(c);
            }
            if have < need {
                return Err("not enough coins to fund the token lock".into());
            }
            let mut outs = vec![TxOutSpec { value: POSTAGE, script: tlock.script() }, TxOutSpec { value: commit_value, script: commit_script }];
            if have - need >= crate::claims::DUST {
                outs.push(TxOutSpec { value: have - need, script: own.clone() });
            }
            let tx = arcade_wallet::tx::sign_p2pkh(p, &ins, &outs, &sk, t);
            s["fund"] = json!(send_home(node, &tx)?);
            s["step"] = json!("funded");
        }
        "funded" if ready("fund") => {
            // Note 2's reveal, onto the lock: its coin output 0, the reserve output 1.
            let commit = OutPoint { txid: Hash256::from_hex(s["fund"].as_str().unwrap_or("")).map_err(|_| "bad txid")?, vout: 1 };
            let raw = arcade_wallet::ordwrite::reveal(p, &sk, commit, CTYPE, note.as_bytes(), vec![(POSTAGE, tlock.script()), (RESERVE, tlock.script())], t)?;
            let tx = Tx::parse_with(&raw, &p.format).map_err(|e| e.to_string())?;
            s["note2"] = json!(send_home(node, &tx)?);
            s["step"] = json!("locked");
        }
        "locked" => {
            let n2 = Hash256::from_hex(s["note2"].as_str().unwrap_or("")).map_err(|_| "bad txid")?;
            let lock0 = OutPoint { txid: n2, vout: 0 };
            if let Some(spent) = spender(node, p, &lock0) {
                // The taker claimed: the secret is in its input script.
                let secret = spent.inputs.iter().filter(|i| i.prevout == lock0).find_map(|i| htlc::secret_from_script_sig(&i.script_sig, &tlock.redeem()));
                let Some(secret) = secret else {
                    s["step"] = json!("refunded");
                    return Ok(());
                };
                let dl = dgb_lock(s, node, pd)?;
                let (dsk, dpk) = wallet_key(node, pd)?;
                let op = OutPoint { txid: Hash256::from_hex(s["pay_lock"].as_str().unwrap_or("")).map_err(|_| "bad txid")?, vout: s["pay_vout"].as_u64().unwrap_or(0) as u32 };
                let value = s["pay_value"].as_u64().unwrap_or(0);
                let ptx = tx_of(node, pd, &op.txid).ok_or("the DGB lock isn't here")?;
                if ptx.outputs.get(op.vout as usize).map(|o| &o.script_pubkey) != Some(&dl.script()) {
                    return Err("that coin is not held by the DGB lock as computed here: nothing sent".into());
                }
                let to = arcade_ledger::address::p2wpkh_script(&arcade_ledger::address::hash160(&dpk.serialize()));
                let outs = htlc::pay_out(&[(op, value)], &to, htlc::SPEND_FEE)?;
                let raw = htlc::claim(pd, &dl, &[(op, value)], outs, &secret, &dsk)?;
                s["claim_tx"] = json!(broadcast(node, pd, &raw)?);
                s["step"] = json!("claimed");
            } else if t > tlock.time + 600 {
                let lock1 = OutPoint { txid: n2, vout: 1 };
                let raw = htlc::refund(p, &tlock, &[(lock0, POSTAGE), (lock1, RESERVE)], vec![(POSTAGE, own)], &sk)?;
                s["refund_tx"] = json!(send_home(node, &Tx::parse_with(&raw, &p.format).map_err(|e| e.to_string())?)?);
                s["step"] = json!("refunded");
            }
        }
        _ => {}
    }
    Ok(())
}

fn taker_step(node: &Node, s: &mut Value) -> Result<(), String> {
    let p = node.params;
    let pd = dgb(s["dgb"].as_str().unwrap_or(""))?;
    let dl = dgb_lock(s, node, pd)?;
    let pay = OutPoint { txid: Hash256::from_hex(s["pay_lock"].as_str().unwrap_or("")).map_err(|_| "bad txid")?, vout: s["pay_vout"].as_u64().unwrap_or(0) as u32 };
    if s["step"] != "claimed" && now() > dl.time + 600 && spender(node, pd, &pay).is_none() {
        let (dsk, dpk) = wallet_key(node, pd)?;
        let to = arcade_ledger::address::p2wpkh_script(&arcade_ledger::address::hash160(&dpk.serialize()));
        let value = s["pay_value"].as_u64().unwrap_or(0);
        let raw = htlc::refund(pd, &dl, &[(pay, value)], htlc::pay_out(&[(pay, value)], &to, htlc::SPEND_FEE)?, &dsk)?;
        s["refund_tx"] = json!(broadcast(node, pd, &raw)?);
        s["step"] = json!("refunded");
        return Ok(());
    }
    match s["step"].as_str().unwrap_or("") {
        "pay_locked" => {
            if depth(node, pd, &pay.txid) < 1 {
                return Ok(());
            }
            let q = json!({"op": "tlocked", "chain": "dgbtest", "order": s["order"], "hash": s["id"], "pay_lock": s["pay_lock"], "time": s["pay_time"],
                           "taker_dgb_key": s["taker_dgb_key"], "taker_token_key": s["taker_token_key"]});
            match ask(node, s["maker"].as_str().unwrap_or(""), q) {
                Ok(_) => s["step"] = json!("waiting"),
                Err(e) => s["last"] = json!(e),
            }
        }
        "waiting" => {
            let a = ask(node, s["maker"].as_str().unwrap_or(""), json!({"op": "tstatus", "chain": "dgbtest", "hash": s["id"]}))?;
            if a["step"] != "locked" || a["note2"].is_null() {
                return Ok(());
            }
            s["note2"] = a["note2"].clone();
            s["lock_time"] = a["lock_time"].clone();
            let tlock = token_lock(s, node)?;
            if tlock.time < now() + 1800 {
                s["last"] = json!("the token lock ends too soon to claim safely");
                return Ok(());
            }
            // Counted only when this arcade's own token index reads note 2 as a live note of exactly the tick and
            // amount, owned by the lock it computes itself.
            let n2 = Hash256::from_hex(s["note2"].as_str().unwrap_or("")).map_err(|_| "bad txid")?;
            if depth(node, p, &n2) < 1 {
                return Ok(());
            }
            let lock_addr = tlock.address(p);
            let iid = arcade_ledger::ord::iid_bytes(&n2, 0);
            let note = arcade_ledger::tokens::note(&node.chain, p.chain_no, &iid);
            let tick = s["tick"].as_str().unwrap_or("");
            let want = arcade_ledger::tokens::num(s["amount"].as_str().unwrap_or("")).and_then(|n| n.units(18)).map(|u| u.to_string());
            let ok = note.as_ref().is_some_and(|n| n["tick"] == tick && n["owner"] == lock_addr.as_str() && n["amt"].as_str().map(String::from) == want);
            if !ok {
                s["last"] = json!(format!("note 2 is not yet a live {tick} note of the lock here ({:?})", note));
                return Ok(());
            }
            let tx = tx_of(node, p, &n2).ok_or("note 2 isn't here")?;
            if tx.outputs.len() < 2 || tx.outputs[0].script_pubkey != tlock.script() || tx.outputs[1].script_pubkey != tlock.script() {
                return Err("note 2's coins are not both in the lock".into());
            }
            let (sk, pk) = wallet_key(node, p)?;
            let secret = hex::decode(s["secret"].as_str().unwrap_or("")).map_err(|_| "no secret")?;
            let coins = [(OutPoint { txid: n2, vout: 0 }, tx.outputs[0].value), (OutPoint { txid: n2, vout: 1 }, tx.outputs[1].value)];
            // The note's coin whole to this wallet as output 0 (its first move); the reserve pays the fee.
            let raw = htlc::claim(p, &tlock, &coins, vec![(tx.outputs[0].value, p2pkh_of(&pk))], &secret, &sk)?;
            s["claim_tx"] = json!(send_home(node, &Tx::parse_with(&raw, &p.format).map_err(|e| e.to_string())?)?);
            s["step"] = json!("claimed");
        }
        _ => {}
    }
    Ok(())
}

pub fn start(node: Arc<Node>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(15));
        for mut s in list(&node, "tswap") {
            if matches!(s["step"].as_str(), Some("claimed" | "refunded")) {
                continue;
            }
            let before = s.clone();
            let r = if s["role"] == "maker" { maker_step(&node, &mut s) } else { taker_step(&node, &mut s) };
            if let Err(e) = r {
                s["last"] = json!(e);
            }
            if s != before {
                put(&node, "tswap", &s);
            }
        }
    });
}
