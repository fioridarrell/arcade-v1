//! KOT staking (Martin 2026-11-10: "KOT staking should be available in the mine tab when KOT
//! is selected"). Rules in arcade_chain::kotia_pos; this side keeps the stake modifier, checks
//! itself against real blocks, and stakes this wallet's KOT when turned on.
//!
//! - Modifier: worked out from genesis over the stored chain (needs Kotia installed in full),
//!   carried forward block by block; checkpoints every 10,000 blocks in
//!   HOME/cache/kot-modifiers.json (a reorg goes back to the newest checkpoint still on the
//!   best chain). The last WINDOW modifiers stay in memory.
//! - Self-check: before staking (and on request) the newest PoS blocks are checked with our
//!   rules: kernel, retarget, block signature. Staking waits until every one passes, so a
//!   wrong rule never builds a block (a wrong block is only refused by the network anyway;
//!   no coin moves unless the network takes it).
//! - Staking: every 16-second slot, each mature coin (20 confirmations) of this wallet's KOT
//!   address is tried. A hit becomes a block: an empty coinbase, a coinstake paying coin + 20
//!   KOT back to the same address with the signing key in an OP_RETURN output (so the coin
//!   stays a plain address coin), signed, and sent to the chain's P2P peers.

use crate::node::Node;
use arcade_chain::kotia_pos as pos;
use arcade_chain::params::{by_key, ChainParams};
use arcade_chain::{Hash256, OutPoint, Tx};
use arcade_wallet::secp256k1::{ecdsa::Signature, Message, PublicKey, SECP256K1};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

const KEY: &str = "kot";
const CHECKPOINT_EVERY: u32 = 10_000;
const WINDOW: usize = 2_000;
/// Newest PoS blocks the self-check reads.
const SELF_CHECK: u32 = 200;
const META_ON: &[u8] = b"stake:kot:on";

#[derive(Default)]
struct Mods {
    /// Height, block hash and modifier after it, of the newest block worked out.
    at: Option<(u32, Hash256, Hash256)>,
    recent: VecDeque<(u32, Hash256)>,
    cps: Vec<(u32, Hash256, Hash256)>,
    loaded: bool,
}

#[derive(Default)]
struct St {
    mods: Mods,
    state: String,
    checked: Option<Value>,
    last_slot: u32,
    tried: u64,
    staked: Vec<Value>,
    prev_time: HashMap<Hash256, u32>,
}

static ST: Mutex<Option<St>> = Mutex::new(None);
/// Asked for a self-check (works the modifier out even while staking is off).
static CHECK: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn ask_check() -> Value {
    CHECK.store(true, std::sync::atomic::Ordering::Relaxed);
    json!({"asked": true})
}

fn with<T>(f: impl FnOnce(&mut St) -> T) -> T {
    let mut g = ST.lock().unwrap();
    f(g.get_or_insert_with(St::default))
}

fn params() -> &'static ChainParams {
    by_key(KEY).expect("kot params")
}

fn file(node: &Node) -> std::path::PathBuf {
    node.home.join("cache").join("kot-modifiers.json")
}

fn h(s: &str) -> Hash256 {
    Hash256::from_hex(s).unwrap_or(Hash256::ZERO)
}

fn load(node: &Node, m: &mut Mods) {
    if m.loaded {
        return;
    }
    m.loaded = true;
    if let Ok(v) = std::fs::read(file(node)).map_err(|_| ()).and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|_| ())) {
        m.cps = v["cps"].as_array().into_iter().flatten().filter_map(|c| Some((c[0].as_u64()? as u32, h(c[1].as_str()?), h(c[2].as_str()?)))).collect();
    }
}

fn save(node: &Node, m: &Mods) {
    let v = json!({"cps": m.cps.iter().map(|(a, b, c)| json!([a, b.to_hex(), c.to_hex()])).collect::<Vec<_>>()});
    let f = file(node);
    let _ = std::fs::create_dir_all(f.parent().unwrap());
    let tmp = f.with_extension("tmp");
    if std::fs::write(&tmp, v.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &f);
    }
}

