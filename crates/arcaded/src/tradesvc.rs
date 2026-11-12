//! Trades between two players of one room (arcade.swap.trade), the original arcade's way:
//! one transaction both sides sign, so both legs move or neither does.
//!
//! The app windows run the talk between the two players, as hidden room messages
//! ("\u{1f}arcade-trade:", appwindow.js). This arcade does the parts that need the wallet:
//!
//! - `trade.leg`: the side that signs builds its leg (`arcade_wallet::legs`): two of its own
//!   coins, the swap's OP_RETURN, its payment, both signed SINGLE|ANYONECANPAY. The leg is sealed
//!   to the other player's published messaging key (inner box from our key, in a sealed box) as
//!   `{"leg": {raw, signatures, pubkey, seller, amount, give, take}}`, and its two coins are held
//!   back from other spends for a while.
//! - `trade.open`: open a sealed leg sent to us.
//! - `trade.fill`: check a leg against the trade agreed, then complete it with our coins: first to
//!   say the fee, then (send) signed, pooled and sent. Checked here, beyond the original: each
//!   signature of the leg, over the digest of the coin it spends.
//! - `trade.items`, `trade.who`: what a player holds, and whether a player can be sealed to.
//!
//! A side is `{inscription: id or number}`, `{token: id, amount: "25"}` or `{coins: "1.5"}`. The
//! side that signs gives an inscription or a token, never coins (the leg pays its seller).

use crate::node::Node;
use arcade_chain::{Hash256, OutPoint, Tx};
use arcade_wallet::legacy::{self, Shape};
use arcade_wallet::legs::{self, Coin, Fees, Side};
use arcade_wallet::secp256k1::{PublicKey, SECP256K1};
use base64::Engine as _;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a leg's two coins are kept from other spends.
const HOLD: Duration = Duration::from_secs(30 * 60);

fn fees() -> Fees {
    Fees { per_kb: crate::claims::FEE_PER_KB, dust: crate::claims::DUST, soft_dust: true }
}

fn b64() -> base64::engine::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

// ---------------- sides ----------------

/// A decimal amount ("1.5", "25") in units with `places` decimals.
fn units_of(text: &str, places: u32) -> Result<u64, String> {
    let t = text.trim();
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    if whole.is_empty() && frac.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) || frac.len() > places as usize {
        return Err(format!("{t} is not an amount"));
    }
    let mut f = frac.to_string();
    while f.len() < places as usize {
        f.push('0');
    }
    let n: u128 = format!("{}{f}", if whole.is_empty() { "0" } else { whole }).parse().map_err(|_| format!("{t} is not an amount"))?;
    u64::try_from(n).ok().filter(|n| *n > 0).ok_or_else(|| format!("{t} is not an amount above nothing"))
}

fn property(node: &Node, pid: u32) -> Option<Value> {
    arcade_ledger::engine::property(&node.chain, node.params.chain_no, pid).ok().flatten()
}

fn divisible(p: &Value) -> bool {
    p["divisible"] == true || p["type"] == 2
}

/// A side as the page names it.
pub fn side_of(node: &Node, v: &Value) -> Result<Side, String> {
    if let Some(id) = v.get("inscription").filter(|x| !x.is_null()) {
        let key = id.as_str().map(String::from).or_else(|| id.as_u64().map(|n| n.to_string())).unwrap_or_default();
        let row = arcade_ledger::content::inscription(&node.chain, node.params.chain_no, key.trim_start_matches('#')).ok_or("there is no such inscription")?;
        let t: [u8; 32] = hex::decode(row["txid"].as_str().unwrap_or("")).ok().and_then(|b| b.try_into().ok()).ok_or("bad inscription id")?;
        return Ok(Side::Inscription(t));
    }
    if let Some(tok) = v.get("token").filter(|x| !x.is_null()) {
        let pid = tok.as_u64().or_else(|| tok.as_str().and_then(|s| s.parse().ok())).and_then(|n| u32::try_from(n).ok()).ok_or("a token is named by its number")?;
        let p = property(node, pid).ok_or("there is no such token")?;
        let amount = v["amount"].as_str().map(String::from).or_else(|| v["amount"].as_f64().map(|f| f.to_string())).unwrap_or_default();
        return Ok(Side::Token { pid, units: units_of(&amount, if divisible(&p) { 8 } else { 0 })? });
    }
    if let Some(c) = v.get("coins").filter(|x| !x.is_null()) {
        let text = c.as_str().map(String::from).or_else(|| c.as_f64().map(|f| f.to_string())).unwrap_or_default();
        return Ok(Side::Coins(units_of(&text, 8)?));
    }
    Err("a side is {inscription}, {token, amount} or {coins}".into())
}

