//! Shared mempools (Martin 2026-11-10: "mempools should be shared"). A transaction that enters
//! this arcade's pool for a chain it keeps (from coin peers, a send, a claim, a message) goes to
//! every mesh link as an `mp` frame; an arcade keeping that chain checks it and pools it, and
//! passes it on once. A new link gets our current pool. So a payment, a claim or a group line
//! shows on every arcade within seconds, even one whose coin peers haven't relayed it.
//!
//!   {"t": "mp", "net": <net>, "tx": <raw hex>}
//!
//! Checks before pooling (a peer can't fill others' pools with junk): parses on that chain, not
//! a coinbase, at most MAX_TX bytes, not already known, room in the pool (POOL_TXS, POOL_BYTES),
//! every input spends an unspent coin or a pooled output, no input already spent in the pool, at
//! most MAX_ANCESTORS waiting ancestors, inputs cover the outputs, and every input's signature
//! verifies (arcade_wallet::verify). The pool itself still drops what blocks confirm or replace.

use crate::node::Node;
use arcade_chain::params::{by_net, ChainParams};
use arcade_chain::{OutPoint, Tx};
use arcade_mesh::link::{Frame, Link, LinkError};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

const MAX_TX: usize = 100_000;
/// The most transactions, and bytes, a pool takes from others (the coin's own peers and this
/// arcade's own transactions are not held to it).
pub const POOL_TXS: u64 = 5_000;
pub const POOL_BYTES: u64 = 20 << 20;
/// Waiting ancestors a transaction from others may have (as a coin node's default limit), so
/// a chain of children can't grow without end.
pub const MAX_ANCESTORS: usize = 25;
/// Frames kept for links to send (oldest dropped; a slow link resends its pool instead).
const RING: usize = 4000;
/// Transactions a new link gets from our pool, per chain.
const SNAPSHOT: usize = 500;

struct Ring {
    next: u64,
    frames: VecDeque<(u64, Value)>,
    /// Per peer: last sequence sent, and when it last ticked (a gap means a new link).
    cursors: HashMap<String, (u64, std::time::Instant)>,
}

static R: Mutex<Option<Ring>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut Ring) -> T) -> T {
    let mut g = R.lock().unwrap();
    f(g.get_or_insert_with(|| Ring { next: 0, frames: VecDeque::new(), cursors: HashMap::new() }))
}

/// Chains whose pool we keep and share: the home chain and installed ones, not while borrowed.
fn keeps(node: &Node, p: &ChainParams) -> bool {
    node.chain_params(p.key).is_some() && !crate::borrow::is_borrowed(p.key)
}

