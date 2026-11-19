//! Shared mempools (Martin 2026-11-10: "mempools should be shared"). A transaction that enters
//! this arcade's pool for a chain it keeps (from coin peers, a send, a claim, a message) goes to
//! every mesh link as an `mp` frame; an arcade keeping that chain checks it and pools it, and
//! passes it on once. A new link gets our current pool. So a payment, a claim or a group line
//! shows on every arcade within seconds, even one whose coin peers haven't relayed it.
//!
//!   {"t": "mp", "net": <net>, "tx": <raw hex>}
//!
//! The frame carries the transaction as hex in its head, so one over about 32 KB can't be sent
//! on a link (MAX_FRAME): it is pooled here and goes to the coin's own peers, but is not passed
//! between arcades.
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
/// Virtual bytes a transaction and its waiting ancestors may add up to (as a coin node's default
/// ancestor size limit, 101 kvB).
pub const MAX_ANCESTOR_VBYTES: usize = 101_000;
/// How long a passed-on transaction (one with an input this arcade can't check, sent to the coin's
/// own peers without pooling) is remembered, so its children can spend its outputs before a block
/// confirms it.
const PASSED_FOR: u64 = 48 * 3600;
/// Passed-on transactions remembered per chain, at most (oldest forgotten first).
const PASSED_MAX: usize = 5_000;

/// Transactions passed on to a coin's peers without being pooled here (an inscription's reveal and
/// the like, whose input script this arcade can't check), by chain and txid. Their outputs are
/// what the next reveal of a chain spends (TxTOK 2026-11-18: a chain of reveals stalled at one per
/// block, "spends an unknown coin"). A child of one is passed on too, never pooled.
#[derive(Default)]
struct Passed {
    /// (chain, txid) -> (seen, raw)
    txs: HashMap<(u16, arcade_chain::Hash256), (u64, Vec<u8>)>,
    /// (chain, coin) -> the passed-on transaction spending it
    spent: HashMap<(u16, OutPoint), arcade_chain::Hash256>,
}

static PASSED: Mutex<Option<Passed>> = Mutex::new(None);

fn passed<T>(f: impl FnOnce(&mut Passed) -> T) -> T {
    let mut g = PASSED.lock().unwrap();
    f(g.get_or_insert_with(Passed::default))
}

impl Passed {
    fn forget(&mut self, p: &ChainParams, txid: &arcade_chain::Hash256) {
        if let Some((_, raw)) = self.txs.remove(&(p.chain_no, *txid)) {
            if let Ok(t) = Tx::parse_with(&raw, &p.format) {
                for i in &t.inputs {
                    if self.spent.get(&(p.chain_no, i.prevout)) == Some(txid) {
                        self.spent.remove(&(p.chain_no, i.prevout));
                    }
                }
            }
        }
    }
}

/// Remember a passed-on transaction (and forget old ones).
fn remember_passed(p: &ChainParams, tx: &Tx) {
    let now = arcade_app::now();
    passed(|m| {
        if m.txs.contains_key(&(p.chain_no, tx.txid)) {
            return;
        }
        let old: Vec<arcade_chain::Hash256> = m.txs.iter().filter(|(k, v)| k.0 == p.chain_no && now.saturating_sub(v.0) >= PASSED_FOR).map(|(k, _)| k.1).collect();
        for t in old {
            m.forget(p, &t);
        }
        if m.txs.keys().filter(|k| k.0 == p.chain_no).count() >= PASSED_MAX {
            if let Some(oldest) = m.txs.iter().filter(|(k, _)| k.0 == p.chain_no).min_by_key(|(_, v)| v.0).map(|(k, _)| k.1) {
                m.forget(p, &oldest);
            }
        }
        m.txs.insert((p.chain_no, tx.txid), (now, tx.raw.clone()));
        for i in &tx.inputs {
            m.spent.insert((p.chain_no, i.prevout), tx.txid);
        }
    });
}