/// The kernel a block adds to the modifier: its coinstake's first input txid (PoS), else its id.
fn kernel_at(node: &Node, p: &ChainParams, height: u32) -> Result<(Hash256, Hash256), String> {
    let c = p.chain_no;
    let hash = node.chain.hash_at(c, height).map_err(|e| e.to_string())?.ok_or_else(|| format!("no block at {height}"))?;
    let rec = node.chain.header(c, &hash).map_err(|e| e.to_string())?.ok_or("header missing")?;
    if rec.b == 0 {
        return Err(format!("block {height} not stored"));
    }
    let k = match node.chain.tx_at(p, rec.b, 1).map_err(|e| e.to_string())? {
        Some(t) if t.is_coinstake() => t.inputs[0].prevout.txid,
        _ => hash,
    };
    Ok((hash, k))
}

/// Carry the modifier forward to the tip (at most `max` blocks per call).
fn advance(node: &Node, max: u32) -> Result<Option<u32>, String> {
    let p = params();
    let c = p.chain_no;
    let Some(tip) = node.chain.tip(c).map_err(|e| e.to_string())? else { return Ok(None) };
    let mut m = with(|s| std::mem::take(&mut s.mods));
    load(node, &mut m);
    // Still on the best chain? Else back to the newest checkpoint that is.
    let on_best = |node: &Node, at: u32, hash: &Hash256| node.chain.hash_at(c, at).ok().flatten().as_ref() == Some(hash);
    if let Some((at, hash, _)) = m.at {
        if at > tip.height || !on_best(node, at, &hash) {
            m.at = None;
            m.recent.clear();
        }
    }
    if m.at.is_none() {
        m.cps.retain(|(a, hh, _)| *a <= tip.height && on_best(node, *a, hh));
        m.at = m.cps.last().cloned();
    }
    let mut done = 0;
    let mut err = None;
    while done < max {
        let next = m.at.map(|a| a.0 + 1).unwrap_or(0);
        if next > tip.height {
            break;
        }
        let (hash, k) = match kernel_at(node, p, next) {
            Ok(x) => x,
            Err(e) => {
                err = Some(e);
                break;
            }
        };
        // Genesis's modifier is 0; every later one hashes in its block's kernel.
        let modi = match m.at { None => Hash256::ZERO, Some((_, _, prev)) => pos::next_modifier(&prev, &k) };
        m.at = Some((next, hash, modi));
        m.recent.push_back((next, modi));
        while m.recent.len() > WINDOW {
            m.recent.pop_front();
        }
        if next % CHECKPOINT_EVERY == 0 && m.cps.last().map(|x| x.0) < Some(next) {
            m.cps.push((next, hash, modi));
            save(node, &m);
        }
        done += 1;
    }
    let at = m.at.map(|a| a.0);
    with(|s| s.mods = m);
    match err {
        Some(e) if done == 0 => Err(e),
        _ => Ok(at),
    }
}

fn modifier_at(height: u32) -> Option<Hash256> {
    with(|s| s.mods.recent.iter().rev().find(|x| x.0 == height).map(|x| x.1))
}

/// A transaction's own time field (Kotia txs carry one).
fn tx_time(node: &Node, p: &ChainParams, txid: &Hash256) -> Option<u32> {
    if let Some(t) = with(|s| s.prev_time.get(txid).copied()) {
        return Some(t);
    }
    let (raw, _) = node.chain.tx_raw(p, txid).ok()??;
    let t = Tx::parse_with(&raw, &p.format).ok()?.time?;
    with(|s| {
        if s.prev_time.len() > 50_000 {
            s.prev_time.clear();
        }
        s.prev_time.insert(*txid, t);
    });
    Some(t)
}