/// A side as the original describes it (swap.py leg_json).
pub fn side_json(node: &Node, s: &Side) -> Value {
    match s {
        Side::Inscription(t) => {
            let id = hex::encode(t);
            let row = arcade_ledger::content::inscription(&node.chain, node.params.chain_no, &id).unwrap_or(Value::Null);
            json!({"kind": "inscription", "txid": id, "number": row["number"], "collection": Value::Null, "edition": Value::Null})
        }
        Side::Token { pid, units } => {
            let p = property(node, *pid).unwrap_or(Value::Null);
            json!({"kind": "token", "propertyid": pid, "name": p["name"], "amount": arcade_ledger::amount::token_plain(divisible(&p), *units), "units": units})
        }
        Side::Coins(s) => json!({"kind": "coins", "amount": arcade_ledger::amount::fixed8(*s), "sats": s}),
    }
}

/// Whether `address` holds `side` now, waiting transactions counted.
fn holds(node: &Node, address: &str, side: &Side) -> Result<(), String> {
    let c = node.params.chain_no;
    match side {
        Side::Inscription(t) => {
            let row = arcade_ledger::content::inscription(&node.chain, c, &hex::encode(t)).ok_or("there is no such inscription")?;
            if row["owner"].as_str() != Some(address) {
                return Err(format!("#{} is not held by {address}", row["number"]));
            }
            let (_, leaving) = crate::walletsvc::pending_moves(node, address);
            if leaving.iter().any(|m| m["kind"] == "inscription" && m["txid"].as_str() == Some(&hex::encode(t))) {
                return Err(format!("#{} is already on its way out", row["number"]));
            }
            Ok(())
        }
        Side::Token { pid, units } => {
            let have = arcade_ledger::tokens::balances(&node.chain, c, address).into_iter().find(|b| b.0 == pid.to_string()).and_then(|b| b.2.parse::<u64>().ok()).unwrap_or(0);
            let (_, leaving) = crate::walletsvc::pending_moves(node, address);
            let out: u64 = leaving.iter().filter(|m| m["kind"] == "token" && m["property_id"].as_u64() == Some(*pid as u64)).filter_map(|m| m["units"].as_u64()).sum();
            if have.saturating_sub(out) < *units {
                return Err(format!("{address} does not hold that many of token #{pid}"));
            }
            Ok(())
        }
        Side::Coins(_) => Ok(()),
    }
}

// ---------------- coins held for legs ----------------

static HELD: Mutex<Vec<(OutPoint, Instant)>> = Mutex::new(Vec::new());

fn hold(node: &Node, ops: &[OutPoint]) {
    let mut h = HELD.lock().unwrap();
    let now = Instant::now();
    let mut spent = node.pending_spent.lock().unwrap();
    h.retain(|(op, at)| {
        let keep = now.duration_since(*at) < HOLD;
        if !keep {
            spent.remove(op);
        }
        keep
    });
    for op in ops {
        spent.insert(*op);
        h.push((*op, now));
    }
}

/// The seller's coins for a leg: its two smallest of at least the dust line.
fn leg_coins(node: &Node, script: &[u8]) -> Vec<Coin> {
    let mut cs: Vec<Coin> = crate::claims::spendable(node, script).into_iter().filter(|u| u.value >= crate::claims::DUST).map(|u| Coin { outpoint: u.outpoint, value: u.value }).collect();
    cs.sort_by_key(|c| c.value);
    cs
}

/// Split one coin into two for a leg (a wallet with one coin), pooled and sent: its outputs.
fn split(node: &Node, sk: &arcade_wallet::SecretKey, script: &[u8], from: &Coin) -> Result<[Coin; 2], String> {
    let f = fees();
    let outs_shape = vec![(0u64, script.to_vec()), (0u64, script.to_vec())];
    let fee = legs::price(&f, 1, &outs_shape, false);
    let first = 2 * f.dust;
    let rest = from.value.checked_sub(first + fee).filter(|r| *r >= f.dust).ok_or("the wallet needs two coins of at least 0.01 for a trade, or one big enough to split")?;
    let shape = Shape {
        version: 1,
        time: None,
        inputs: vec![legacy::In { prevout: from.outpoint, sequence: u32::MAX }],
        outputs: vec![(first, script.to_vec()), (rest, script.to_vec())],
        lock_time: 0,
    };
    let tx = sign_and_send(node, &shape, &[], &[(0, sk)], script)?;
    Ok([Coin { outpoint: OutPoint { txid: tx, vout: 0 }, value: first }, Coin { outpoint: OutPoint { txid: tx, vout: 1 }, value: rest }])
}