/// A passed-on transaction still remembered (and not yet in a block).
fn passed_tx(node: &Node, p: &ChainParams, txid: &arcade_chain::Hash256) -> Option<Tx> {
    let raw = passed(|m| m.txs.get(&(p.chain_no, *txid)).map(|v| v.1.clone()))?;
    if node.chain.tx_raw(p, txid).ok().flatten().is_some() {
        passed(|m| m.forget(p, txid));
        return None;
    }
    Tx::parse_with(&raw, &p.format).ok().filter(|t| t.txid == *txid)
}

/// Every passed-on transaction of a chain still waiting (not in a block): (seen, raw), oldest first.
/// The /chain/<key> RPC lists them with the pool (getrawtransaction, getrawmempool, gettxout).
pub fn passed_pool(node: &Node, p: &ChainParams) -> Vec<(u64, Vec<u8>)> {
    let mut all: Vec<(arcade_chain::Hash256, u64, Vec<u8>)> =
        passed(|m| m.txs.iter().filter(|(k, _)| k.0 == p.chain_no).map(|(k, v)| (k.1, v.0, v.1.clone())).collect());
    all.retain(|(id, ..)| node.chain.tx_raw(p, id).ok().flatten().is_none());
    all.sort_by_key(|x| x.1);
    all.into_iter().map(|(_, seen, raw)| (seen, raw)).collect()
}

/// The passed-on transaction spending this coin, if any.
pub fn passed_spender_of(p: &ChainParams, op: &OutPoint) -> Option<arcade_chain::Hash256> {
    passed(|m| m.spent.get(&(p.chain_no, *op)).copied())
}

/// Whether a passed-on transaction other than `me` already spends this coin.
fn passed_spender(p: &ChainParams, op: &OutPoint, me: &arcade_chain::Hash256) -> bool {
    passed(|m| m.spent.get(&(p.chain_no, *op)).is_some_and(|t| t != me))
}