/// Pool `tx` (any source) and share it when it is new here. Returns whether it was new.
pub fn add(node: &Node, p: &ChainParams, tx: &Tx) -> bool {
    let new = node.chain.mempool_add(p, tx, arcade_app::now()).unwrap_or(false);
    if new && p.chain_no == node.params.chain_no {
        crate::msgsvc::WAKE.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if new && keeps(node, p) && tx.raw.len() <= MAX_TX {
        let f = json!({"t": "mp", "net": p.net, "tx": hex::encode(&tx.raw)});
        with(|r| {
            r.frames.push_back((r.next, f));
            r.next += 1;
            while r.frames.len() > RING {
                r.frames.pop_front();
            }
        });
    }
    new
}

/// An `mp` frame from a link.
pub fn on_frame(node: &Node, head: &Value) {
    let Some(p) = head["net"].as_str().and_then(by_net) else { return };
    if !keeps(node, p) {
        return;
    }
    let Some(raw) = head["tx"].as_str().filter(|h| h.len() <= MAX_TX * 2).and_then(|h| hex::decode(h).ok()) else { return };
    let Ok(tx) = Tx::parse_with(&raw, &p.format) else { return };
    if check(node, p, &tx).is_ok() {
        add(node, p, &tx);
    }
}

/// Is `tx` fit for the pool here (see the module notes)?
pub fn check(node: &Node, p: &ChainParams, tx: &Tx) -> Result<(), String> {
    let c = p.chain_no;
    if tx.is_coinbase() {
        return Err("coinbase".into());
    }
    if tx.raw.len() > MAX_TX {
        return Err(format!("larger than {MAX_TX} bytes"));
    }
    if node.chain.mempool_tx(c, &tx.txid).map_err(|e| e.to_string())?.is_some() || node.chain.tx_raw(p, &tx.txid).map_err(|e| e.to_string())?.is_some() {
        return Err("known".into());
    }
    let (n, bytes) = node.chain.mempool_size(c).map_err(|e| e.to_string())?;
    if n >= POOL_TXS || bytes + tx.raw.len() as u64 > POOL_BYTES {
        return Err("the pool is full".into());
    }
    let mut utxos = Vec::with_capacity(tx.inputs.len());
    let mut parents = Vec::new();
    for i in &tx.inputs {
        if node.chain.mempool_spender(c, &i.prevout).map_err(|e| e.to_string())?.is_some() {
            return Err("spends a pooled coin twice".into());
        }
        let (value, script) = match node.chain.coin(p, &i.prevout).map_err(|e| e.to_string())? {
            Some(coin) => (coin.value, coin.script),
            None => {
                let parent = node.chain.mempool_tx(c, &i.prevout.txid).map_err(|e| e.to_string())?.ok_or("spends an unknown coin")?;
                let t = Tx::parse_with(&parent, &p.format).map_err(|e| e.to_string())?;
                if t.txid != i.prevout.txid {
                    return Err("spends an unknown coin".into());
                }
                let o = t.outputs.get(i.prevout.vout as usize).ok_or("spends an unknown output")?;
                parents.push(t.txid);
                (o.value, o.script_pubkey.clone())
            }
        };
        utxos.push(arcade_wallet::tx::Utxo { outpoint: OutPoint { txid: i.prevout.txid, vout: i.prevout.vout }, value, script });
    }
    if ancestors(node, p, parents)? > MAX_ANCESTORS {
        return Err(format!("more than {MAX_ANCESTORS} waiting ancestors"));
    }
    let ins = utxos.iter().try_fold(0u64, |a, u| a.checked_add(u.value)).ok_or("inputs overflow")?;
    let outs = tx.outputs.iter().try_fold(0u64, |a, o| a.checked_add(o.value)).ok_or("outputs overflow")?;
    if ins < outs {
        return Err("outputs exceed inputs".into());
    }
    // Every input must verify, and an input whose kind of coin can't be checked here (another
    // P2SH script, bare multisig, P2WSH, a Taproot script path) is refused too. A pooled spend
    // is first-seen: it hides the coin from this arcade's own sends, shows as a pending payment
    // and makes a tag claim look taken, so an unchecked one must not get in. Such
    // transactions still reach every arcade once a block confirms them, or through the coin's
    // own peers (whose nodes check every script), just not from other arcades.
    if let Err((n, e)) = arcade_wallet::verify::all(p, tx, &utxos) {
        return Err(format!("input {n}: {e}"));
    }
    Ok(())
}

/// How many distinct waiting ancestors a transaction with these pooled parents has (counting
/// stops just past MAX_ANCESTORS).
fn ancestors(node: &Node, p: &ChainParams, parents: Vec<arcade_chain::Hash256>) -> Result<usize, String> {
    let mut seen = std::collections::HashSet::new();
    let mut todo = parents;
    while let Some(t) = todo.pop() {
        if !seen.insert(t) {
            continue;
        }
        if seen.len() > MAX_ANCESTORS {
            break;
        }
        let Some(raw) = node.chain.mempool_tx(p.chain_no, &t).map_err(|e| e.to_string())? else { continue };
        let Ok(tx) = Tx::parse_with(&raw, &p.format) else { continue };
        for i in &tx.inputs {
            if !seen.contains(&i.prevout.txid) && node.chain.mempool_tx(p.chain_no, &i.prevout.txid).map_err(|e| e.to_string())?.is_some() {
                todo.push(i.prevout.txid);
            }
        }
    }
    Ok(seen.len())
}

/// Per link, each round: new pooled transactions; a link new to us (or back after a gap) gets
/// our pool first.
pub fn tick(node: &Node, peer: &str, link: &mut Link) -> Result<(), LinkError> {
    let now = std::time::Instant::now();
    let (fresh, out) = with(|r| {
        let oldest = r.frames.front().map(|f| f.0).unwrap_or(r.next);
        let (from, fresh) = match r.cursors.get(peer) {
            Some((s, at)) if now.duration_since(*at).as_secs() < 60 && *s >= oldest => (*s, false),
            _ => (r.next, true),
        };
        let out: Vec<Value> = r.frames.iter().filter(|f| f.0 >= from).map(|f| f.1.clone()).collect();
        r.cursors.insert(peer.to_string(), (r.next, now));
        (fresh, out)
    });
    if fresh {
        let mut all: Vec<&'static ChainParams> = vec![node.params];
        all.extend(node.extra.lock().unwrap().values().map(|r| r.params));
        for p in all.into_iter().filter(|p| keeps(node, p)) {
            let mut pool = node.chain.mempool(p.chain_no).unwrap_or_default();
            // Oldest first, so parents arrive before their children.
            pool.sort_by_key(|x| x.0);
            for (_, raw) in pool.into_iter().filter(|x| x.1.len() <= MAX_TX).take(SNAPSHOT) {
                link.send(&Frame::new(json!({"t": "mp", "net": p.net, "tx": hex::encode(raw)})))?;
            }
        }
    }
    for f in out {
        link.send(&Frame::new(f))?;
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::sitesvc::tests::{block, p2pkh, setup_with, tx, T};
    use arcade_chain::Hash256;
    use arcade_wallet::secp256k1::{PublicKey, SecretKey, SECP256K1};
    use arcade_wallet::tx::{sign_keyhash, sign_p2pkh, TxOutSpec, Utxo};

    pub(crate) fn key() -> SecretKey {
        SecretKey::from_slice(&[0x31; 32]).unwrap()
    }

    /// The test chain with SegWit on, where block 3 pays our key 1500 at its P2WPKH script
    /// (vout 0) and 1400 at its P2PKH one (vout 1).
    pub(crate) fn funded() -> (T, Vec<Utxo>) {
        let t = setup_with(|p| p.format.segwit = true);
        let h = arcade_ledger::address::hash160(&PublicKey::from_secret_key(SECP256K1, &key()).serialize());
        let (w, k) = (arcade_ledger::address::p2wpkh_script(&h), arcade_ledger::address::p2pkh_script(&h));
        let fund = tx(&[(t.spend.txid, 0)], &[(1500, w.clone()), (1400, k.clone())], 0);
        let tip = t.node.chain.tip(t.p.chain_no).unwrap().unwrap();
        t.node.chain.connect_blocks(t.p, &[block(tip.hash, vec![tx(&[], &[(5000, p2pkh(3))], 3), fund.clone()], 4)]).unwrap();
        let coins = vec![
            Utxo { outpoint: OutPoint { txid: fund.txid, vout: 0 }, value: 1500, script: w },
            Utxo { outpoint: OutPoint { txid: fund.txid, vout: 1 }, value: 1400, script: k },
        ];
        (t, coins)
    }

    #[test]
    fn every_input_is_verified() {
        let (t, coins) = funded();
        let outs = vec![TxOutSpec { value: 2800, script: p2pkh(5) }];
        let good = sign_keyhash(t.p, 1, &coins, &outs, &key(), 0).unwrap();
        assert!(good.has_witness());
        assert_eq!(check(&t.node, t.p, &good), Ok(()));
        // A garbage signature on the P2WPKH input: refused (only P2PKH inputs were checked).
        let mut bad = good.clone();
        bad.inputs[0].witness[0][10] ^= 1;
        assert!(check(&t.node, t.p, &bad).unwrap_err().starts_with("input 0: bad signature"));
        // No witness at all.
        let mut bare = good.clone();
        bare.inputs[0].witness.clear();
        assert!(check(&t.node, t.p, &bare).unwrap_err().starts_with("input 0"));
        // Another key's signature on the P2PKH input.
        let other = SecretKey::from_slice(&[0x32; 32]).unwrap();
        let theirs = sign_p2pkh(t.p, &coins[1..], &[TxOutSpec { value: 1000, script: p2pkh(5) }], &other, 0);
        assert!(check(&t.node, t.p, &theirs).unwrap_err().starts_with("input 0: bad signature"));
        assert!(add(&t.node, t.p, &good));
    }

    #[test]
    fn waiting_ancestors_are_capped() {
        let (t, coins) = funded();
        let child_of = |u: &Utxo| sign_p2pkh(t.p, &[u.clone()], &[TxOutSpec { value: u.value - 1, script: u.script.clone() }], &key(), 0);
        let mut prev = coins[1].clone();
        for n in 0..=MAX_ANCESTORS {
            let child = child_of(&prev);
            assert_eq!(check(&t.node, t.p, &child), Ok(()), "child {n} has {n} waiting ancestors");
            assert!(add(&t.node, t.p, &child));
            prev = Utxo { outpoint: OutPoint { txid: child.txid, vout: 0 }, value: prev.value - 1, script: prev.script };
        }
        assert!(check(&t.node, t.p, &child_of(&prev)).unwrap_err().contains("ancestors"));
        // The other coin is unrelated: still fine.
        let other = sign_keyhash(t.p, 1, &coins[..1], &[TxOutSpec { value: 1000, script: p2pkh(6) }], &key(), 0).unwrap();
        assert_eq!(check(&t.node, t.p, &other), Ok(()));
    }

    #[test]
    fn a_full_pool_takes_nothing_more_from_others() {
        let (t, coins) = funded();
        for n in 0..POOL_TXS as u32 {
            let mut h = [0xeeu8; 32];
            h[..4].copy_from_slice(&n.to_le_bytes());
            assert!(t.node.chain.mempool_add(t.p, &tx(&[(Hash256(h), 0)], &[(1, p2pkh(7))], 0), 1).unwrap());
        }
        let good = sign_keyhash(t.p, 1, &coins, &[TxOutSpec { value: 2800, script: p2pkh(5) }], &key(), 0).unwrap();
        assert_eq!(check(&t.node, t.p, &good), Err("the pool is full".into()));
    }
}