/// Sign the inputs listed (index, key) ALL over `script`, keep the given scriptSigs for the
/// rest, pool and send. Its txid.
fn sign_and_send(node: &Node, shape: &Shape, given: &[(usize, Vec<u8>)], mine: &[(usize, &arcade_wallet::SecretKey)], script: &[u8]) -> Result<Hash256, String> {
    let mut sigs: Vec<Vec<u8>> = vec![Vec::new(); shape.inputs.len()];
    for (i, s) in given {
        sigs[*i] = s.clone();
    }
    for (i, sk) in mine {
        let d = legacy::sighash(shape, *i, script, legacy::ALL)?;
        let pk = PublicKey::from_secret_key(SECP256K1, sk).serialize();
        sigs[*i] = legs::script_sig(&legacy::sign(sk, &d, legacy::ALL), &pk);
    }
    let raw = legacy::serialize(shape, &sigs);
    let tx = Tx::parse_with(&raw, &node.params.format).map_err(|e| format!("built a transaction that does not parse: {e}"))?;
    if !crate::mempoolshare::add(node, node.params, &tx) {
        return Err("the transaction was refused: one of its coins is already spent by a waiting transaction".into());
    }
    crate::claims::mark_spent(node, &tx);
    node.tx_outbox.lock().unwrap().push(tx.raw.clone());
    Ok(tx.txid)
}

// ---------------- the side that signs ----------------

/// trade.leg: build, sign and seal our leg giving `give` for `take` to `buyer`.
pub fn leg(node: &Node, give: &Value, take: &Value, buyer: &str) -> Result<Value, String> {
    let (me, _, sk) = crate::msgsvc::sender_key(node)?;
    let buyer = buyer.trim();
    if buyer.is_empty() || buyer == me {
        return Err("a trade needs another player".into());
    }
    let (g, t) = (side_of(node, give)?, side_of(node, take)?);
    if matches!(g, Side::Coins(_)) {
        return Err("the side that signs gives an inscription or a token, not coins".into());
    }
    if matches!(t, Side::Coins(_)) && matches!(g, Side::Coins(_)) {
        return Err("coins for coins is no trade".into());
    }
    holds(node, &me, &g)?;
    holds(node, buyer, &t).map_err(|e| format!("the other player: {e}"))?;
    let seal_to = crate::msgsvc::who(node, buyer).map_err(|e| format!("the other player can't be sealed to: {e}"))?;
    let key: [u8; 32] = hex::decode(seal_to["key"].as_str().unwrap_or("")).ok().and_then(|b| b.try_into().ok()).ok_or("the other player has no messaging key")?;
    let pk = PublicKey::from_secret_key(SECP256K1, &sk);
    let script = arcade_wallet::keys::p2pkh_script_for(&pk);
    let mut coins = leg_coins(node, &script);
    if coins.len() < 2 {
        let big = coins.pop().ok_or("the wallet needs two coins of at least 0.01 for a trade")?;
        coins = split(node, &sk, &script, &big)?.to_vec();
    }
    let opret = legs::op_return(&legs::swap_data(&g, &t))?;
    let built = legs::build(&fees(), &coins[0], &coins[1], t.coins(), &opret, &script)?;
    let sigs: Vec<String> = built.sighashes.iter().map(|d| hex::encode(legacy::sign(&sk, d, legacy::SINGLE_ANYONECANPAY))).collect();
    hold(node, &[coins[0].outpoint, coins[1].outpoint]);
    let body = json!({"leg": {
        "raw": hex::encode(legacy::serialize(&built.shape, &[])), "signatures": sigs, "pubkey": hex::encode(pk.serialize()), "seller": me,
        "amount": if t.coins() > 0 { arcade_ledger::amount::fixed8(t.coins()) } else { "0".into() },
        "take": side_json(node, &t), "give": side_json(node, &g)}});
    let keys = crate::msgsvc::keys(node)?;
    let sealed = arcade_msg::envelope::seal(&keys.secret, &crypto_box::PublicKey::from(key), b"", body.to_string().as_bytes());
    Ok(json!({"sealed": b64().encode(sealed), "seal_to": hex::encode(key), "fee": built.fee, "paid": built.paid, "give": side_json(node, &g), "take": side_json(node, &t)}))
}