fn verify_block_sig(id: &Hash256, sig: &[u8], key: &[u8]) -> bool {
    let (Ok(pk), Ok(mut s)) = (PublicKey::from_slice(key), Signature::from_der_lax(sig)) else { return false };
    s.normalize_s();
    SECP256K1.verify_ecdsa(&Message::from_digest(id.0), &s, &pk).is_ok()
}

/// Check our rules on the newest `n` PoS blocks: (checked, passed, first failure).
pub fn self_check(node: &Node, n: u32) -> Value {
    let p = params();
    let c = p.chain_no;
    let Some(at) = with(|s| s.mods.at.map(|a| a.0)) else { return json!({"ok": false, "why": "stake modifier not worked out yet"}) };
    let (mut checked, mut passed) = (0u32, 0u32);
    let mut fails: Vec<Value> = Vec::new();
    let lo = at.saturating_sub(n.min(WINDOW as u32 - 3)).max(2);
    for height in lo..=at {
        let Some(hash) = node.chain.hash_at(c, height).ok().flatten() else { continue };
        let Some(raw) = node.chain.block_raw(c, &hash).ok().flatten() else { continue };
        let Ok(b) = arcade_chain::block::Block::parse_with(&raw, &p.format) else { continue };
        if !b.is_proof_of_stake() {
            continue;
        }
        checked += 1;
        let cs = &b.txs[1];
        let i0 = &cs.inputs[0];
        let prev_mod = modifier_at(height - 1);
        let value = node.chain.tx_raw(p, &i0.prevout.txid).ok().flatten()
            .and_then(|(r, _)| Tx::parse_with(&r, &p.format).ok())
            .and_then(|t| t.outputs.get(i0.prevout.vout as usize).map(|o| o.value));
        let ptime = tx_time(node, p, &i0.prevout.txid);
        let kernel = match (prev_mod, ptime, value) {
            (Some(m), Some(t), Some(v)) => pos::check_kernel(&m, b.header.bits, t, &i0.prevout.txid, i0.prevout.vout, v, b.header.time),
            _ => false,
        };
        let prev = |d: u32| node.chain.hash_at(c, height - d).ok().flatten().and_then(|hh| node.chain.header(c, &hh).ok().flatten()).map(|r| r.header);
        let bits_ok = match (prev(1), prev(2)) {
            (Some(a), Some(z)) => pos::next_bits(a.bits, a.time, z.time) == b.header.bits,
            _ => false,
        };
        let sig_ok = cs.outputs.get(1).and_then(|o| pos::block_signing_key(&o.script_pubkey)).is_some_and(|k| verify_block_sig(&b.id, b.sig.as_deref().unwrap_or(&[]), k));
        let time_ok = cs.time == Some(b.header.time) && b.header.time & pos::STAKE_TIMESTAMP_MASK == 0;
        if kernel && bits_ok && sig_ok && time_ok {
            passed += 1;
        } else if fails.len() < 5 {
            fails.push(json!({"height": height, "kernel": kernel, "bits": bits_ok, "signature": sig_ok, "time": time_ok}));
        }
    }
    let ok = checked >= 20 && passed == checked;
    let v = json!({"ok": ok, "checked": checked, "passed": passed, "failures": fails, "to": at});
    with(|s| s.checked = Some(v.clone()));
    v
}

fn script_num(n: u32) -> Vec<u8> {
    let mut v = Vec::new();
    let mut x = n as u64;
    while x > 0 {
        v.push((x & 0xff) as u8);
        x >>= 8;
    }
    if v.last().is_some_and(|b| b & 0x80 != 0) {
        v.push(0);
    }
    v
}

