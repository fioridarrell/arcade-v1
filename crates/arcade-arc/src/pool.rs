//! Transactions waiting for a block (docs/ARCADECHAIN.md §8, §14.10).
//!
//! Nothing here is a rule of the chain: a block may carry any transaction the rules in
//! `coins.rs` allow. These are the rules an arcade keeps for what it holds, passes on and
//! puts into its own blocks, so that holding and passing on can't be had for free:
//!
//! - a transaction must be valid in the next block, against the coins as they are with the
//!   waiting transactions before it applied;
//! - it pays at least the fee floor (0.001 ARCADE per 1,000 bytes);
//! - the first spend of a coin stays: a second one is refused, whatever it pays;
//! - at most 25 waiting transactions stand in a row, each spending the one before;
//! - the pool holds 5 MB. When it is full, a transaction gets in only by paying more per
//!   byte than the cheapest ones, which leave with everything that spends them.
//!
//! After every change of tip the pool is gone through again. What a signature covers never
//! changes (a coin's script and amount are fixed by the id of the transaction that made
//! it), so a waiting transaction's scripts are run once, when it comes in; afterwards only
//! its coins are looked up again.

use crate::coins::{self, Coin, Coins, TxError, MAX_SIG_CHECKS};
use crate::params::{COINBASE_MATURITY, MIN_FEE_PER_KB};
use crate::script;
use arcade_chain::hash::Hash256;
use arcade_chain::{OutPoint, Tx};
use std::collections::{BTreeMap, HashMap, HashSet};

/// Bytes of transactions the pool holds.
pub const MAX_POOL_BYTES: usize = 5_000_000;
/// The longest row of waiting transactions that spend one another, counted generously
/// (a transaction with two waiting parents counts both rows).
pub const MAX_ROW: usize = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PoolError {
    #[error("already waiting")]
    Known,
    #[error("{0}")]
    Tx(#[from] TxError),
    #[error("the fee is under 0.001 ARCADE per 1,000 bytes")]
    LowFee,
    #[error("a waiting transaction already spends one of its coins")]
    Conflict,
    #[error("too many waiting transactions in a row")]
    Row,
    #[error("the fee doesn't cover the blobs it brings (0.01 ARCADE per 1,000 bytes of blob)")]
    LowBlobFee,
    #[error("a blob it brings isn't here: send its bytes with it")]
    BlobMissing,
    #[error("the pool is full of transactions that pay more")]
    Full,
    /// A blob upgrade it carries is refused (rules version 6; before it the pool takes none).
    #[error("blob upgrade: {0}")]
    Upgrade(crate::blob::UpgradeError),
    /// Not the pool's own answer: an arcade's, for a transaction it refused a moment ago.
    #[error("refused a moment ago; it can be offered again after the next block")]
    Refused,
}

impl PoolError {
    /// A stable code for a refusal, as a peer reports it back ("txr" in pool gossip) and tx.status shows it. Local
    /// bookkeeping only: no rule depends on it.
    pub fn code(&self) -> &'static str {
        match self {
            PoolError::Known => "known",
            PoolError::LowFee | PoolError::LowBlobFee => "fee_too_low",
            PoolError::Conflict => "conflict",
            PoolError::Row => "rules",
            PoolError::BlobMissing => "blob_missing",
            PoolError::Full => "pool_full",
            PoolError::Upgrade(crate::blob::UpgradeError::Underpaid) => "fee_too_low",
            PoolError::Upgrade(crate::blob::UpgradeError::NotYet) => "not_yet_valid",
            PoolError::Upgrade(_) => "invalid",
            PoolError::Refused => "refused_recently",
            PoolError::Tx(TxError::MissingCoin) => "missing_coin",
            PoolError::Tx(TxError::Dust) => "dust",
            PoolError::Tx(TxError::Locked | TxError::Immature) => "not_yet_valid",
            PoolError::Tx(TxError::Version | TxError::TooManyChecks | TxError::TooBig) => "rules",
            PoolError::Tx(_) => "invalid",
        }
    }
}

/// A waiting transaction.
#[derive(Debug, Clone)]
pub struct Waiting {
    pub tx: Tx,
    pub fee: u64,
    /// Signature checks its scripts took.
    pub checks: u32,
    /// Its place in the order of arrival.
    seq: u64,
    /// The waiting transactions it spends.
    parents: Vec<Hash256>,
    row: usize,
    /// The blobs its records name that the chain didn't hold when it came (id, bytes): it paid the blob fee for them.
    pub blobs: Vec<(crate::merkle::Hash, u64)>,
}

impl Waiting {
    fn bytes(&self) -> usize {
        self.tx.raw.len()
    }

    /// Its number in the order of arrival (what `Pool::after` goes by): a peer told of numbers up to this one has
    /// been told of it.
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

/// Does `a` pay more per byte than `b`? (fee, bytes)
fn pays_more(a: (u64, usize), b: (u64, usize)) -> bool {
    a.0 as u128 * b.1 as u128 > b.0 as u128 * a.1 as u128
}

#[derive(Debug)]
pub struct Pool {
    txs: HashMap<Hash256, Waiting>,
    /// Order of arrival: parents before the transactions that spend them.
    order: BTreeMap<u64, Hash256>,
    /// Every coin a waiting transaction spends, and which one.
    spent: HashMap<OutPoint, Hash256>,
    bytes: usize,
    next_seq: u64,
    max_bytes: usize,
}

impl Default for Pool {
    fn default() -> Self {
        Pool::new(MAX_POOL_BYTES)
    }
}

/// The coins as the next waiting transaction finds them: the chain's, less what the pool
/// spends, plus what it makes.
struct View<'a, C: Coins> {
    base: &'a C,
    pool: &'a Pool,
    height: u64,
}

impl<C: Coins> Coins for View<'_, C> {
    fn coin(&self, at: &OutPoint) -> Option<Coin> {
        if self.pool.spent.contains_key(at) {
            return None;
        }
        match self.pool.txs.get(&at.txid) {
            Some(w) => {
                let out = w.tx.outputs.get(at.vout as usize)?;
                (!script::unspendable(&out.script_pubkey)).then(|| Coin { value: out.value, script: out.script_pubkey.clone(), height: self.height, coinbase: false })
            }
            None => self.base.coin(at),
        }
    }
}