/// trade.open: a sealed leg (or bundle) sent to us.
pub fn open(node: &Node, sealed: &str) -> Result<Value, String> {
    let bytes = b64().decode(sealed.trim()).map_err(|_| "that trade did not arrive whole")?;
    let keys = crate::msgsvc::keys(node)?;
    let (_, plain) = arcade_msg::envelope::open(&keys.secret, b"", &bytes).map_err(|_| "that trade was not sealed for this player")?;
    serde_json::from_slice(&plain).map_err(|_| "that trade is not readable".into())
}

// ---------------- the side that completes ----------------

fn shape_of(tx: &Tx) -> Shape {
    Shape {
        version: tx.version,
        time: tx.time,
        inputs: tx.inputs.iter().map(|i| legacy::In { prevout: i.prevout, sequence: i.sequence }).collect(),
        outputs: tx.outputs.iter().map(|o| (o.value, o.script_pubkey.clone())).collect(),
        lock_time: tx.lock_time,
    }
}

/// A coin's value: confirmed and unspent, or the output of a waiting transaction, unspent.
fn coin_value(node: &Node, op: &OutPoint, script: &[u8]) -> Option<u64> {
    let p = node.params;
    if node.chain.mempool_spender(p.chain_no, op).ok().flatten().is_some() {
        return None;
    }
    if let Ok(Some(c)) = node.chain.coin(p, op) {
        return (c.script == script).then_some(c.value);
    }
    crate::claims::unconfirmed_coins(node, script).into_iter().find(|u| u.outpoint == *op).map(|u| u.value)
}

struct Checked {
    shape: Shape,
    sigs: [Vec<u8>; 2],
    seller: String,
    reserve: u64,
    give: Side,
    take: Side,
}

/// A leg, checked against the trade agreed: `expect` is the signer's view {give, take}, and the
/// signer must be `peer` (the player we are trading with).
fn check(node: &Node, leg: &Value, expect: &Value, peer: &str) -> Result<Checked, String> {
    let p = node.params;
    let raw = hex::decode(leg["raw"].as_str().unwrap_or("")).map_err(|_| "the leg has no transaction")?;
    let tx = Tx::parse_with(&raw, &p.format).map_err(|_| "the leg's transaction does not parse")?;
    if tx.inputs.len() != 2 || tx.outputs.len() != 2 || tx.inputs.iter().any(|i| !i.script_sig.is_empty()) || tx.lock_time != 0 {
        return Err("that is not a leg (two coins in, two outputs, nothing signed in it yet)".into());
    }
    let pubkey = hex::decode(leg["pubkey"].as_str().unwrap_or("")).map_err(|_| "the leg has no key")?;
    let pk = PublicKey::from_slice(&pubkey).map_err(|_| "the leg's key is not a key")?;
    let script = arcade_wallet::keys::p2pkh_script_for(&pk);
    let seller = arcade_ledger::address::address_of(&script, p).unwrap_or_default();
    if seller.is_empty() || leg["seller"].as_str() != Some(seller.as_str()) || seller != peer {
        return Err("that leg is not from the player you are trading with".into());
    }
    if tx.outputs[1].script_pubkey != script {
        return Err("the leg does not pay its own seller".into());
    }
    let (give, take) = legs::swap_of(&tx.outputs[0].script_pubkey).filter(|_| tx.outputs[0].value == 0).ok_or("the leg does not name a swap")?;
    if give != side_of(node, &expect["give"])? || take != side_of(node, &expect["take"])? {
        return Err("that leg is not the trade you agreed to".into());
    }
    let shape = shape_of(&tx);
    let sigs: Vec<Vec<u8>> = leg["signatures"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().and_then(|h| hex::decode(h).ok())).collect()).unwrap_or_default();
    if sigs.len() != 2 {
        return Err("a leg carries two signatures".into());
    }
    let mut held = 0u64;
    for i in 0..2 {
        let d = legacy::sighash(&shape, i, &script, legacy::SINGLE_ANYONECANPAY)?;
        if !legacy::verify(&pubkey, &sigs[i], &d, legacy::SINGLE_ANYONECANPAY) {
            return Err(format!("the leg's signature {i} does not hold"));
        }
        held += coin_value(node, &tx.inputs[i].prevout, &script).ok_or("one of the leg's coins is spent or unknown here")?;
    }
    let fee = legs::swap_fee(&fees(), &tx.outputs[0].script_pubkey);
    if tx.outputs[1].value != (held + take.coins()).saturating_sub(fee) {
        return Err("the leg's payment is not its coins plus the price less the fee".into());
    }
    holds(node, &seller, &give).map_err(|e| format!("the other player: {e}"))?;
    Ok(Checked { shape, sigs: [legs::script_sig(&sigs[0], &pubkey), legs::script_sig(&sigs[1], &pubkey)], seller, reserve: fee, give, take })
}