fn coinbase(p: &ChainParams, height: u32, time: u32) -> Tx {
    let mut ss = Vec::new();
    arcade_wallet::tx::push(&mut ss, &script_num(height));
    ss.push(0x00); // OP_0
    let mut o = Vec::new();
    o.extend_from_slice(&1i32.to_le_bytes());
    o.extend_from_slice(&time.to_le_bytes());
    o.push(1);
    o.extend_from_slice(&[0u8; 32]);
    o.extend_from_slice(&u32::MAX.to_le_bytes());
    arcade_chain::encode::write_varint(&mut o, ss.len() as u64);
    o.extend_from_slice(&ss);
    o.extend_from_slice(&u32::MAX.to_le_bytes());
    o.push(1); // one empty output
    o.extend_from_slice(&0u64.to_le_bytes());
    o.push(0);
    o.extend_from_slice(&0u32.to_le_bytes());
    Tx::parse_with(&o, &p.format).expect("coinbase we built")
}

struct Found {
    utxo: arcade_wallet::tx::Utxo,
    time: u32,
    bits: u32,
}

/// Build, self-check and sign a staked block on top of the tip.
fn build_block(node: &Node, p: &ChainParams, f: &Found, sk: &arcade_wallet::SecretKey) -> Result<(Vec<u8>, Hash256, u32), String> {
    let c = p.chain_no;
    let tip = node.chain.tip(c).map_err(|e| e.to_string())?.ok_or("no tip")?;
    let trec = node.chain.header(c, &tip.hash).map_err(|e| e.to_string())?.ok_or("no tip header")?;
    let height = tip.height + 1;
    let pk = PublicKey::from_secret_key(SECP256K1, sk);
    let cb = coinbase(p, height, f.time);
    let outs = vec![
        arcade_wallet::tx::TxOutSpec { value: 0, script: vec![] },
        arcade_wallet::tx::TxOutSpec { value: 0, script: arcade_wallet::tx::op_return(&pk.serialize()) },
        arcade_wallet::tx::TxOutSpec { value: f.utxo.value + pos::STAKE_SUBSIDY, script: f.utxo.script.clone() },
    ];
    let cs = arcade_wallet::tx::sign_p2pkh(p, std::slice::from_ref(&f.utxo), &outs, sk, f.time);
    if !cs.is_coinstake() || !arcade_wallet::tx::verify_p2pkh_input(p, &cs, std::slice::from_ref(&f.utxo), 0) {
        return Err("coinstake self-check failed".into());
    }
    let header = arcade_chain::header::Header {
        version: trec.header.version,
        prev: tip.hash,
        merkle_root: arcade_chain::hash::merkle_root(&[cb.txid, cs.txid]),
        time: f.time,
        bits: f.bits,
        nonce: 0,
    };
    let id = p.format.block_id(&header);
    let sig = SECP256K1.sign_ecdsa(&Message::from_digest(id.0), sk).serialize_der().to_vec();
    let mut raw = header.encode().to_vec();
    raw.push(2);
    raw.extend_from_slice(&cb.raw);
    raw.extend_from_slice(&cs.raw);
    arcade_chain::encode::write_varint(&mut raw, sig.len() as u64);
    raw.extend_from_slice(&sig);
    // Read it back as any node would.
    let b = arcade_chain::block::Block::parse_with(&raw, &p.format).map_err(|e| e.to_string())?;
    let key = b.txs[1].outputs.get(1).and_then(|o| pos::block_signing_key(&o.script_pubkey)).ok_or("no signing key")?;
    if b.id != id || b.computed_merkle_root() != header.merkle_root || !b.is_proof_of_stake() || !verify_block_sig(&b.id, b.sig.as_deref().unwrap_or(&[]), key) {
        return Err("staked block self-check failed".into());
    }
    Ok((raw, id, height))
}

fn send_block(node: &Node, p: &'static ChainParams, raw: Vec<u8>) {
    let peers: Vec<String> = node.coin_candidates(p).into_iter().take(8).collect();
    for a in peers {
        let raw = raw.clone();
        std::thread::spawn(move || {
            if let Ok(mut peer) = arcade_chain::p2p::Peer::connect(p, &a, Duration::from_secs(8)) {
                let _ = peer.send("block", &raw);
                std::thread::sleep(Duration::from_secs(3));
            }
        });
    }
}