/// Of the blobs `tx`'s records name, those `held` says the chain hasn't got, each once (id, declared length), and what its
/// blob fee pays for ([`crate::blob::fee_at`]): each of those, and every boost.
fn named(tx: &Tx, net: &crate::params::Net, held: &dyn Fn(&crate::merkle::Hash) -> bool) -> (Vec<(crate::merkle::Hash, u64)>, Vec<(crate::blob::Class, u64)>) {
    let mut blobs: Vec<(crate::merkle::Hash, u64)> = Vec::new();
    let mut paid: Vec<(crate::blob::Class, u64)> = Vec::new();
    for r in crate::blob::records(tx) {
        let id = r.id(net);
        if !held(&id) && !blobs.iter().any(|b| b.0 == id) {
            blobs.push((id, r.len));
            paid.push((r.class, r.len));
        } else if r.class == crate::blob::Class::Boost {
            paid.push((r.class, r.len));
        }
    }
    (blobs, paid)
}

impl Pool {
    pub fn new(max_bytes: usize) -> Pool {
        Pool { txs: HashMap::new(), order: BTreeMap::new(), spent: HashMap::new(), bytes: 0, next_seq: 0, max_bytes }
    }

    pub fn len(&self) -> usize {
        self.txs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.txs.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn get(&self, id: &Hash256) -> Option<&Waiting> {
        self.txs.get(id)
    }

    pub fn contains(&self, id: &Hash256) -> bool {
        self.txs.contains_key(id)
    }

    /// The waiting transactions in order of arrival.
    pub fn iter(&self) -> impl Iterator<Item = &Waiting> {
        self.order.values().map(|id| &self.txs[id])
    }

    /// Up to `max` waiting transactions that arrived after number `seq` (all, from the
    /// first, for `None`), in order: (number, id). Numbers only ever go up.
    pub fn after(&self, seq: Option<u64>, max: usize) -> Vec<(u64, Hash256)> {
        let from = seq.map_or(0, |s| s + 1);
        self.order.range(from..).take(max).map(|(n, id)| (*n, *id)).collect()
    }

    /// Which waiting transaction spends this coin.
    pub fn spender(&self, at: &OutPoint) -> Option<Hash256> {
        self.spent.get(at).copied()
    }

    /// Take in a transaction for the block at `height` (the one after the tip), whose coins
    /// before it are `coins`. Its fee.
    pub fn add(&mut self, tx: Tx, coins: &impl Coins, height: u64) -> Result<u64, PoolError> {
        self.add_inner(tx, coins, height, &crate::params::TESTNET, &|_| true, None)
    }

    /// [`Pool::add`] for a chain with blobs (rules version 3): of the blobs `tx`'s records name, those `held` says the
    /// chain hasn't got are paid for at the blob rate on top of the transaction's own floor (every waiting transaction
    /// naming a blob new to the chain pays for it, so whichever a block puts first may bring it). Its upgrades (rules
    /// version 6) are judged against the blobs as `state` says the chain holds them, each blob upgraded by one waiting
    /// transaction at most, and paid for on top; before version 6 a transaction carrying one is refused (the chain would
    /// take it as plain data, and its sender would pay for nothing).
    pub fn add_with(&mut self, tx: Tx, coins: &impl Coins, height: u64, net: &crate::params::Net, state: &dyn Fn(&crate::merkle::Hash) -> Option<crate::blob::State>) -> Result<u64, PoolError> {
        self.add_inner(tx, coins, height, net, &|id| state(id).is_some(), Some(state))
    }

    fn add_inner(&mut self, tx: Tx, coins: &impl Coins, height: u64, net: &crate::params::Net, held: &dyn Fn(&crate::merkle::Hash) -> bool, state: Option<&dyn Fn(&crate::merkle::Hash) -> Option<crate::blob::State>>) -> Result<u64, PoolError> {
        if self.txs.contains_key(&tx.txid) {
            return Err(PoolError::Known);
        }
        // Before any script is run: what a look at the transaction and the pool shows.
        coins::check_form(&tx)?;
        if tx.inputs.iter().any(|i| self.spent.contains_key(&i.prevout)) {
            return Err(PoolError::Conflict);
        }
        let (parents, row) = self.parents(&tx);
        if row > MAX_ROW {
            return Err(PoolError::Row);
        }
        let mut checks = 0u32;
        let fee = coins::check_spend(&tx, height, &View { base: coins, pool: self, height }, &mut checks)?;
        let bytes = tx.raw.len();
        let (blobs, paid) = named(&tx, net, held);
        // More new blobs by their records' lengths than one block brings (a block's cap from rules version 6, and a block
        // template's always, Pool::pick_with): no block could take it.
        if blobs.iter().map(|b| b.1).sum::<u64>() > crate::blob::MAX_BLOB_BYTES as u64 {
            return Err(PoolError::Tx(TxError::TooBig));
        }
        // Priced as the next block (`height`) will judge it.
        let mut blob_fee = crate::blob::fee_at(net, height, &paid);
        if fee < blob_fee {
            return Err(PoolError::LowBlobFee);
        }
        // Upgrades (only where the caller knows the chain's blobs: a transaction a block left comes back without, and
        // the pool is gone through again after, Pool::drop_bad_upgrades).
        let ups = crate::blob::upgrades(&tx);
        if let (Some(state), false) = (state, ups.is_empty()) {
            let mut taken = self.upgrading();
            blob_fee = blob_fee.saturating_add(crate::blob::check_upgrades(net, height, &ups, state, &mut taken).map_err(PoolError::Upgrade)?);
            if fee < blob_fee {
                return Err(PoolError::Upgrade(crate::blob::UpgradeError::Underpaid));
            }
        }
        if ((fee - blob_fee) as u128) * 1_000 < MIN_FEE_PER_KB as u128 * bytes as u128 {
            return Err(PoolError::LowFee);
        }
        if self.bytes + bytes > self.max_bytes {
            self.make_room(bytes, fee, &parents)?;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.admit(Waiting { tx, fee, checks, seq, parents, row, blobs });
        Ok(fee)
    }

    /// The blobs waiting transactions upgrade (rules version 6).
    fn upgrading(&self) -> HashSet<crate::merkle::Hash> {
        self.txs.values().flat_map(|w| crate::blob::upgrades(&w.tx)).collect()
    }

    /// After a change of tip: waiting transactions whose upgrades the block at `height` would refuse go, with everything
    /// that spends them (a blob upgraded by the block just taken, past its time, or upgraded by a transaction that waited
    /// longer; before rules version 6, any). `state` is the chain's blobs at the tip. How many went.
    pub fn drop_bad_upgrades(&mut self, net: &crate::params::Net, height: u64, state: &dyn Fn(&crate::merkle::Hash) -> Option<crate::blob::State>) -> usize {
        let mut taken: HashSet<crate::merkle::Hash> = HashSet::new();
        let mut bad: HashSet<Hash256> = HashSet::new();
        let order: Vec<Hash256> = self.order.values().copied().collect();
        for id in order {
            let w = &self.txs[&id];
            let ups = crate::blob::upgrades(&w.tx);
            if ups.is_empty() || bad.contains(&id) {
                continue;
            }
            // As Pool::add_with judges it: the blob fee for what it brings now and its boosts, plus the upgrades.
            let (_, paid) = named(&w.tx, net, &|id| state(id).is_some());
            let blob_fee = crate::blob::fee_at(net, height, &paid);
            let ok = crate::blob::check_upgrades(net, height, &ups, state, &mut taken).is_ok_and(|f| w.fee >= blob_fee.saturating_add(f));
            if !ok {
                self.with_descendants(&id, &mut bad);
            }
        }
        for id in &bad {
            self.remove(id);
        }
        bad.len()
    }

    /// The waiting transactions `tx` spends, and how long a row it would stand at the end of.
    fn parents(&self, tx: &Tx) -> (Vec<Hash256>, usize) {
        let mut parents: Vec<Hash256> = tx.inputs.iter().map(|i| i.prevout.txid).filter(|id| self.txs.contains_key(id)).collect();
        parents.sort_unstable_by_key(|h| h.0);
        parents.dedup();
        let row = 1 + parents.iter().map(|p| self.txs[p].row).sum::<usize>();
        (parents, row)
    }

    fn admit(&mut self, w: Waiting) {
        for input in &w.tx.inputs {
            self.spent.insert(input.prevout, w.tx.txid);
        }
        self.bytes += w.bytes();
        self.order.insert(w.seq, w.tx.txid);
        self.txs.insert(w.tx.txid, w);
    }

    /// `id` and every waiting transaction that spends it, directly or not.
    fn with_descendants(&self, id: &Hash256, into: &mut HashSet<Hash256>) {
        let mut todo = vec![*id];
        while let Some(id) = todo.pop() {
            if !into.insert(id) {
                continue;
            }
            let Some(w) = self.txs.get(&id) else { continue };
            for vout in 0..w.tx.outputs.len() as u32 {
                if let Some(child) = self.spent.get(&OutPoint { txid: id, vout }) {
                    todo.push(*child);
                }
            }
        }
    }

    fn remove(&mut self, id: &Hash256) {
        let Some(w) = self.txs.remove(id) else { return };
        for input in &w.tx.inputs {
            self.spent.remove(&input.prevout);
        }
        self.bytes -= w.bytes();
        self.order.remove(&w.seq);
    }

    /// Free `bytes` for a transaction paying `fee`: the cheapest transactions per byte go,
    /// each with all that spend it, as long as each pays less than the newcomer. Nothing
    /// goes unless enough can.
    fn make_room(&mut self, bytes: usize, fee: u64, parents: &[Hash256]) -> Result<(), PoolError> {
        // What the newcomer stands on stays.
        let mut keep: HashSet<Hash256> = HashSet::new();
        let mut todo: Vec<Hash256> = parents.to_vec();
        while let Some(id) = todo.pop() {
            if keep.insert(id) {
                todo.extend(self.txs[&id].parents.iter().copied());
            }
        }
        let mut cheapest: Vec<&Waiting> = self.txs.values().filter(|w| !keep.contains(&w.tx.txid)).collect();
        cheapest.sort_unstable_by(|a, b| {
            // fee per byte, lowest first; the newest of equals first.
            (a.fee as u128 * b.bytes() as u128).cmp(&(b.fee as u128 * a.bytes() as u128)).then(b.seq.cmp(&a.seq))
        });
        let mut out: HashSet<Hash256> = HashSet::new();
        let mut freed = 0usize;
        for w in cheapest {
            if self.bytes - freed + bytes <= self.max_bytes {
                break;
            }
            if out.contains(&w.tx.txid) {
                continue;
            }
            if !pays_more((fee, bytes), (w.fee, w.bytes())) {
                return Err(PoolError::Full);
            }
            let before = out.len();
            self.with_descendants(&w.tx.txid, &mut out);
            debug_assert!(out.len() > before);
            freed = out.iter().map(|id| self.txs[id].bytes()).sum();
        }
        if self.bytes - freed + bytes > self.max_bytes {
            return Err(PoolError::Full);
        }
        for id in out {
            self.remove(&id);
        }
        Ok(())
    }

    /// The tip changed: the next block is at `height` and finds `coins`. `back` are the
    /// transactions of blocks the chain left, oldest first; they wait again where they
    /// still can. Waiting transactions whose coins are gone (spent by a block, or made by
    /// a transaction that left) are dropped.
    ///
    /// The ones that stay keep their numbers ([`Pool::after`]), unless transactions came
    /// back: those go in front of what may spend them, and everything is numbered anew.
    pub fn retip(&mut self, coins: &impl Coins, height: u64, back: Vec<Tx>) {
        self.retip_inner(coins, height, back, None);
    }

    /// [`Pool::retip`] for a chain with blobs: the transactions that come back are taken as [`Pool::add_with`] takes them,
    /// against the blobs as `state` says the chain holds them now, and every waiting transaction's new blobs (what a
    /// block's blob budget counts, [`Pool::pick_with`]) are counted again: after a reorg a blob the chain held may be new
    /// once more.
    pub fn retip_with(&mut self, coins: &impl Coins, height: u64, back: Vec<Tx>, net: &crate::params::Net, state: &dyn Fn(&crate::merkle::Hash) -> Option<crate::blob::State>) {
        self.retip_inner(coins, height, back, Some((net, state)));
    }

    fn retip_inner(&mut self, coins: &impl Coins, height: u64, back: Vec<Tx>, chain: Option<(&crate::params::Net, &dyn Fn(&crate::merkle::Hash) -> Option<crate::blob::State>)>) {
        let renumber = back.iter().any(|tx| !tx.is_coinbase());
        let old: Vec<Waiting> = {
            let mut txs = std::mem::take(&mut self.txs);
            std::mem::take(&mut self.order).into_values().filter_map(|id| txs.remove(&id)).collect()
        };
        self.spent.clear();
        self.bytes = 0;
        for tx in back {
            if !tx.is_coinbase() {
                let _ = match chain {
                    Some((net, state)) => self.add_inner(tx, coins, height, net, &|id| state(id).is_some(), Some(state)),
                    None => self.add(tx, coins, height),
                };
            }
        }
        for mut w in old {
            if self.txs.contains_key(&w.tx.txid) || !self.still_good(&w.tx, coins, height) {
                continue;
            }
            let (parents, row) = self.parents(&w.tx);
            if row > MAX_ROW || self.bytes + w.bytes() > self.max_bytes {
                continue;
            }
            if let Some((net, state)) = chain {
                w.blobs = named(&w.tx, net, &|id| state(id).is_some()).0;
            }
            let seq = if renumber { self.next_seq } else { w.seq };
            self.next_seq = self.next_seq.max(seq + 1);
            self.admit(Waiting { parents, row, seq, ..w });
        }
    }

    /// A transaction whose scripts were run before: are its coins there for the block at `height`?
    fn still_good(&self, tx: &Tx, coins: &impl Coins, height: u64) -> bool {
        let view = View { base: coins, pool: self, height };
        coins::unlocked(tx, height)
            && tx.inputs.iter().all(|i| view.coin(&i.prevout).is_some_and(|c| !c.coinbase || height >= c.height + COINBASE_MATURITY))
    }

    /// Transactions for a block: the ones that pay most per byte first, each after the
    /// waiting transactions it spends, within `max_bytes` and the block's signature checks.
    /// With them, their fees.
    pub fn pick(&self, max_bytes: usize) -> (Vec<Tx>, u64) {
        self.pick_with(max_bytes, usize::MAX, &|_| false)
    }

    /// [`Pool::pick`] within a blob budget too: `new` says which of a transaction's blobs the chain hasn't got yet
    /// (each counted once, however many transactions name it).
    pub fn pick_with(&self, max_bytes: usize, max_blob_bytes: usize, new: &dyn Fn(&crate::merkle::Hash) -> bool) -> (Vec<Tx>, u64) {
        let mut blob_ids: HashSet<crate::merkle::Hash> = HashSet::new();
        let mut upgraded: HashSet<crate::merkle::Hash> = HashSet::new();
        let mut blob_bytes = 0usize;
        let mut best: Vec<&Waiting> = self.txs.values().collect();
        best.sort_unstable_by(|a, b| (b.fee as u128 * a.bytes() as u128).cmp(&(a.fee as u128 * b.bytes() as u128)).then(a.seq.cmp(&b.seq)));
        let mut picked: HashSet<Hash256> = HashSet::new();
        let (mut txs, mut fees, mut bytes, mut checks) = (Vec::new(), 0u64, 0usize, 0u32);
        // A transaction passed over because a parent wasn't in yet gets another turn; a row
        // is at most MAX_ROW long, so that many turns reach everyone.
        for _ in 0..MAX_ROW {
            let mut added = false;
            for w in &best {
                if picked.contains(&w.tx.txid) || bytes + w.bytes() > max_bytes || checks + w.checks > MAX_SIG_CHECKS {
                    continue;
                }
                if !w.parents.iter().all(|p| picked.contains(p)) {
                    continue;
                }
                let more: usize = w.blobs.iter().filter(|b| new(&b.0) && !blob_ids.contains(&b.0)).map(|b| b.1 as usize).sum();
                if blob_bytes + more > max_blob_bytes {
                    continue;
                }
                // A blob is upgraded once in a block (rules version 6; the pool takes one upgrade of each anyway).
                let ups = crate::blob::upgrades(&w.tx);
                if ups.iter().any(|u| upgraded.contains(u)) {
                    continue;
                }
                upgraded.extend(ups);
                blob_bytes += more;
                blob_ids.extend(w.blobs.iter().filter(|b| new(&b.0)).map(|b| b.0));
                picked.insert(w.tx.txid);
                bytes += w.bytes();
                checks += w.checks;
                fees += w.fee;
                txs.push(w.tx.clone());
                added = true;
            }
            if !added {
                break;
            }
        }
        (txs, fees)
    }

    /// Drop everything. The numbers go on from where they were.
    pub fn clear(&mut self) {
        let next_seq = self.next_seq;
        *self = Pool::new(self.max_bytes);
        self.next_seq = next_seq;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coins::check_block;
    use crate::params::COIN;
    use crate::script::tests::{key, p2pkh, sign, tx, with_sig};
    use crate::script::{push, SIGHASH_ALL};
    use secp256k1::SecretKey;

    const PREV: Hash256 = Hash256([0xab; 32]);
    const TESTNET_V6: crate::params::Net = crate::params::Net { rules: &[(3, 0), (5, 1), (6, HEIGHT)], ..crate::params::TESTNET };
    const HEIGHT: u64 = 500;

    fn point(n: u32) -> OutPoint {
        OutPoint { txid: Hash256([0x11; 32]), vout: n }
    }

    /// `n` coins of 10 ARCADE each, locked to key 1.
    fn held(n: u32) -> HashMap<OutPoint, Coin> {
        let script = p2pkh(&key(1).1);
        (0..n).map(|i| (point(i), Coin { value: 10 * COIN, script: script.clone(), height: 1, coinbase: false })).collect()
    }

    /// Spend `from` (locked to `sk`'s key) into `outputs`, signed ALL.
    fn pay(from: &[OutPoint], outputs: Vec<(u64, Vec<u8>)>, sk: &SecretKey, pk: &[u8]) -> Tx {
        let mut t = tx(from.iter().map(|p| (*p, vec![], u32::MAX)).collect(), outputs, 0);
        let code = p2pkh(pk);
        for index in 0..from.len() {
            let mut unlock = Vec::new();
            push(&mut unlock, &sign(&t, index, &code, sk, SIGHASH_ALL));
            push(&mut unlock, pk);
            t = with_sig(&t, index, unlock);
        }
        t
    }

    /// One coin of key 1 to key 1 again, leaving `fee`.
    fn send(from: OutPoint, value: u64, fee: u64) -> Tx {
        let (sk, pk) = key(1);
        pay(&[from], vec![(value - fee, p2pkh(&pk))], &sk, &pk)
    }

    fn first_out(t: &Tx) -> OutPoint {
        OutPoint { txid: t.txid, vout: 0 }
    }

    /// The picked transactions make a block the coin rules take.
    fn block_holds(pool: &Pool, coins: &HashMap<OutPoint, Coin>, max_bytes: usize) -> (Vec<Tx>, u64) {
        let (txs, fees) = pool.pick(max_bytes);
        let mut all = vec![coins::coinbase(HEIGHT, &PREV, b"", &[])];
        all.extend(txs.iter().cloned());
        let changes = check_block(&all, HEIGHT, &PREV, coins, &[]).expect("a block of picked transactions");
        assert_eq!(changes.fees, fees);
        (txs, fees)
    }

    #[test]
    fn a_transaction_waits_once_and_pays_the_floor() {
        let coins = held(3);
        let mut pool = Pool::default();
        let t = send(point(0), 10 * COIN, COIN / 100);
        assert_eq!(pool.add(t.clone(), &coins, HEIGHT), Ok(COIN / 100));
        assert_eq!(pool.add(t.clone(), &coins, HEIGHT), Err(PoolError::Known));
        assert_eq!((pool.len(), pool.bytes()), (1, t.raw.len()));

        // Under the floor: about 190 bytes need 0.00019.
        let cheap = send(point(1), 10 * COIN, COIN / 100_000);
        assert_eq!(pool.add(cheap, &coins, HEIGHT), Err(PoolError::LowFee));
        // A coin nobody has, and a signature that doesn't hold.
        assert_eq!(pool.add(send(point(9), 10 * COIN, COIN / 100), &coins, HEIGHT), Err(PoolError::Tx(TxError::MissingCoin)));
        let (sk2, _) = key(2);
        let forged = pay(&[point(2)], vec![(9 * COIN, p2pkh(&key(1).1))], &sk2, &key(1).1);
        assert!(matches!(pool.add(forged, &coins, HEIGHT), Err(PoolError::Tx(TxError::Script(0, _)))));
        assert_eq!(pool.len(), 1);
        block_holds(&pool, &coins, 1_000_000);
    }

    #[test]
    fn the_first_spend_of_a_coin_stays() {
        let coins = held(1);
        let mut pool = Pool::default();
        let first = send(point(0), 10 * COIN, COIN / 100);
        pool.add(first.clone(), &coins, HEIGHT).unwrap();
        // The same coin again, paying ten times the fee.
        assert_eq!(pool.add(send(point(0), 10 * COIN, COIN / 10), &coins, HEIGHT), Err(PoolError::Conflict));
        assert_eq!(pool.spender(&point(0)), Some(first.txid));
    }

    #[test]
    fn waiting_transactions_spend_one_another_up_to_a_row() {
        let coins = held(1);
        let mut pool = Pool::default();
        let mut last = send(point(0), 10 * COIN, COIN / 100);
        pool.add(last.clone(), &coins, HEIGHT).unwrap();
        let mut value = 10 * COIN - COIN / 100;
        for _ in 1..MAX_ROW {
            last = send(first_out(&last), value, COIN / 100);
            value -= COIN / 100;
            pool.add(last.clone(), &coins, HEIGHT).unwrap();
        }
        assert_eq!(pool.len(), MAX_ROW);
        assert_eq!(pool.add(send(first_out(&last), value, COIN / 100), &coins, HEIGHT), Err(PoolError::Row));
        // All of the row fits a block, parents first.
        let (txs, fees) = block_holds(&pool, &coins, 1_000_000);
        assert_eq!((txs.len(), fees), (MAX_ROW, MAX_ROW as u64 * COIN / 100));
    }

    #[test]
    fn a_block_gets_the_best_paying_that_fit() {
        let coins = held(4);
        let mut pool = Pool::default();
        let low = send(point(0), 10 * COIN, COIN / 1_000);
        let high = send(point(1), 10 * COIN, COIN);
        let mid = send(point(2), 10 * COIN, COIN / 10);
        // A child that pays well on a parent that pays little: it can't go in before it.
        let child = send(first_out(&low), 10 * COIN - COIN / 1_000, 5 * COIN);
        for t in [&low, &high, &mid, &child] {
            pool.add(t.clone(), &coins, HEIGHT).unwrap();
        }
        let (all, _) = block_holds(&pool, &coins, 1_000_000);
        assert_eq!(all.iter().map(|t| t.txid).collect::<Vec<_>>(), vec![high.txid, mid.txid, low.txid, child.txid]);

        // Room for two: the two that pay most and can stand alone.
        let (two, fees) = block_holds(&pool, &coins, high.raw.len() + mid.raw.len() + 10);
        assert_eq!(two.iter().map(|t| t.txid).collect::<Vec<_>>(), vec![high.txid, mid.txid]);
        assert_eq!(fees, COIN + COIN / 10);
        assert!(pool.pick(10).0.is_empty());
    }

    #[test]
    fn a_full_pool_lets_in_only_what_pays_more() {
        let coins = held(6);
        let a = send(point(0), 10 * COIN, COIN / 100);
        let size = a.raw.len();
        // Room for three transactions of this size (signatures differ by a byte or two).
        let mut pool = Pool::new(3 * size + 6);
        let b = send(point(1), 10 * COIN, COIN / 50);
        let b_child = send(first_out(&b), 10 * COIN - COIN / 50, COIN);
        pool.add(a.clone(), &coins, HEIGHT).unwrap();
        pool.add(b.clone(), &coins, HEIGHT).unwrap();
        pool.add(b_child.clone(), &coins, HEIGHT).unwrap();

        // Pays no more than the cheapest: refused, and nothing left.
        assert_eq!(pool.add(send(point(2), 10 * COIN, COIN / 100), &coins, HEIGHT), Err(PoolError::Full));
        assert_eq!(pool.len(), 3);
        // Pays more than the cheapest: that one leaves.
        let c = send(point(3), 10 * COIN, COIN / 40);
        pool.add(c.clone(), &coins, HEIGHT).unwrap();
        assert!(!pool.contains(&a.txid) && pool.contains(&c.txid) && pool.len() == 3);
        // Pays more than `b`: `b` leaves with the transaction that spends it, however much that pays.
        let d = send(point(4), 10 * COIN, COIN / 30);
        pool.add(d.clone(), &coins, HEIGHT).unwrap();
        assert!(!pool.contains(&b.txid) && !pool.contains(&b_child.txid));
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.spender(&point(1)), None);
        // What a newcomer stands on is never pushed out for it.
        let c_child = send(first_out(&c), 10 * COIN - COIN / 40, 2 * COIN);
        pool.add(c_child.clone(), &coins, HEIGHT).unwrap();
        let c_grandchild = send(first_out(&c_child), 10 * COIN - COIN / 40 - 2 * COIN, 3 * COIN);
        pool.add(c_grandchild.clone(), &coins, HEIGHT).unwrap();
        assert!(pool.contains(&c.txid) && pool.contains(&c_child.txid) && pool.contains(&c_grandchild.txid) && !pool.contains(&d.txid));
        assert_eq!(pool.bytes(), pool.iter().map(|w| w.tx.raw.len()).sum::<usize>());
        block_holds(&pool, &coins, 1_000_000);
    }

    #[test]
    fn a_new_tip_drops_what_it_spent_and_keeps_the_rest() {
        let mut coins = held(3);
        let mut pool = Pool::default();
        let a = send(point(0), 10 * COIN, COIN / 100);
        let a_child = send(first_out(&a), 10 * COIN - COIN / 100, COIN / 100);
        let b = send(point(1), 10 * COIN, COIN / 100);
        let c = send(point(2), 10 * COIN, COIN / 100);
        for t in [&a, &a_child, &b, &c] {
            pool.add(t.clone(), &coins, HEIGHT).unwrap();
        }

        // A block with `a`, and with another spend of `b`'s coin.
        let other = send(point(1), 10 * COIN, COIN / 5);
        let block = vec![coins::coinbase(HEIGHT, &PREV, b"", &[]), a.clone(), other.clone()];
        let changes = check_block(&block, HEIGHT, &PREV, &coins, &[]).unwrap();
        for (at, _) in &changes.spent {
            coins.remove(at);
        }
        coins.extend(changes.made.iter().cloned());
        pool.retip(&coins, HEIGHT + 1, Vec::new());
        // `a` is in the chain, `b` can never be; `a`'s child now spends a coin of the chain.
        assert_eq!(pool.iter().map(|w| w.tx.txid).collect::<Vec<_>>(), vec![a_child.txid, c.txid]);
        assert_eq!(pool.spender(&point(1)), None);
        // They keep their numbers: someone who heard of everything up to 3 hears nothing twice.
        assert_eq!(pool.after(None, 10), vec![(1, a_child.txid), (3, c.txid)]);
        assert_eq!(pool.after(Some(1), 10), vec![(3, c.txid)]);
        assert!(pool.after(Some(3), 10).is_empty());
        block_holds_at(&pool, &coins, HEIGHT + 1);

        // The chain leaves that block again: its transactions wait again, `a` before its child.
        for (at, _) in &changes.made {
            coins.remove(at);
        }
        coins.extend(changes.spent.iter().cloned());
        pool.retip(&coins, HEIGHT, block);
        let ids: Vec<Hash256> = pool.iter().map(|w| w.tx.txid).collect();
        assert_eq!(ids, vec![a.txid, other.txid, a_child.txid, c.txid]);
        // All numbered anew, above every number given before.
        assert_eq!(pool.after(Some(3), 10).iter().map(|(_, id)| *id).collect::<Vec<_>>(), ids);
        assert_eq!(pool.after(Some(3), 2).len(), 2);
        // `b` lost its coin to `other` for good.
        assert_eq!(pool.add(b, &coins, HEIGHT), Err(PoolError::Conflict));
        block_holds(&pool, &coins, 1_000_000);
    }

    fn block_holds_at(pool: &Pool, coins: &HashMap<OutPoint, Coin>, height: u64) {
        let mut all = vec![coins::coinbase(height, &PREV, b"", &[])];
        all.extend(pool.pick(1_000_000).0);
        check_block(&all, height, &PREV, coins, &[]).expect("a block of picked transactions");
    }

    #[test]
    fn a_reward_and_a_lock_wait_for_their_height() {
        let (sk, pk) = key(1);
        let mut coins = held(1);
        let reward = OutPoint { txid: Hash256([0x22; 32]), vout: 0 };
        coins.insert(reward, Coin { value: 50 * COIN, script: p2pkh(&pk), height: HEIGHT - 50, coinbase: true });
        let mut pool = Pool::default();
        assert_eq!(pool.add(send(reward, 50 * COIN, COIN / 100), &coins, HEIGHT), Err(PoolError::Tx(TxError::Immature)));
        assert_eq!(pool.add(send(reward, 50 * COIN, COIN / 100), &coins, HEIGHT + 50), Ok(COIN / 100));
        // The chain goes back below the reward's 100 blocks: it leaves the pool.
        pool.retip(&coins, HEIGHT + 49, Vec::new());
        assert!(pool.is_empty());

        // Locked until after block HEIGHT + 5.
        let mut locked = tx(vec![(point(0), vec![], 0)], vec![(9 * COIN, p2pkh(&pk))], HEIGHT as u32 + 5);
        let mut unlock = Vec::new();
        push(&mut unlock, &sign(&locked, 0, &p2pkh(&pk), &sk, SIGHASH_ALL));
        push(&mut unlock, &pk);
        locked = with_sig(&locked, 0, unlock);
        assert_eq!(pool.add(locked.clone(), &coins, HEIGHT + 5), Err(PoolError::Tx(TxError::Locked)));
        assert_eq!(pool.add(locked, &coins, HEIGHT + 6), Ok(COIN));
        pool.retip(&coins, HEIGHT + 5, Vec::new());
        assert!(pool.is_empty());
    }

    #[test]
    fn refusals_have_stable_codes() {
        assert_eq!(PoolError::LowFee.code(), "fee_too_low");
        assert_eq!(PoolError::LowBlobFee.code(), "fee_too_low");
        assert_eq!(PoolError::Conflict.code(), "conflict");
        assert_eq!(PoolError::Tx(TxError::MissingCoin).code(), "missing_coin");
        assert_eq!(PoolError::Tx(TxError::Overspend).code(), "invalid");
        assert_eq!(PoolError::Tx(TxError::Immature).code(), "not_yet_valid");
        assert_eq!(PoolError::Known.code(), "known");
    }

    #[test]
    fn rules_v6_the_pool_takes_one_paid_upgrade_of_a_temporary_blob() {
        use crate::blob::{self, Class, State, UpgradeError};
        use crate::params::{Net, TESTNET};
        const V6: Net = Net { rules: &[(3, 0), (5, 1), (6, HEIGHT)], ..TESTNET };
        let coins = held(6);
        let (sk, pk) = key(1);
        let t: crate::merkle::Hash = [1; 32];
        let state = |id: &crate::merkle::Hash| (*id == t).then(|| State::brought(&V6, Class::Temporary, 1_000, 10));
        let diff = blob::upgrade_fee_at(&V6, HEIGHT, Class::Temporary, 1_000);
        // Paying `fee` in all: the difference plus the floor, or less.
        let up = |n: u32, ids: &[crate::merkle::Hash], fee: u64| pay(&[point(n)], vec![(0, blob::upgrade_script(ids)), (10 * COIN - fee, p2pkh(&pk))], &sk, &pk);
        let full = diff + COIN / 100;
        let mut pool = Pool::new(MAX_POOL_BYTES);
        // Before version 6 the pool takes none.
        assert_eq!(pool.add_with(up(0, &[t], full), &coins, HEIGHT - 1, &V6, &state), Err(PoolError::Upgrade(UpgradeError::NotYet)));
        // Short of the difference, of an unknown blob: refused.
        assert_eq!(pool.add_with(up(0, &[t], diff - 1), &coins, HEIGHT, &V6, &state), Err(PoolError::Upgrade(UpgradeError::Underpaid)));
        assert_eq!(pool.add_with(up(0, &[[2; 32]], full), &coins, HEIGHT, &V6, &state), Err(PoolError::Upgrade(UpgradeError::Unknown)));
        // Paid: in; a second upgrade of the same blob waits behind none.
        assert_eq!(pool.add_with(up(0, &[t], full), &coins, HEIGHT, &V6, &state), Ok(full));
        assert_eq!(pool.add_with(up(1, &[t], full), &coins, HEIGHT, &V6, &state), Err(PoolError::Upgrade(UpgradeError::Twice)));
        assert_eq!(PoolError::Upgrade(UpgradeError::Underpaid).code(), "fee_too_low");
        // A block upgraded it: the waiting one goes at the next change of tip, with what spends it.
        let first = pool.iter().next().unwrap().tx.clone();
        let child = pay(&[OutPoint { txid: first.txid, vout: 1 }], vec![(10 * COIN - 2 * COIN / 100 - diff, p2pkh(&pk))], &sk, &pk);
        pool.add_with(child, &coins, HEIGHT, &V6, &state).unwrap();
        assert_eq!(pool.len(), 2);
        assert_eq!(pool.drop_bad_upgrades(&V6, HEIGHT, &state), 0);
        let upgraded = |id: &crate::merkle::Hash| state(id).map(|mut s| {
            s.upgraded = Some(blob::Upgraded { height: HEIGHT, txid: Hash256([3; 32]), paid: diff, by: Vec::new() });
            s
        });
        assert_eq!(pool.drop_bad_upgrades(&V6, HEIGHT + 1, &upgraded), 2);
        assert!(pool.is_empty());
        // Two waiting upgrades of one blob (a block's transactions back after a reorg): the first stays, and a block
        // takes one only.
        pool.add(up(2, &[t], full), &coins, HEIGHT).unwrap();
        pool.add(up(3, &[t], full), &coins, HEIGHT).unwrap();
        assert_eq!(pool.pick(1_000_000).0.len(), 1);
        assert_eq!(pool.drop_bad_upgrades(&V6, HEIGHT, &state), 1);
        assert_eq!(pool.len(), 1);
    }

    #[test]
    fn rules_v6_a_block_picks_at_most_4_mb_of_blobs_by_their_records() {
        use crate::blob::{self, Class, Encoding, Record, MAX_BLOB_BYTES};
        let coins = held(6);
        let (sk, pk) = key(1);
        let max = MAX_BLOB_BYTES as u64;
        // Records only (their bytes needn't be here for the pool's own counting), new to the chain.
        let rec = |seed: u8, len: u64| Record { root: [seed; 32], len, class: Class::Temporary, encoding: Encoding::Plain, kind: "x".into() };
        let with = |n: u32, recs: &[Record]| {
            let fee = blob::fee_at(&TESTNET_V6, HEIGHT, &recs.iter().map(|r| (r.class, r.len)).collect::<Vec<_>>()) + COIN / 100;
            let mut outs: Vec<(u64, Vec<u8>)> = recs.iter().map(|r| (0, r.script())).collect();
            outs.push((10 * COIN - fee, p2pkh(&pk)));
            pay(&[point(n)], outs, &sk, &pk)
        };
        let none = |_: &crate::merkle::Hash| None;
        let mut pool = Pool::new(MAX_POOL_BYTES);
        // One transaction declaring more than a block brings: refused.
        assert_eq!(pool.add_with(with(0, &[rec(1, max), rec(2, 1)]), &coins, HEIGHT, &TESTNET_V6, &none), Err(PoolError::Tx(TxError::TooBig)));
        // Two of 3 MB each: both wait, a block takes one (the declared lengths count, not bytes held).
        pool.add_with(with(1, &[rec(3, 3_000_000)]), &coins, HEIGHT, &TESTNET_V6, &none).unwrap();
        pool.add_with(with(2, &[rec(4, 3_000_000)]), &coins, HEIGHT, &TESTNET_V6, &none).unwrap();
        pool.add_with(with(3, &[rec(5, max - 3_000_000)]), &coins, HEIGHT, &TESTNET_V6, &none).unwrap();
        let new = |_: &crate::merkle::Hash| true;
        let (txs, _) = pool.pick_with(1_000_000, MAX_BLOB_BYTES, &new);
        let declared: u64 = txs.iter().flat_map(blob::records).map(|r| r.len).sum();
        assert_eq!(txs.len(), 2, "one 3 MB and the 1 MB one: at the cap exactly");
        assert_eq!(declared, max);
    }

    #[test]
    fn rules_v6_after_a_change_of_tip_expired_upgrades_go_and_new_blobs_are_counted_again() {
        use crate::blob::{self, Class, Encoding, Record, State, MAX_BLOB_BYTES};
        let coins = held(6);
        let (sk, pk) = key(1);
        let t: crate::merkle::Hash = [1; 32];
        // t brought at 10, kept until HEIGHT + 1 (its first height past its time).
        let temporary = |id: &crate::merkle::Hash| (*id == t).then(|| State { until: HEIGHT + 1, ..State::brought(&TESTNET_V6, Class::Temporary, 1_000, 10) });
        let diff = blob::upgrade_fee_at(&TESTNET_V6, HEIGHT, Class::Temporary, 1_000);
        let up = pay(&[point(0)], vec![(0, blob::upgrade_script(&[t])), (10 * COIN - diff - COIN / 100, p2pkh(&pk))], &sk, &pk);
        let mut pool = Pool::new(MAX_POOL_BYTES);
        pool.add_with(up, &coins, HEIGHT, &TESTNET_V6, &temporary).unwrap();
        assert_eq!(pool.drop_bad_upgrades(&TESTNET_V6, HEIGHT, &temporary), 0);
        // Past its time at the next block: the upgrade goes.
        assert_eq!(pool.drop_bad_upgrades(&TESTNET_V6, HEIGHT + 1, &temporary), 1);
        assert!(pool.is_empty());

        // A transaction bringing a 3 MB blob and upgrading t, paying for both. Once the chain holds that blob it pays only
        // the upgrade; if a reorg takes the blob back off the chain, it has to pay the blob again: still paid, it stays,
        // and its 3 MB count in the next block's blob budget again.
        let big = Record { root: [7; 32], len: 3_000_000, class: Class::Temporary, encoding: Encoding::Plain, kind: "x".into() };
        let big_id = big.id(&TESTNET_V6);
        let blob_fee = blob::fee_at(&TESTNET_V6, HEIGHT, &[(Class::Temporary, 3_000_000)]);
        let both = pay(&[point(1)], vec![(0, big.script()), (0, blob::upgrade_script(&[t])), (10 * COIN - blob_fee - diff - COIN / 100, p2pkh(&pk))], &sk, &pk);
        let holds_big = |id: &crate::merkle::Hash| temporary(id).or_else(|| (*id == big_id).then(|| State::brought(&TESTNET_V6, Class::Temporary, 3_000_000, 20)));
        pool.add_with(both.clone(), &coins, HEIGHT, &TESTNET_V6, &holds_big).unwrap();
        assert!(pool.get(&both.txid).unwrap().blobs.is_empty(), "held: nothing new");
        pool.retip_with(&coins, HEIGHT, Vec::new(), &TESTNET_V6, &temporary);
        assert_eq!(pool.get(&both.txid).unwrap().blobs, vec![(big_id, 3_000_000)], "new again after the reorg");
        assert_eq!(pool.drop_bad_upgrades(&TESTNET_V6, HEIGHT, &temporary), 0, "it pays the blob and the upgrade");
        let (txs, _) = pool.pick_with(1_000_000, MAX_BLOB_BYTES - 1, &|id| *id == big_id);
        assert_eq!(txs.len(), 1);
        let (txs, _) = pool.pick_with(1_000_000, 2_999_999, &|id| *id == big_id);
        assert!(txs.is_empty(), "its 3 MB count against the block's blob budget");

        // One that paid only the upgrade (the blob was held when it came) is short once the blob is new again: it goes.
        let mut pool = Pool::new(MAX_POOL_BYTES);
        let only_up = pay(&[point(2)], vec![(0, big.script()), (0, blob::upgrade_script(&[t])), (10 * COIN - diff - COIN / 100, p2pkh(&pk))], &sk, &pk);
        pool.add_with(only_up, &coins, HEIGHT, &TESTNET_V6, &holds_big).unwrap();
        assert_eq!(pool.drop_bad_upgrades(&TESTNET_V6, HEIGHT, &holds_big), 0);
        assert_eq!(pool.drop_bad_upgrades(&TESTNET_V6, HEIGHT, &temporary), 1);
        // And a transaction a block left comes back counted against the chain as it is now.
        let mut pool = Pool::new(MAX_POOL_BYTES);
        let back = pay(&[point(3)], vec![(0, big.script()), (10 * COIN - blob_fee - COIN / 100, p2pkh(&pk))], &sk, &pk);
        pool.retip_with(&coins, HEIGHT, vec![back.clone()], &TESTNET_V6, &temporary);
        assert_eq!(pool.get(&back.txid).unwrap().blobs, vec![(big_id, 3_000_000)]);
    }
}