/// A transaction's virtual size: witness bytes count a quarter.
pub fn vbytes(tx: &Tx) -> usize {
    if !tx.has_witness() {
        return tx.raw.len();
    }
    let wit: usize = tx.inputs.iter().map(|i| 1 + i.witness.iter().map(|w| w.len() + 1 + (w.len() >= 0xfd) as usize * 2).sum::<usize>()).sum::<usize>() + 2;
    let base = tx.raw.len().saturating_sub(wit);
    (base * 4 + wit).div_ceil(4)
}
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
    if let Some(f) = (new && keeps(node, p)).then(|| frame(p.net, &tx.raw)).flatten() {
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

/// The `mp` frame for `raw`, or None when it is too big to send on a link (see the top).
fn frame(net: &str, raw: &[u8]) -> Option<Value> {
    let f = Frame::new(json!({"t": "mp", "net": net, "tx": hex::encode(raw)}));
    arcade_mesh::link::fits(&f).then_some(f.head)
}

/// An `mp` frame from a link.
pub fn on_frame(node: &Node, head: &Value) {
    // The home chain as this arcade runs it (its own parameters), any other by its network name.
    let net = head["net"].as_str().unwrap_or("");
    let Some(p) = (if net == node.params.net { Some(node.params) } else { by_net(net) }) else { return };
    if !keeps(node, p) {
        return;
    }
    let Some(raw) = head["tx"].as_str().filter(|h| h.len() <= MAX_TX * 2).and_then(|h| hex::decode(h).ok()) else { return };
    let Ok(tx) = Tx::parse_with(&raw, &p.format) else { return };
    if check(node, p, &tx).is_ok() && add(node, p, &tx) {
        to_network(node, p, raw);
    }
}

/// Home-chain transactions from the mesh waiting for the coin peer, at most (the outbox is sent and
/// emptied each sync round; with no coin peer for a long while, the oldest go first).
const OUTBOX_FROM_MESH: usize = 500;
/// Installed-chain transactions from the mesh being sent to that chain's own peers at once, at most.
const SENDING_FROM_MESH: usize = 8;
static SENDING: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// A checked transaction new to this arcade, from the mesh, also goes to the coin's own network
/// (Martin 2026-11-12). Pooled and passed between arcades only, it reached no miner: an arcade the
/// coin's nodes refuse (banned, a network that drops the coin's port) or a website's visitor sent a
/// transaction that every arcade held and none handed on. The coin's nodes drop one they have.
fn to_network(node: &Node, p: &'static ChainParams, raw: Vec<u8>) {
    use std::sync::atomic::Ordering;
    if p.chain_no == node.params.chain_no {
        let mut out = node.tx_outbox.lock().unwrap();
        if out.len() >= OUTBOX_FROM_MESH {
            out.remove(0);
        }
        out.push(raw);
        return;
    }
    if SENDING.fetch_add(1, Ordering::Relaxed) >= SENDING_FROM_MESH {
        SENDING.fetch_sub(1, Ordering::Relaxed);
        return;
    }
    // As pass_on sends an installed chain's: straight to a few of its own peers.
    let peers: Vec<String> = node.coin_candidates(p).into_iter().take(4).collect();
    std::thread::spawn(move || {
        for a in peers {
            if let Ok(mut peer) = arcade_chain::p2p::Peer::connect(p, &a, std::time::Duration::from_secs(10)) {
                let _ = peer.send_tx(&raw);
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
        SENDING.fetch_sub(1, Ordering::Relaxed);
    });
}

/// Check every pooled transaction again, oldest first, as a new one is checked: one the
/// coin's network would refuse (a bad signature, a mining reward spent too early: taken in by
/// an arcade version without those checks, or a reward whose block a reorg moved) goes, with
/// the transactions that spend its outputs. Returns the txids dropped and why.
pub fn recheck(node: &Node, p: &ChainParams) -> Vec<(String, String)> {
    let c = p.chain_no;
    let mut pooled = node.chain.mempool(c).unwrap_or_default();
    pooled.sort_by_key(|x| x.0);
    let mut dropped = Vec::new();
    for (seen, raw) in pooled {
        let Ok(tx) = Tx::parse_with(&raw, &p.format) else { continue };
        if node.chain.mempool_remove(p, &tx.txid).ok() != Some(true) {
            continue;
        }
        let why = match sort(node, p, &tx) {
            Ok(None) => None,
            Ok(Some((n, e))) => Some(format!("input {n} can't be checked here ({e})")),
            Err(e) => Some(e),
        };
        match why {
            None => {
                let _ = node.chain.mempool_add(p, &tx, seen);
            }
            Some(e) => dropped.push((tx.txid.to_hex(), e)),
        }
    }
    dropped
}

/// Whether a coin is a mining reward too young to spend at a chain height of `tip` (as the
/// wallet's own sends count it, claims::spendable).
pub fn immature(p: &ChainParams, coinbase: bool, height: u32, tip: u32) -> bool {
    coinbase && height + p.coinbase_maturity > tip
}

fn tip_of(node: &Node, p: &ChainParams) -> u32 {
    node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height).unwrap_or(0)
}

/// Is `tx` fit for the pool here (see the module notes)?
pub fn check(node: &Node, p: &ChainParams, tx: &Tx) -> Result<(), String> {
    match sort(node, p, tx)? {
        None => Ok(()),
        Some((n, e)) => Err(format!("input {n}: {e}")),
    }
}

/// `check`, except that an input of a kind this arcade can't check is not a refusal: Ok(None)
/// when every input checked out (fit for the pool), Ok(Some(that input)) when everything else is
/// in order but some input couldn't be checked. Such a transaction may be passed on to the
/// coin's own peers, whose nodes check every script (`pass_on`), but is never pooled or shared.
pub fn sort(node: &Node, p: &ChainParams, tx: &Tx) -> Result<Option<(usize, arcade_wallet::verify::Refused)>, String> {
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
    // The first input that spends a passed-on transaction's output: the whole transaction is then
    // passed on too (its parent was never checked here), not pooled.
    let mut unchecked_parent: Option<usize> = None;
    for (n, i) in tx.inputs.iter().enumerate() {
        if node.chain.mempool_spender(c, &i.prevout).map_err(|e| e.to_string())?.is_some() || passed_spender(p, &i.prevout, &tx.txid) {
            return Err("spends a pooled coin twice".into());
        }
        let (value, script) = match node.chain.coin(p, &i.prevout).map_err(|e| e.to_string())? {
            // A mining reward can't be spent until it is mature: the coin's network refuses
            // such a spend (bad-txns-premature-spend-of-coinbase), so it must not wait here.
            Some(coin) if immature(p, coin.coinbase, coin.height, tip_of(node, p)) => {
                return Err(format!("spends a mining reward that isn't mature yet (from block {}, spendable after block {})", coin.height, coin.height + p.coinbase_maturity));
            }
            Some(coin) => (coin.value, coin.script),
            None => {
                let t = match node.chain.mempool_tx(c, &i.prevout.txid).map_err(|e| e.to_string())? {
                    Some(parent) => Tx::parse_with(&parent, &p.format).map_err(|e| e.to_string())?,
                    // A parent passed on to the coin's peers (a reveal before this one): its outputs
                    // are spendable, but this transaction is passed on too.
                    None => match passed_tx(node, p, &i.prevout.txid) {
                        Some(t) => {
                            unchecked_parent.get_or_insert(n);
                            t
                        }
                        None => return Err("spends an unknown coin".into()),
                    },
                };
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
    let (count, vb) = ancestors(node, p, parents)?;
    if count > MAX_ANCESTORS {
        return Err(format!("too-long-mempool-chain: more than {MAX_ANCESTORS} waiting ancestors"));
    }
    if vb + vbytes(tx) > MAX_ANCESTOR_VBYTES {
        return Err(format!("too-long-mempool-chain: waiting ancestors and this transaction exceed {MAX_ANCESTOR_VBYTES} virtual bytes"));
    }
    let ins = utxos.iter().try_fold(0u64, |a, u| a.checked_add(u.value)).ok_or("inputs overflow")?;
    let outs = tx.outputs.iter().try_fold(0u64, |a, o| a.checked_add(o.value)).ok_or("outputs overflow")?;
    if ins < outs {
        return Err("outputs exceed inputs".into());
    }
    // Every input must verify, and an input whose kind of coin can't be checked here (another
    // P2SH script, bare multisig, P2WSH, a Taproot script path) keeps it out of the pool too. A
    // pooled spend is first-seen: it hides the coin from this arcade's own sends, shows as a
    // pending payment and makes a tag claim look taken, so an unchecked one must not get in. Such
    // transactions still reach every arcade once a block confirms them, or through the coin's
    // own peers (whose nodes check every script, `pass_on`), just not from other arcades.
    match arcade_wallet::verify::each(p, tx, &utxos).map_err(|(n, e)| format!("input {n}: {e}"))? {
        None => Ok(unchecked_parent.map(|n| (n, arcade_wallet::verify::Refused::Unknown("its coin comes from a transaction passed on unchecked".into())))),
        some => Ok(some),
    }
}

/// How many distinct waiting ancestors a transaction with these pooled parents has (counting
/// stops just past MAX_ANCESTORS).
fn ancestors(node: &Node, p: &ChainParams, parents: Vec<arcade_chain::Hash256>) -> Result<(usize, usize), String> {
    let waiting = |t: &arcade_chain::Hash256| -> Result<Option<Tx>, String> {
        Ok(match node.chain.mempool_tx(p.chain_no, t).map_err(|e| e.to_string())? {
            Some(raw) => Tx::parse_with(&raw, &p.format).ok(),
            None => passed_tx(node, p, t),
        })
    };
    let mut seen = std::collections::HashSet::new();
    let mut vb = 0usize;
    let mut todo = parents;
    while let Some(t) = todo.pop() {
        if !seen.insert(t) {
            continue;
        }
        if seen.len() > MAX_ANCESTORS {
            break;
        }
        let Some(tx) = waiting(&t)? else { continue };
        vb += vbytes(&tx);
        for i in &tx.inputs {
            if !seen.contains(&i.prevout.txid) && waiting(&i.prevout.txid)?.is_some() {
                todo.push(i.prevout.txid);
            }
        }
    }
    Ok((seen.len(), vb))
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
            for f in pool.into_iter().filter_map(|x| frame(p.net, &x.1)).take(SNAPSHOT) {
                link.send(&Frame::new(f))?;
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

    /// Pooled transactions the network would refuse (taken in without checks) go at the
    /// re-check, with what spends them; good ones stay, with their first-seen time.
    #[test]
    fn a_recheck_drops_what_the_network_would_refuse() {
        let (t, coins) = funded();
        let mine = coins[1].script.clone();
        let good = sign_keyhash(t.p, 1, &coins[..1], &[TxOutSpec { value: 1400, script: mine.clone() }], &key(), 0).unwrap();
        assert!(add(&t.node, t.p, &good));
        // Spends the other coin with no signature, and a child of it: pooled the old way.
        let bad = tx(&[(coins[1].outpoint.txid, coins[1].outpoint.vout)], &[(1300, mine.clone())], 1);
        let child = tx(&[(bad.txid, 0)], &[(1200, p2pkh(4))], 2);
        assert!(t.node.chain.mempool_add(t.p, &bad, 5).unwrap() && t.node.chain.mempool_add(t.p, &child, 6).unwrap());
        let seen_good = t.node.chain.mempool(t.p.chain_no).unwrap().into_iter().find(|x| Tx::parse_with(&x.1, &t.p.format).unwrap().txid == good.txid).unwrap().0;
        let dropped = recheck(&t.node, t.p);
        let ids: Vec<&str> = dropped.iter().map(|d| d.0.as_str()).collect();
        assert_eq!(ids, vec![bad.txid.to_hex(), child.txid.to_hex()], "{dropped:?}");
        let left = t.node.chain.mempool(t.p.chain_no).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].0, seen_good, "kept with its first-seen time");
        assert!(t.node.chain.mempool_spender(t.p.chain_no, &coins[1].outpoint).unwrap().is_none(), "the coin is free again");
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
    fn a_transaction_from_the_mesh_goes_to_the_coin_network_once() {
        let (t, coins) = funded();
        let good = sign_keyhash(t.p, 1, &coins, &[TxOutSpec { value: 2800, script: p2pkh(5) }], &key(), 0).unwrap();
        let frame = json!({"t": "mp", "net": t.p.net, "tx": hex::encode(&good.raw)});
        on_frame(&t.node, &frame);
        assert_eq!(t.node.tx_outbox.lock().unwrap().clone(), vec![good.raw.clone()], "pooled and handed to the coin peer");
        on_frame(&t.node, &frame);
        assert_eq!(t.node.tx_outbox.lock().unwrap().len(), 1, "heard again: not sent again");
        // One that fails the checks goes nowhere.
        let bad = sign_keyhash(t.p, 1, &coins, &[TxOutSpec { value: 2800, script: p2pkh(6) }], &key(), 0).unwrap();
        on_frame(&t.node, &json!({"t": "mp", "net": t.p.net, "tx": hex::encode(&bad.raw)}));
        assert_eq!(t.node.tx_outbox.lock().unwrap().len(), 1, "a double spend of a pooled coin is not sent");
    }

    #[test]
    fn a_transaction_too_big_to_send_is_pooled_but_not_shared() {
        let t = setup_with(|_| {});
        // The ring is shared by every test here: look for these transactions only.
        let shared = |raw: &[u8]| with(|r| r.frames.iter().any(|f| f.1["tx"] == hex::encode(raw)));
        // About 40 KB: under MAX_TX, but its hex is over the frame limit.
        let big = tx(&[(Hash256([0xb1; 32]), 0)], &[(1, vec![0x6a; 40_000])], 0);
        assert!(big.raw.len() <= MAX_TX && frame(t.p.net, &big.raw).is_none());
        assert!(add(&t.node, t.p, &big));
        assert!(!shared(&big.raw));
        let pool = t.node.chain.mempool(t.p.chain_no).unwrap();
        assert!(pool.iter().any(|x| x.1 == big.raw), "kept here");
        // A normal one still is shared.
        let small = tx(&[(Hash256([0xb2; 32]), 0)], &[(1, p2pkh(8))], 0);
        assert!(add(&t.node, t.p, &small));
        assert!(shared(&small.raw));
    }

    /// A transaction spending output 0 of `prev` with `script_sig`, paying `value` to `out` and, if
    /// `pad` > 0, an OP_RETURN output of that many bytes (to make it big).
    pub(crate) fn raw_spend(prev: &Hash256, script_sig: &[u8], value: u64, out: &[u8], pad: usize) -> Vec<u8> {
        let push = |o: &mut Vec<u8>, n: usize| {
            if n < 0xfd {
                o.push(n as u8);
            } else {
                o.push(0xfd);
                o.extend_from_slice(&(n as u16).to_le_bytes());
            }
        };
        let mut o = 1i32.to_le_bytes().to_vec();
        o.push(1);
        o.extend_from_slice(&prev.0);
        o.extend_from_slice(&0u32.to_le_bytes());
        push(&mut o, script_sig.len());
        o.extend_from_slice(script_sig);
        o.extend_from_slice(&u32::MAX.to_le_bytes());
        o.push(if pad > 0 { 2 } else { 1 });
        o.extend_from_slice(&value.to_le_bytes());
        push(&mut o, out.len());
        o.extend_from_slice(out);
        if pad > 0 {
            o.extend_from_slice(&0u64.to_le_bytes());
            push(&mut o, pad);
            o.push(0x6a);
            o.extend(std::iter::repeat(0u8).take(pad - 1));
        }
        o.extend_from_slice(&0u32.to_le_bytes());
        o
    }

    /// A confirmed coin at a P2SH script this arcade can't check (as an inscription's commit
    /// pays): (the chain, the coin's txid, the script, the redeem script's push).
    pub(crate) fn p2sh_funded(salt: u64) -> (T, Hash256, Vec<u8>, Vec<u8>) {
        let (t, _) = funded();
        let redeem = [0x51u8, 0x75, 0x51];
        let sh = arcade_ledger::address::p2sh_script(&arcade_ledger::address::hash160(&redeem));
        // Each test its own coin: the passed-on table is shared by every test.
        let fund = tx(&[(t.spend.txid, 1)], &[(100_000 + salt, sh.clone())], 7);
        let tip = t.node.chain.tip(t.p.chain_no).unwrap().unwrap();
        t.node.chain.connect_blocks(t.p, &[block(tip.hash, vec![tx(&[], &[(5000, p2pkh(3))], 8), fund.clone()], 5)]).unwrap();
        (t, fund.txid, sh, vec![3, 0x51, 0x75, 0x51])
    }

    /// A chain of reveals (each spending the one before, all waiting): every one is passed on to the
    /// coin's peers as its parent was, up to the ancestor limit; none waits for a block
    /// (TxTOK 2026-11-18: reveal 2 got "spends an unknown coin" until reveal 1 was mined).
    #[test]
    fn a_chain_of_unchecked_reveals_is_passed_on_up_to_the_ancestor_limit() {
        let (t, fund, sh, sig) = p2sh_funded(1);
        let mut prev = fund;
        let mut value = 100_001u64;
        for n in 0..=MAX_ANCESTORS {
            value -= 100;
            let raw = raw_spend(&prev, &sig, value, &sh, 0);
            let tx = Tx::parse_with(&raw, &t.p.format).unwrap();
            let sorted = sort(&t.node, t.p, &tx);
            assert!(matches!(sorted, Ok(Some(_))), "reveal {n} ({n} waiting ancestors): {sorted:?}");
            pass_on(&t.node, t.p, raw);
            prev = tx.txid;
        }
        assert!(t.node.chain.mempool(t.p.chain_no).unwrap().is_empty(), "none pooled");
        assert_eq!(t.node.tx_outbox.lock().unwrap().len(), MAX_ANCESTORS + 1, "all passed on, in order");
        let next = Tx::parse_with(&raw_spend(&prev, &sig, value - 100, &sh, 0), &t.p.format).unwrap();
        assert!(sort(&t.node, t.p, &next).unwrap_err().contains("too-long-mempool-chain"));
    }

    /// A second spend of a passed-on transaction's output is refused, as for a pooled one.
    #[test]
    fn a_passed_on_output_is_spent_once() {
        let (t, fund, sh, sig) = p2sh_funded(2);
        let r1 = raw_spend(&fund, &sig, 99_000, &sh, 0);
        let t1 = Tx::parse_with(&r1, &t.p.format).unwrap();
        pass_on(&t.node, t.p, r1);
        let a = Tx::parse_with(&raw_spend(&t1.txid, &sig, 98_000, &sh, 0), &t.p.format).unwrap();
        assert!(matches!(sort(&t.node, t.p, &a), Ok(Some(_))));
        pass_on(&t.node, t.p, a.raw.clone());
        let b = Tx::parse_with(&raw_spend(&t1.txid, &sig, 97_000, &sh, 0), &t.p.format).unwrap();
        assert_eq!(sort(&t.node, t.p, &b).unwrap_err(), "spends a pooled coin twice");
    }

    /// Ancestors are also capped by their virtual size (101 kvB with the transaction itself).
    #[test]
    fn a_chain_is_capped_by_size_too() {
        let (t, fund, sh, sig) = p2sh_funded(3);
        let mut prev = fund;
        let mut value = 100_003u64;
        let mut n = 0;
        loop {
            value -= 100;
            let tx = Tx::parse_with(&raw_spend(&prev, &sig, value, &sh, 30_000), &t.p.format).unwrap();
            match sort(&t.node, t.p, &tx) {
                Ok(Some(_)) => {
                    pass_on(&t.node, t.p, tx.raw.clone());
                    prev = tx.txid;
                    n += 1;
                }
                Err(e) => {
                    assert!(e.contains("virtual bytes"), "{e}");
                    break;
                }
                Ok(None) => panic!("an unchecked parent was pooled"),
            }
            assert!(n < 10);
        }
        assert_eq!(n, 3, "three of ~30 kB fit under 101 kvB, a fourth doesn't");
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

/// Send `raw` to the coin's own network without pooling it: the home chain's through its P2P
/// link, an installed chain we lend straight to a few of its own peers.
pub fn pass_on(node: &Node, p: &'static ChainParams, raw: Vec<u8>) {
    // Remembered, so a child spending its outputs can follow it before a block confirms it.
    if let Ok(tx) = Tx::parse_with(&raw, &p.format) {
        remember_passed(p, &tx);
    }
    if p.chain_no == node.params.chain_no {
        node.tx_outbox.lock().unwrap().push(raw);
        return;
    }
    // In order, through one sender per chain: a reveal must reach the coin's peers after the
    // transaction it spends (a thread per transaction could send a child first).
    let start = {
        let mut q = SEND_Q.lock().unwrap();
        let q = q.get_or_insert_with(HashMap::new);
        let e = q.entry(p.chain_no).or_insert_with(|| (VecDeque::new(), false));
        e.0.push_back(raw);
        let start = !e.1;
        e.1 = true;
        start
    };
    if start {
        let peers: Vec<String> = node.coin_candidates(p).into_iter().take(4).collect();
        std::thread::Builder::new().name(format!("pass on {}", p.key)).spawn(move || send_in_order(p, peers)).ok();
    }
}

/// Transactions waiting to be passed on, per chain, and whether that chain's sender runs.
static SEND_Q: Mutex<Option<HashMap<u16, (VecDeque<Vec<u8>>, bool)>>> = Mutex::new(None);

/// One chain's sender: everything queued, oldest first, to each peer over one connection, until
/// the queue is empty.
fn send_in_order(p: &'static ChainParams, peers: Vec<String>) {
    loop {
        let batch: Vec<Vec<u8>> = {
            let mut q = SEND_Q.lock().unwrap();
            let e = q.get_or_insert_with(HashMap::new).entry(p.chain_no).or_insert_with(|| (VecDeque::new(), false));
            if e.0.is_empty() {
                e.1 = false;
                return;
            }
            e.0.drain(..).collect()
        };
        for a in &peers {
            if let Ok(mut peer) = arcade_chain::p2p::Peer::connect(p, a, std::time::Duration::from_secs(10)) {
                for raw in &batch {
                    let _ = peer.send_tx(raw);
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
    }
}