pub fn is_on(node: &Node) -> bool {
    node.app.store.read(|r| r.get(arcade_app::META, META_ON)).ok().flatten().is_some_and(|v| v == b"1")
}

pub fn set_on(node: &Node, on: bool) -> Result<Value, String> {
    node.app.store.write(|w| w.put(arcade_app::META, META_ON, if on { b"1" } else { b"0" })).map_err(|e| e.to_string())?;
    let _ = node.app.event("stake", if on { "KOT staking turned on" } else { "KOT staking turned off" });
    Ok(status(node))
}

/// This wallet's KOT staking key and address script (key index 0).
fn stake_key(node: &Node, p: &ChainParams) -> Result<(arcade_wallet::SecretKey, Vec<u8>, String), String> {
    let (k, sk) = node.app.tag_key(p, 0).map_err(|e| e.to_string())?;
    let script = arcade_ledger::address::p2pkh_script_for_address(p, &k.address).ok_or("bad address")?;
    Ok((sk, script, k.address))
}

/// Mature coins of the staking address: (utxo, confirmations).
fn stake_coins(node: &Node, p: &ChainParams, script: &[u8], tip: u32) -> Vec<(arcade_wallet::tx::Utxo, u32)> {
    node.chain.coins_for_script(p, script).unwrap_or_default().into_iter()
        .filter(|(op, _, _)| node.chain.mempool_spender(p.chain_no, op).ok().flatten().is_none())
        .map(|(op, value, height)| (arcade_wallet::tx::Utxo { outpoint: OutPoint { txid: op.txid, vout: op.vout }, value, script: script.to_vec() }, tip + 1 - height.min(tip + 1)))
        .collect()
}

pub fn status(node: &Node) -> Value {
    let p = params();
    let installed = node.extra.lock().unwrap().contains_key(KEY);
    let tip = node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height).unwrap_or(0);
    let (addr, coins) = match (installed, stake_key(node, p)) {
        (true, Ok((_, script, a))) => {
            let cs = stake_coins(node, p, &script, tip);
            let mature: u64 = cs.iter().filter(|c| c.1 >= pos::COINBASE_MATURITY).map(|c| c.0.value).sum();
            let all: u64 = cs.iter().map(|c| c.0.value).sum();
            (json!(a), json!({"count": cs.len(), "total": all, "mature": mature}))
        }
        (_, Ok((_, _, a))) => (json!(a), Value::Null),
        _ => (Value::Null, Value::Null),
    };
    with(|s| json!({
        "on": is_on(node), "installed": installed, "address": addr, "coins": coins, "state": s.state,
        "modifier_at": s.mods.at.map(|a| a.0), "tip": tip, "self_check": s.checked, "tried": s.tried, "staked": s.staked,
    }))
}

fn set_state(t: &str) {
    with(|s| s.state = t.to_string());
}