/// trade.fill: complete a leg from `peer` with our coins; `send` signs and sends it.
pub fn fill(node: &Node, leg: &Value, expect: &Value, peer: &str, send: bool) -> Result<Value, String> {
    let (me, _, sk) = crate::msgsvc::sender_key(node)?;
    let c = check(node, leg, expect, peer)?;
    if c.seller == me {
        return Err("that is your own leg".into());
    }
    holds(node, &me, &c.take)?;
    let pk = PublicKey::from_secret_key(SECP256K1, &sk);
    let script = arcade_wallet::keys::p2pkh_script_for(&pk);
    let leg_ins: Vec<OutPoint> = c.shape.inputs.iter().map(|i| i.prevout).collect();
    let coins: Vec<Coin> = crate::claims::spendable(node, &script).into_iter().filter(|u| !leg_ins.contains(&u.outpoint)).map(|u| Coin { outpoint: u.outpoint, value: u.value }).collect();
    let done = legs::complete(&fees(), &c.shape, c.reserve, c.take.coins(), &coins, &script)?;
    let what = json!({"give": side_json(node, &c.take), "get": side_json(node, &c.give), "fee": done.fee, "fee_text": arcade_ledger::amount::fixed8(done.fee), "price": c.take.coins()});
    if !send {
        return Ok(what);
    }
    let mine: Vec<(usize, &arcade_wallet::SecretKey)> = (2..done.shape.inputs.len()).map(|i| (i, &sk)).collect();
    let txid = sign_and_send(node, &done.shape, &[(0, c.sigs[0].clone()), (1, c.sigs[1].clone())], &mine, &script)?;
    Ok(json!({"txid": txid.to_hex(), "what": what}))
}

/// trade.watch: how a sent trade stands. "settled" once in a block (and, for an inscription, held
/// by `owner` there); "failed" when it can no longer confirm or the block did not move the piece;
/// else "waiting" (the original's tradeWatch).
pub fn watch(node: &Node, txid: &str, inscription: Option<&str>, owner: Option<&str>) -> Value {
    let Some(st) = crate::walletsvc::tx_status(node, txid) else {
        return json!({"state": "waiting", "why": "not seen here yet"});
    };
    if st["conflicted"] == true {
        return json!({"state": "failed", "why": "the trade was not confirmed"});
    }
    if st["confirmed"] != true {
        return json!({"state": "waiting"});
    }
    if let (Some(i), Some(o)) = (inscription.filter(|i| !i.is_empty()), owner.filter(|o| !o.is_empty())) {
        let row = arcade_ledger::content::inscription(&node.chain, node.params.chain_no, i).unwrap_or(Value::Null);
        if row["owner"].as_str().is_some_and(|now| now != o) {
            return json!({"state": "failed", "why": "the block refused the trade; nothing moved"});
        }
    }
    json!({"state": "settled", "txid": txid})
}

/// trade.items: what this player holds, for a trade window (the original's items()).
pub fn items(node: &Node) -> Result<Value, String> {
    let (me, _, _) = crate::msgsvc::sender_key(node)?;
    let q: serde_json::Map<String, Value> = [("pending".to_string(), json!("1"))].into_iter().collect();
    let parse = |(_, body): (u16, String)| serde_json::from_str::<Value>(&body).unwrap_or(json!([]));
    Ok(json!({"address": me, "inscriptions": parse(crate::walletsvc::route(node, &format!("inscriptions/{me}"), &q)),
              "balances": parse(crate::walletsvc::route(node, &format!("balances/{me}"), &q))}))
}

/// trade.who: whether a player can be sealed to (has published a messaging key).
pub fn who(node: &Node, address: &str) -> Result<Value, String> {
    crate::msgsvc::who(node, address)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn amounts_read_as_the_original() {
        assert_eq!(units_of("1.5", 8).unwrap(), 150_000_000);
        assert_eq!(units_of("25", 0).unwrap(), 25);
        assert_eq!(units_of(".01", 8).unwrap(), 1_000_000);
        assert!(units_of("1.5", 0).is_err(), "an indivisible token has no fractions");
        assert!(units_of("0", 8).is_err());
        assert!(units_of("1e5", 8).is_err());
        assert!(units_of("-1", 8).is_err());
        assert!(units_of("1.123456789", 8).is_err());
    }
}
