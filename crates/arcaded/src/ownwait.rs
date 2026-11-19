//! This arcade's own transactions while they wait for a block, on every chain (ARCADE #15).
//!
//! Kept in the arcade's META under OWN, so a restart doesn't forget them:
//! chain key -> txid -> {raw, first, ins: [[script hex, value]], missing?, dropped?}.
//! Each round (`sweep`), one that is:
//!   in a block    is forgotten;
//!   in the pool   waits;
//!   in neither    is put back once a run (a restart empties the ARCADE pool and the borrowed
//!                 ones) when its coins are still there. Still missing after MISSING_FOR, it is
//!                 marked dropped: its coins are free again (the wallet's figures read the live
//!                 pool) and the Wallet's Activity says it didn't go through. Kept KEEP, then forgotten.
//! The home chain's pool is kept on disk and resend.rs sends its transactions again; this list is
//! for what the Wallet shows, and for the pools a restart empties.

use crate::node::Node;
use arcade_chain::{params::ChainParams, Hash256, OutPoint, Tx};
use serde_json::{json, Map, Value};
use std::collections::HashSet;
use std::sync::Mutex;

const OWN: &str = "own_waiting";
/// Out of the pool this long (and not in a block): dropped.
const MISSING_FOR: u64 = 60;
/// A dropped one is shown this long.
const KEEP: u64 = 3 * 86_400;
/// Most kept per chain (the oldest go first).
const MOST: usize = 500;

/// Put back (or sent) during this run: not put back again.
static TRIED: Mutex<Option<HashSet<Hash256>>> = Mutex::new(None);

fn tried(id: &Hash256) -> bool {
    TRIED.lock().unwrap().as_ref().is_some_and(|s| s.contains(id))
}

fn mark_tried(id: Hash256) {
    TRIED.lock().unwrap().get_or_insert_with(HashSet::new).insert(id);
}