/// Background: keep the modifier current and stake when turned on.
pub fn run(node: std::sync::Arc<Node>) {
    let p = params();
    let mut last_check = 0u64;
    loop {
        std::thread::sleep(Duration::from_secs(1));
        if !is_on(&node) && !CHECK.load(std::sync::atomic::Ordering::Relaxed) && with(|s| s.checked.is_none() && s.mods.at.is_none()) {
            set_state("off");
            continue;
        }
        let installed = node.extra.lock().unwrap().get(KEY).map(|r| r.mode == "full").unwrap_or(false);
        if !installed || crate::borrow::is_borrowed(KEY) {
            set_state("Kotia isn't installed in full here");
            continue;
        }
        let asked = CHECK.swap(false, std::sync::atomic::Ordering::Relaxed);
        if asked {
            last_check = 0;
            with(|s| s.checked = None);
        }
        if !is_on(&node) && with(|s| s.checked.is_some()) {
            set_state("off");
            continue;
        }
        // The modifier: in steps, so a fresh install catches up without holding things up.
        match advance(&node, 20_000) {
            Ok(Some(at)) => {
                let tip = node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height).unwrap_or(0);
                if at < tip {
                    set_state(&format!("working out the stake modifier: block {at} of {tip}"));
                    continue;
                }
            }
            Ok(None) => continue,
            Err(e) => {
                set_state(&format!("stake modifier: {e}"));
                std::thread::sleep(Duration::from_secs(20));
                continue;
            }
        }
        let now = arcade_app::now();
        if with(|s| s.checked.is_none()) || now > last_check + 3600 {
            last_check = now;
            self_check(&node, SELF_CHECK);
        }
        if !with(|s| s.checked.as_ref().is_some_and(|v| v["ok"] == true)) {
            set_state("our stake rules don't match the latest blocks yet; not staking");
            continue;
        }
        if !is_on(&node) {
            set_state("off");
            continue;
        }
        if let Err(e) = try_slot(&node, p) {
            set_state(&e);
        }
    }
}

fn try_slot(node: &Node, p: &'static ChainParams) -> Result<(), String> {
    let c = p.chain_no;
    let tip = node.chain.tip(c).map_err(|e| e.to_string())?.ok_or("no tip")?;
    let best = node.chain.best_header(c).ok().flatten().and_then(|hh| node.chain.header(c, &hh).ok().flatten()).map(|r| r.height).unwrap_or(0);
    if best > tip.height + 1 {
        return Err(format!("catching up ({} blocks behind)", best - tip.height));
    }
    let trec = node.chain.header(c, &tip.hash).map_err(|e| e.to_string())?.ok_or("no tip header")?;
    let prev = node.chain.hash_at(c, tip.height.saturating_sub(1)).ok().flatten().and_then(|hh| node.chain.header(c, &hh).ok().flatten()).ok_or("no previous header")?;
    let now = arcade_app::now() as u32;
    let slot = now & !pos::STAKE_TIMESTAMP_MASK;
    if slot <= trec.header.time || with(|s| s.last_slot >= slot) {
        return Ok(());
    }
    with(|s| s.last_slot = slot);
    let modi = modifier_at(tip.height).ok_or("modifier of the tip not worked out")?;
    let bits = pos::next_bits(trec.header.bits, trec.header.time, prev.header.time);
    let (sk, script, addr) = stake_key(node, p)?;
    let coins: Vec<_> = stake_coins(node, p, &script, tip.height).into_iter().filter(|c| c.1 >= pos::COINBASE_MATURITY).collect();
    if coins.is_empty() {
        return Err(format!("no mature KOT at {addr} (coins need {} confirmations)", pos::COINBASE_MATURITY));
    }
    let total: u64 = coins.iter().map(|c| c.0.value).sum();
    set_state(&format!("staking {} coin{} ({} KOT) from {addr}", coins.len(), if coins.len() == 1 { "" } else { "s" }, total as f64 / 1e8));
    for (u, _) in coins {
        with(|s| s.tried += 1);
        let Some(t) = tx_time(node, p, &u.outpoint.txid) else { continue };
        if !pos::check_kernel(&modi, bits, t, &u.outpoint.txid, u.outpoint.vout, u.value, slot) {
            continue;
        }
        let f = Found { utxo: u, time: slot, bits };
        let (raw, id, height) = build_block(node, p, &f, &sk)?;
        send_block(node, p, raw);
        let rec = json!({"height": height, "block": id.to_hex(), "at": now, "stake": f.utxo.value, "reward": pos::STAKE_SUBSIDY});
        with(|s| {
            s.staked.push(rec.clone());
            if s.staked.len() > 50 {
                s.staked.remove(0);
            }
        });
        let _ = node.app.event("stake", &format!("staked KOT block {height} ({}); sent to peers", id.to_hex()));
        break;
    }
    Ok(())
}