fn load(node: &Node) -> Map<String, Value> {
    node.app.store.read(|r| r.get(arcade_app::META, OWN.as_bytes())).ok().flatten()
        .and_then(|v| serde_json::from_slice::<Value>(&v).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

fn save(node: &Node, m: &Map<String, Value>) {
    let v = Value::Object(m.clone()).to_string();
    let _ = node.app.store.write(|w| w.put(arcade_app::META, OWN.as_bytes(), v.as_bytes()));
}

/// The chain a key names here (the home chain as this arcade runs it).
fn chain(node: &Node, key: &str) -> Option<&'static ChainParams> {
    if key == node.params.key {
        return Some(node.params);
    }
    crate::chains::find(key)
}

/// An unspent confirmed coin, or a waiting output: (script, value).
fn coin_of(node: &Node, p: &'static ChainParams, pooled: &[Tx], op: &OutPoint) -> Option<(Vec<u8>, u64)> {
    let confirmed = if crate::borrow::is_borrowed(p.key) {
        crate::borrow::unspent_coin(node, p, op)
    } else {
        node.chain.coin(p, op).ok().flatten().map(|c| (c.script, c.value))
    };
    confirmed.or_else(|| pooled.iter().find(|t| t.txid == op.txid).and_then(|t| t.outputs.get(op.vout as usize)).map(|o| (o.script_pubkey.clone(), o.value)))
}

fn pool_of(node: &Node, p: &'static ChainParams) -> Vec<Tx> {
    if crate::borrow::is_borrowed(p.key) {
        crate::borrow::pool_txs(p)
    } else {
        crate::sitesvc::pool(node, p).into_iter().map(|(_, t)| t).collect()
    }
}

/// One of this arcade's own transactions just sent on chain `p`: remembered until a block has it.
pub fn note(node: &Node, p: &'static ChainParams, tx: &Tx) {
    mark_tried(tx.txid);
    let mut m = load(node);
    let id = tx.txid.to_hex();
    if m.get(p.key).is_some_and(|c| c.get(&id).is_some()) {
        return;
    }
    let pooled = pool_of(node, p);
    let ins: Vec<Value> = tx.inputs.iter().map(|i| match coin_of(node, p, &pooled, &i.prevout) {
        Some((s, v)) => json!([hex::encode(s), v]),
        None => json!(["", 0]),
    }).collect();
    let c = m.entry(p.key.to_string()).or_insert_with(|| json!({})).as_object_mut().unwrap();
    c.insert(id, json!({"raw": hex::encode(&tx.raw), "first": arcade_app::now(), "ins": ins}));
    while c.len() > MOST {
        let oldest = c.iter().min_by_key(|(_, v)| v["first"].as_u64().unwrap_or(0)).map(|(k, _)| k.clone()).unwrap();
        c.remove(&oldest);
    }
    save(node, &m);
}

/// An ARCADE transaction of this arcade's own: into the chain's pool, and remembered.
/// Borrowed (not installed, or installed and still catching up): to the lenders, which take it into their pools.
pub fn send_arc(node: &Node, tx: &Tx) -> Result<(Hash256, u64), String> {
    if let Some(p) = crate::arcsvc::defined().filter(|p| crate::arcsvc::borrowed(p)) {
        // The fee from the coins it spends, as the borrowed copy knows them (confirmed, or change still waiting).
        let waiting: Vec<(arcade_chain::OutPoint, u64)> = crate::borrow::pool_txs(p).iter()
            .flat_map(|t| t.outputs.iter().enumerate().map(move |(i, o)| (arcade_chain::OutPoint { txid: t.txid, vout: i as u32 }, o.value)))
            .collect();
        let ins: u64 = tx.inputs.iter().filter_map(|i| {
            crate::borrow::unspent_coin(node, p, &i.prevout).map(|c| c.1).or_else(|| waiting.iter().find(|w| w.0 == i.prevout).map(|w| w.1))
        }).sum();
        let fee = ins.saturating_sub(tx.outputs.iter().map(|o| o.value).sum());
        crate::borrow::broadcast_tx(node, p, tx).map_err(|(why, _)| why)?;
        note(node, p, tx);
        return Ok((tx.txid, fee));
    }
    let r = crate::arcsvc::send_raw(&tx.raw)?;
    if let Some(p) = crate::arcsvc::defined() {
        note(node, p, tx);
    }
    Ok(r)
}

/// How [`send_own`] sends a transaction on chain `p`.
#[derive(Debug, PartialEq, Clone, Copy)]
enum Way {
    /// The ARCADE chain (own copy or borrowed): [`send_arc`].
    Arcade,
    /// Borrowed (not installed, or installed and still catching up): to the lenders, which pool and send it; its
    /// change is in the borrowed pool at once, so the next transaction can spend it.
    Lenders,
    /// The home chain kept here: its own pool, then its peers (the outbox, resent until a block has it).
    OwnPool,
    /// Another installed chain: its own peers (crate::sitesvc::broadcast).
    Installed,
}

fn way(arcade: bool, borrowed: bool, home: bool) -> Way {
    if arcade {
        Way::Arcade
    } else if borrowed {
        Way::Lenders
    } else if home {
        Way::OwnPool
    } else {
        Way::Installed
    }
}

/// One of this arcade's own transactions out to chain `p`'s network, whichever way the chain is kept here: the one
/// place a send leaves (a home chain only borrowed sent nothing before: its outbox is drained by a sync loop that
/// doesn't run then). Err when it was refused (spent coins, no lender took it).
pub fn send_own(node: &Node, p: &'static ChainParams, tx: &Tx) -> Result<(), String> {
    let home = p.chain_no == node.params.chain_no;
    match way(crate::arcsvc::defined().is_some_and(|d| d.chain_no == p.chain_no), crate::borrow::is_borrowed(p.key), home) {
        Way::Arcade => send_arc(node, tx).map(|_| ()),
        Way::Lenders => {
            crate::borrow::broadcast_tx(node, p, tx).map_err(|(why, _)| format!("the transaction was refused: {why}"))?;
            if home {
                // Its coins stay spent here meanwhile; an installed home catching up also sends it to its own peers.
                crate::claims::mark_spent(node, tx);
                if crate::homeinst::installed() {
                    node.tx_outbox.lock().unwrap().push(tx.raw.clone());
                }
            }
            Ok(())
        }
        Way::OwnPool => {
            if !crate::mempoolshare::add(node, p, tx) {
                return Err("the transaction was refused: its coins are spent by another waiting transaction".into());
            }
            crate::claims::mark_spent(node, tx);
            crate::resend::send(node, p, tx);
            Ok(())
        }
        Way::Installed => crate::sitesvc::broadcast(node, &json!({"hex": hex::encode(&tx.raw), "chain": p.key})).map(|_| ()),
    }
}

#[derive(Debug, PartialEq)]
enum Where {
    Block,
    Pool,
    Neither,
}

fn where_is(node: &Node, p: &'static ChainParams, tx: &Tx) -> Where {
    if crate::borrow::is_borrowed(p.key) {
        return if crate::borrow::pooled(p, &tx.txid) { Where::Pool } else if crate::borrow::mined(node, p, tx) { Where::Block } else { Where::Neither };
    }
    let pooled = if crate::arcsvc::runs(p) { crate::arcsvc::pooled(&tx.txid) } else { node.chain.mempool_tx(p.chain_no, &tx.txid).ok().flatten().is_some() };
    if pooled {
        Where::Pool
    } else if node.chain.tx_raw(p, &tx.txid).ok().flatten().is_some() {
        Where::Block
    } else {
        Where::Neither
    }
}

/// Into the pool again, and out to the network. None while the chain can't say yet (not caught
/// up, no lender); Some(taken) otherwise.
fn put_back(node: &Node, p: &'static ChainParams, tx: &Tx) -> Option<bool> {
    // (An installed ARCADE chain still borrowed is sent by its lenders, below.)
    if crate::arcsvc::own(p) {
        if !crate::arcsvc::ready() {
            return None;
        }
        return Some(crate::arcsvc::send_raw(&tx.raw).is_ok());
    }
    if crate::borrow::is_borrowed(p.key) {
        if node.cr_offerers(p.key).is_empty() {
            return None;
        }
        return Some(match crate::borrow::broadcast_tx(node, p, tx) {
            Ok(_) => true,
            // A lender's node still holds it: it waits here again.
            Err((_, answers)) if answers.to_string().contains("already") => {
                crate::borrow::keep_waiting(p, tx);
                true
            }
            Err(_) => false,
        });
    }
    if !crate::chains::kept(node, p) {
        return None;
    }
    // Every coin it spends still there: unspent in a block, or a waiting output nothing else spends.
    let pooled = pool_of(node, p);
    let free = tx.inputs.iter().all(|i| {
        coin_of(node, p, &pooled, &i.prevout).is_some() && node.chain.mempool_spender(p.chain_no, &i.prevout).ok().flatten().is_none()
    });
    // And checked again as a new one is (a version or a reorg since may make it one the network refuses).
    if !free || crate::mempoolshare::sort(node, p, tx).is_err() || !crate::mempoolshare::add(node, p, tx) {
        return Some(false);
    }
    if p.chain_no == node.params.chain_no {
        crate::resend::again(node, &tx.raw);
    }
    Some(true)
}

/// Each round: forget what blocks have, put back what a restart lost, mark what left the pool
/// without a block as dropped. Returns how many were put back.
pub fn sweep(node: &Node, now: u64) -> usize {
    let mut m = load(node);
    if m.is_empty() {
        return 0;
    }
    let (mut back, mut changed) = (0, false);
    for (key, list) in m.iter_mut() {
        let Some(p) = chain(node, key) else { continue };
        let Some(list) = list.as_object_mut() else { continue };
        let ids: Vec<String> = list.keys().cloned().collect();
        for id in ids {
            let r = &list[&id];
            let tx = hex::decode(r["raw"].as_str().unwrap_or("")).ok().and_then(|raw| Tx::parse_with(&raw, &p.format).ok());
            let Some(tx) = tx else {
                list.remove(&id);
                changed = true;
                continue;
            };
            let mut w = where_is(node, p, &tx);
            if w == Where::Neither && !tried(&tx.txid) {
                match put_back(node, p, &tx) {
                    Some(taken) => {
                        mark_tried(tx.txid);
                        if taken {
                            back += 1;
                            w = Where::Pool;
                        }
                    }
                    None => continue, // asked again next round
                }
            }
            let r = list.get_mut(&id).unwrap().as_object_mut().unwrap();
            match w {
                Where::Block => {
                    list.remove(&id);
                    changed = true;
                }
                Where::Pool => {
                    changed |= r.remove("missing").is_some() | r.remove("dropped").is_some();
                }
                Where::Neither => {
                    let missing = match r.get("missing").and_then(|v| v.as_u64()) {
                        Some(t) => t,
                        None => {
                            r.insert("missing".into(), json!(now));
                            changed = true;
                            now
                        }
                    };
                    match r.get("dropped").and_then(|v| v.as_u64()) {
                        Some(d) if now.saturating_sub(d) > KEEP => {
                            list.remove(&id);
                            changed = true;
                        }
                        None if now.saturating_sub(missing) >= MISSING_FOR => {
                            r.insert("dropped".into(), json!(now));
                            changed = true;
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    m.retain(|_, v| v.as_object().is_some_and(|o| !o.is_empty()));
    if changed {
        save(node, &m);
    }
    if back > 0 {
        let _ = node.app.event("wallet", &format!("put {back} of this arcade's own waiting transaction{} back in the pool", if back == 1 { "" } else { "s" }));
    }
    back
}

/// Every 15 seconds, for as long as the arcade runs.
pub fn run(node: std::sync::Arc<Node>) {
    loop {
        std::thread::sleep(std::time::Duration::from_secs(15));
        sweep(&node, arcade_app::now());
    }
}

/// This wallet's own transactions on chain `p` that left the pool without a block: (txid,
/// received, spent) for the scripts `mine` takes.
pub fn dropped(node: &Node, p: &ChainParams, mine: impl Fn(&[u8]) -> bool) -> Vec<(Hash256, u64, u64)> {
    let m = load(node);
    let Some(list) = m.get(p.key).and_then(|v| v.as_object()) else { return Vec::new() };
    let mut out = Vec::new();
    for (id, r) in list {
        if r["dropped"].is_null() {
            continue;
        }
        let (Ok(txid), Some(tx)) = (Hash256::from_hex(id), hex::decode(r["raw"].as_str().unwrap_or("")).ok().and_then(|raw| Tx::parse_with(&raw, &p.format).ok())) else { continue };
        let received: u64 = tx.outputs.iter().filter(|o| mine(&o.script_pubkey)).map(|o| o.value).sum();
        let spent: u64 = r["ins"].as_array().into_iter().flatten()
            .filter(|i| hex::decode(i[0].as_str().unwrap_or("")).is_ok_and(|s| !s.is_empty() && mine(&s)))
            .map(|i| i[1].as_u64().unwrap_or(0)).sum();
        if received > 0 || spent > 0 {
            out.push((txid, received, spent));
        }
    }
    out
}

/// How many are remembered per chain (for tests and status).
#[cfg(test)]
fn remembered(node: &Node) -> std::collections::HashMap<String, usize> {
    load(node).iter().map(|(k, v)| (k.clone(), v.as_object().map_or(0, |o| o.len()))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sitesvc::tests::{block, p2pkh, setup, tx};

    fn forget_tried(id: &Hash256) {
        if let Some(s) = TRIED.lock().unwrap().as_mut() {
            s.remove(id);
        }
    }

    /// A restart lost an own waiting transaction from the pool: the next round puts it back and
    /// sends it again; a block then has it and it is forgotten.
    #[test]
    fn a_lost_own_transaction_is_put_back_after_a_restart() {
        // A signed spend of the funded test key's P2PKH coin (put back is checked as a new one is).
        let (t, coins) = crate::mempoolshare::tests::funded();
        let out = arcade_wallet::tx::TxOutSpec { value: 1300, script: p2pkh(1) };
        let ours = arcade_wallet::tx::sign_keyhash(t.p, 1, &coins[1..], &[out], &crate::mempoolshare::tests::key(), 0).unwrap();
        assert!(crate::mempoolshare::add(&t.node, t.p, &ours));
        note(&t.node, t.p, &ours);
        assert_eq!(remembered(&t.node)[t.p.key], 1);
        // The pool lost it, and the arcade started again.
        assert!(t.node.chain.mempool_remove(t.p, &ours.txid).unwrap());
        forget_tried(&ours.txid);
        t.node.tx_outbox.lock().unwrap().clear();
        let now = arcade_app::now();
        assert_eq!(sweep(&t.node, now), 1);
        assert!(t.node.chain.mempool_tx(t.p.chain_no, &ours.txid).unwrap().is_some(), "back in the pool");
        assert_eq!(t.node.tx_outbox.lock().unwrap().as_slice(), &[ours.raw.clone()], "and sent again");
        assert!(dropped(&t.node, t.p, |s| s == p2pkh(1)).is_empty());
        // Put back once a run only.
        assert!(t.node.chain.mempool_remove(t.p, &ours.txid).unwrap());
        assert_eq!(sweep(&t.node, now), 0);
        assert!(crate::mempoolshare::add(&t.node, t.p, &ours));
        // In a block: forgotten.
        let tip = t.node.chain.tip(t.p.chain_no).unwrap().unwrap();
        t.node.chain.connect_blocks(t.p, &[block(tip.hash, vec![tx(&[], &[(5000, p2pkh(3))], 12), ours.clone()], 12)]).unwrap();
        sweep(&t.node, now + 1);
        assert!(remembered(&t.node).is_empty());
    }

    /// Which way a send leaves: the ARCADE chain its own way, anything borrowed through the lenders (the home chain
    /// too: its outbox isn't sent while it is only borrowed), the kept home chain through its pool and outbox.
    #[test]
    fn a_send_leaves_by_how_the_chain_is_kept() {
        assert_eq!(way(true, true, false), Way::Arcade);
        assert_eq!(way(true, false, false), Way::Arcade);
        assert_eq!(way(false, true, true), Way::Lenders);
        assert_eq!(way(false, true, false), Way::Lenders);
        assert_eq!(way(false, false, true), Way::OwnPool);
        assert_eq!(way(false, false, false), Way::Installed);
    }

    /// The kept home chain: pooled, out through the outbox, remembered until a block has it; a second spend of the
    /// same coin is refused and sends nothing.
    #[test]
    fn a_home_send_is_pooled_sent_and_remembered() {
        let (t, coins) = crate::mempoolshare::tests::funded();
        let k = crate::mempoolshare::tests::key();
        let a = arcade_wallet::tx::sign_keyhash(t.p, 1, &coins[1..], &[arcade_wallet::tx::TxOutSpec { value: 1300, script: p2pkh(1) }], &k, 0).unwrap();
        let b = arcade_wallet::tx::sign_keyhash(t.p, 1, &coins[1..], &[arcade_wallet::tx::TxOutSpec { value: 1200, script: p2pkh(2) }], &k, 0).unwrap();
        t.node.tx_outbox.lock().unwrap().clear();
        send_own(&t.node, t.p, &a).unwrap();
        assert!(t.node.chain.mempool_tx(t.p.chain_no, &a.txid).unwrap().is_some(), "pooled");
        assert_eq!(t.node.tx_outbox.lock().unwrap().as_slice(), &[a.raw.clone()], "sent");
        assert_eq!(remembered(&t.node)[t.p.key], 1, "remembered");
        let e = send_own(&t.node, t.p, &b).unwrap_err();
        assert!(e.contains("spent by another"), "{e}");
        assert_eq!(t.node.tx_outbox.lock().unwrap().len(), 1, "the refused one isn't sent");
    }

    /// One the network would refuse (no signature) isn't put back, though its coin is there.
    #[test]
    fn an_unsigned_one_is_not_put_back() {
        let t = setup();
        let ours = tx(&[(t.spend.txid, 1)], &[(1800, p2pkh(1))], 41);
        note(&t.node, t.p, &ours);
        forget_tried(&ours.txid);
        assert_eq!(sweep(&t.node, arcade_app::now()), 0);
        assert!(t.node.chain.mempool_tx(t.p.chain_no, &ours.txid).unwrap().is_none());
    }

    /// A spend that left the pool without a block (a block took its coin in another transaction):
    /// not put back, dropped after MISSING_FOR, shown to the wallet, and the coins figures leave it out.
    #[test]
    fn a_spend_that_left_the_pool_is_shown_dropped() {
        let t = setup();
        let mine = |s: &[u8]| s == p2pkh(1);
        let ours = tx(&[(t.spend.txid, 1)], &[(500, p2pkh(4)), (1300, p2pkh(1))], 21);
        assert!(crate::mempoolshare::add(&t.node, t.p, &ours));
        note(&t.node, t.p, &ours);
        let f = crate::sitesvc::wallet_figures(&t.node, t.p, &[p2pkh(1)]).unwrap();
        assert_eq!((f.balance, f.free), (1300, 0));
        // Gone from the pool, its coin unspent: the figures give the coin back at once.
        assert!(t.node.chain.mempool_remove(t.p, &ours.txid).unwrap());
        let f = crate::sitesvc::wallet_figures(&t.node, t.p, &[p2pkh(1)]).unwrap();
        assert_eq!((f.balance, f.free), (1900, 1900));
        // A block spends the coin in another transaction: put back is refused.
        let other = tx(&[(t.spend.txid, 1)], &[(1850, p2pkh(5))], 22);
        let tip = t.node.chain.tip(t.p.chain_no).unwrap().unwrap();
        t.node.chain.connect_blocks(t.p, &[block(tip.hash, vec![tx(&[], &[(5000, p2pkh(3))], 23), other], 23)]).unwrap();
        forget_tried(&ours.txid);
        let now = arcade_app::now();
        assert_eq!(sweep(&t.node, now), 0);
        assert!(t.node.chain.mempool_tx(t.p.chain_no, &ours.txid).unwrap().is_none());
        assert!(dropped(&t.node, t.p, mine).is_empty(), "missing, not dropped yet");
        sweep(&t.node, now + MISSING_FOR);
        assert_eq!(dropped(&t.node, t.p, mine), vec![(ours.txid, 1300, 1900)]);
        assert!(dropped(&t.node, t.p, |s| s == p2pkh(9)).is_empty(), "not this wallet's");
        // Kept KEEP, then forgotten.
        sweep(&t.node, now + MISSING_FOR + KEEP + 1);
        assert!(remembered(&t.node).is_empty());
    }

    /// A transaction back in the pool after it was marked dropped waits again.
    #[test]
    fn back_in_the_pool_is_waiting_again() {
        let t = setup();
        let ours = tx(&[(t.spend.txid, 1)], &[(1700, p2pkh(1))], 31);
        note(&t.node, t.p, &ours);
        let now = arcade_app::now();
        sweep(&t.node, now);
        sweep(&t.node, now + MISSING_FOR);
        assert_eq!(dropped(&t.node, t.p, |s| s == p2pkh(1)).len(), 1);
        assert!(crate::mempoolshare::add(&t.node, t.p, &ours));
        sweep(&t.node, now + MISSING_FOR + 1);
        assert!(dropped(&t.node, t.p, |s| s == p2pkh(1)).is_empty());
        assert_eq!(remembered(&t.node)[t.p.key], 1);
    }
}
