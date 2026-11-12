//! Registered replicas (docs/ARCADECHAIN.md §4, build step 4): the storage records of a chain
//! that anyone can join by sealing a listed segment and registering it.
//!
//! - **Genesis holders** (the genesis list, §10): weights fixed at block 0, so a new chain has
//!   writers and can confirm its first segments.
//! - **A registration** takes two steps. A commit `{segment, owner, payout, replica root,
//!   anchor, nonce}`, signed by the owner, names a listed segment, an anchor block at most
//!   [`ANCHOR_AGE`] blocks back on this branch, and carries a work stamp of at least
//!   [`STAMP_BITS`] zero bits. The seed of the block [`params::CHALLENGE_DELAY`] after the
//!   commit's block picks [`params::REGISTRATION_LEAVES`] leaves; a proof opening each of them
//!   (sealed leaf to the replica root, data to the catalog root, and the sealed leaf unsealing
//!   with the owner's key to the data) is due in the [`params::ANSWER_WINDOW`] blocks after.
//! - **Weight** (Martin 2026-11-11, until audits are built: a replica counts from its proof):
//!   a key weighs its genesis weight plus, for each of its replicas, the segment's bytes split
//!   among the segment's registered replicas (§7: every copy earns).
//! - **Pay**: the pot is shared at each epoch's end by weight, to the genesis holders'
//!   addresses and the replicas' payout addresses.
//!
//! Everything here is rebuilt from the blocks (the block log is replayed at start).

use crate::block::{self, Block, BlockError, Item, Key, Payout, RegCommit, RegProof};
use crate::catalog::{self, Catalog};
use crate::chain::{Applied, Storage};
use crate::merkle::{self, Hash};
use crate::params::{self, Net};
use crate::script;
use crate::sloth;
use crate::tagged;
use arcade_chain::Hash256;
use std::collections::{BTreeMap, HashMap};

/// Zero bits a registration commit's work stamp needs (about a second of one core).
pub const STAMP_BITS: u32 = 20;
/// How far back a commit's anchor block may be.
pub const ANCHOR_AGE: u64 = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Commit {
    segment: Hash,
    owner: Key,
    payout: Payout,
    replica_root: Hash,
    height: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Replica {
    segment: Hash,
    owner: Key,
    payout: Payout,
    bytes: u64,
}

#[derive(Clone, Debug)]
enum Op {
    Committed(Hash, Option<Commit>),
    Proved(Hash, Commit),
}

#[derive(Clone, Debug)]
pub struct Registry {
    net: Net,
    genesis: HashMap<Key, (u64, Payout)>,
    catalog: Catalog,
    pot: u64,
    /// (block id, seed) of blocks 1, 2, ... on this branch.
    blocks: Vec<(Hash256, Hash)>,
    commits: BTreeMap<Hash, Commit>,
    replicas: BTreeMap<Hash, Replica>,
    /// Shares too small to pay, owed to their address and paid with its next payout.
    credit: BTreeMap<Payout, u64>,
}

pub struct Undo {
    pot: u64,
    credit: Option<BTreeMap<Payout, u64>>,
    catalog: catalog::Undo,
    ops: Vec<Op>,
}

/// The weights as the catalog counts with them (taken before the block's own items).
struct Weights<'a>(&'a Registry, u128);

impl catalog::Weights for Weights<'_> {
    fn weight(&self, key: &Key) -> u64 {
        self.0.weight(key)
    }
    fn total(&self) -> u128 {
        self.1
    }
}

/// The leaves the seed of the block `CHALLENGE_DELAY` after a commit picks for its proof.
pub fn picks(net: &Net, seed: &Hash, replica: &Hash, leaves: u64) -> Vec<u64> {
    (0..params::REGISTRATION_LEAVES as u64)
        .map(|k| {
            let h = tagged::hash(net, "pick", &[seed, replica, &k.to_le_bytes()]);
            u64::from_le_bytes(h[..8].try_into().unwrap()) % leaves
        })
        .collect()
}

fn no(what: &'static str) -> BlockError {
    BlockError::Invalid(what)
}

impl Registry {
    /// `holders`: the genesis list (owner key, bytes, payout address). `None` for a key listed
    /// twice, a holder of nothing or an address that isn't one of `net`'s.
    pub fn new(net: &Net, holders: &[(Key, u64, Payout)], segment_bytes: u64) -> Option<Self> {
        let mut genesis = HashMap::new();
        for (key, bytes, pay) in holders {
            script::pay_to(net, pay)?;
            if *bytes == 0 || genesis.insert(*key, (*bytes, *pay)).is_some() {
                return None;
            }
        }
        Some(Registry { net: *net, genesis, catalog: Catalog::new(*net, segment_bytes), pot: 0, blocks: Vec::new(), commits: BTreeMap::new(), replicas: BTreeMap::new(), credit: BTreeMap::new() })
    }

    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    pub fn pot(&self) -> u64 {
        self.pot
    }

    /// Registered replicas of a segment.
    fn holders_of(&self, segment: &Hash) -> u64 {
        self.replicas.values().filter(|r| r.segment == *segment).count() as u64
    }

    /// Every (payout address, weight), the genesis holders' and the replicas'.
    fn shares(&self) -> BTreeMap<Payout, u128> {
        let mut out: BTreeMap<Payout, u128> = BTreeMap::new();
        for (bytes, pay) in self.genesis.values() {
            *out.entry(*pay).or_default() += *bytes as u128;
        }
        for r in self.replicas.values() {
            *out.entry(r.payout).or_default() += (r.bytes / self.holders_of(&r.segment).max(1)) as u128;
        }
        out
    }

    fn total(&self) -> u128 {
        self.shares().values().sum()
    }

    /// An epoch's settlement at `height` (docs/ARCADECHAIN.md §14.18): the pot is shared by
    /// weight in whole units (floors), the units left over go one each to the largest
    /// remainders (ties: the lower address); each address's credit is added; what comes to
    /// less than [`params::MIN_PAYOUT`] is carried as its credit instead of paid. Returns the
    /// payouts in address order, the pot spent, and the credits after.
    fn settle(&self, height: u64) -> (Vec<(Payout, u64)>, u64, BTreeMap<Payout, u64>) {
        if height % params::EPOCH_BLOCKS != 0 {
            return (Vec::new(), 0, self.credit.clone());
        }
        self.settle_on(height, self.pot)
    }

    fn settle_on(&self, _height: u64, pot: u64) -> (Vec<(Payout, u64)>, u64, BTreeMap<Payout, u64>) {
        let shares = self.shares();
        let total: u128 = shares.values().sum();
        let mut credit = self.credit.clone();
        if total == 0 || pot == 0 {
            return (Vec::new(), 0, credit);
        }
        let mut amounts: Vec<(Payout, u64, u128)> = shares
            .iter()
            .map(|(pay, w)| {
                let x = pot as u128 * *w;
                (*pay, (x / total) as u64, x % total)
            })
            .collect();
        let floors: u64 = amounts.iter().map(|a| a.1).sum();
        let mut left = pot - floors;
        let mut order: Vec<usize> = (0..amounts.len()).collect();
        order.sort_by(|&i, &j| amounts[j].2.cmp(&amounts[i].2).then(amounts[i].0.cmp(&amounts[j].0)));
        for i in order {
            if left == 0 {
                break;
            }
            amounts[i].1 += 1;
            left -= 1;
        }
        let mut paid = Vec::new();
        for (pay, amount, _) in amounts {
            let owed = amount + credit.get(&pay).copied().unwrap_or(0);
            if owed >= params::MIN_PAYOUT {
                paid.push((pay, owed));
                credit.remove(&pay);
            } else if owed > 0 {
                credit.insert(pay, owed);
            }
        }
        (paid, pot, credit)
    }

    /// What an address is owed but not yet paid (a share under the payout minimum).
    pub fn credit_of(&self, pay: &Payout) -> u64 {
        self.credit.get(pay).copied().unwrap_or(0)
    }

    /// The (block id, seed) of block `h` on this branch (1 and up).
    pub fn block_at(&self, h: u64) -> Option<(Hash256, Hash)> {
        h.checked_sub(1).and_then(|i| self.blocks.get(i as usize)).copied()
    }

    /// A commit that waits for its proof, by replica id: (segment, owner, replica root, height).
    pub fn commit(&self, replica: &Hash) -> Option<(Hash, Key, Hash, u64)> {
        self.commits.get(replica).map(|c| (c.segment, c.owner, c.replica_root, c.height))
    }

    /// Whether a replica is registered.
    pub fn registered(&self, replica: &Hash) -> bool {
        self.replicas.contains_key(replica)
    }

    /// Every registered replica: (replica id, segment, owner, weight it gives).
    pub fn all_replicas(&self) -> Vec<(Hash, Hash, Key, u64)> {
        self.replicas.iter().map(|(id, r)| (*id, r.segment, r.owner, r.bytes / self.holders_of(&r.segment).max(1))).collect()
    }

    /// Of `items`, in order, the ones the next block (at `height`) can carry.
    pub fn fitting(&self, items: &[Item], height: u64) -> Vec<Item> {
        let mut copy = self.clone();
        let mut out = Vec::new();
        for i in items {
            let undo = copy.items(std::slice::from_ref(i), height);
            if let Ok(u) = undo {
                out.push(i.clone());
                // Keep the effect so later items are checked after it.
                drop(u);
            }
        }
        out
    }

    /// Apply storage items in order; on an error nothing is changed.
    fn items(&mut self, items: &[Item], height: u64) -> Result<(catalog::Undo, Vec<Op>), BlockError> {
        let w = Weights(self, self.total());
        let cat_items: Vec<Item> = items.iter().filter(|i| matches!(i, Item::CatalogEntry(_) | Item::Attest(_))).cloned().collect();
        let mut catalog = self.catalog.clone();
        let cu = catalog.apply(&cat_items, height, &w)?;
        let mut ops = Vec::new();
        let mut staged = self.clone();
        staged.catalog = catalog;
        for item in items {
            let r = match item {
                Item::CatalogEntry(_) | Item::Attest(_) => Ok(()),
                Item::RegCommit(c) => staged.commit_item(c, height, &mut ops),
                Item::RegProof(p) => staged.proof_item(p, height, &mut ops),
                _ => Err(no("this chain takes catalog items and registrations only (audits come later)")),
            };
            r?;
        }
        *self = staged;
        Ok((cu, ops))
    }

    fn commit_item(&mut self, c: &RegCommit, height: u64, ops: &mut Vec<Op>) -> Result<(), BlockError> {
        self.catalog.get(&c.segment).ok_or(no("registration: the segment isn't listed"))?;
        if c.anchor == 0 || c.anchor >= height || height - c.anchor > ANCHOR_AGE {
            return Err(no("registration: the anchor block is too old or not before it"));
        }
        let (anchor_id, _) = self.block_at(c.anchor).ok_or(no("registration: no such anchor block"))?;
        if !block::verify(&c.owner, &c.message(&self.net, &anchor_id), &c.sig) {
            return Err(BlockError::BadSignature("registration commit"));
        }
        if block::work_bits(&c.stamp(&self.net, &anchor_id)) < STAMP_BITS {
            return Err(no("registration: the work stamp is too small"));
        }
        script::pay_to(&self.net, &c.payout).ok_or(no("registration: the payout address isn't one of this chain's"))?;
        let id = c.replica(&self.net);
        if self.replicas.contains_key(&id) {
            return Err(no("registration: that replica is registered already"));
        }
        // A commit whose proof window has passed may be made again.
        if let Some(old) = self.commits.get(&id) {
            if height <= old.height + params::CHALLENGE_DELAY + params::ANSWER_WINDOW {
                return Err(no("registration: a commit for that replica waits for its proof"));
            }
        }
        let prev = self.commits.insert(id, Commit { segment: c.segment, owner: c.owner, payout: c.payout, replica_root: c.replica_root, height });
        ops.push(Op::Committed(id, prev));
        Ok(())
    }

    fn proof_item(&mut self, p: &RegProof, height: u64, ops: &mut Vec<Op>) -> Result<(), BlockError> {
        let c = self.commits.get(&p.replica).cloned().ok_or(no("registration proof: no commit waits for it"))?;
        let challenge = c.height + params::CHALLENGE_DELAY;
        if height <= challenge || height > challenge + params::ANSWER_WINDOW {
            return Err(no("registration proof: not in its window"));
        }
        let (_, seed) = self.block_at(challenge).ok_or(no("registration proof: no challenge block"))?;
        let entry = self.catalog.get(&c.segment).ok_or(no("registration proof: the segment is no longer listed"))?.entry.clone();
        let leaves = entry.leaves();
        let want = picks(&self.net, &seed, &p.replica, leaves);
        if p.leaves.len() != want.len() {
            return Err(no("registration proof: not the leaves picked"));
        }
        for (o, &i) in p.leaves.iter().zip(&want) {
            if !merkle::verify(&c.replica_root, leaves, i, &merkle::leaf_hash(&o.sealed), &o.sealed_path) {
                return Err(no("registration proof: a sealed leaf isn't in the replica"));
            }
            if !merkle::verify(&entry.root, leaves, i, &merkle::leaf_hash(&o.data), &o.data_path) {
                return Err(no("registration proof: a data leaf isn't in the segment"));
            }
            if sloth::unseal_leaf(&c.owner, &c.segment, i, &o.sealed).ok() != Some(o.data) {
                return Err(no("registration proof: a sealed leaf doesn't unseal to the data"));
            }
        }
        self.commits.remove(&p.replica);
        self.replicas.insert(p.replica, Replica { segment: c.segment, owner: c.owner, payout: c.payout, bytes: entry.bytes });
        ops.push(Op::Proved(p.replica, c));
        Ok(())
    }
}

impl Storage for Registry {
    type Undo = Undo;

    fn weight(&self, key: &Key) -> u64 {
        let g = self.genesis.get(key).map_or(0, |x| x.0);
        let r: u64 = self.replicas.values().filter(|r| r.owner == *key).map(|r| r.bytes / self.holders_of(&r.segment).max(1)).sum();
        g + r
    }

    /// Weight can be gained at any block while replicas count from their proof, so the bound
    /// is what any key could hold: its genesis weight and every listed segment's bytes.
    fn weight_bound(&self, key: &Key) -> u64 {
        let g = self.genesis.get(key).map_or(0, |x| x.0);
        let all: u64 = self.catalog.totals().1;
        g.saturating_add(all)
    }

    fn payouts(&self, height: u64) -> Vec<(u64, Vec<u8>)> {
        self.settle(height).0.into_iter().filter_map(|(pay, amount)| Some((amount, script::pay_to(&self.net, &pay)?))).collect()
    }

    fn apply(&mut self, block: &Block, at: &Applied) -> Result<Self::Undo, BlockError> {
        let before = self.pot;
        // The epoch's settlement, worked out before the block's items: as the coinbase was checked.
        let settled = (at.height % params::EPOCH_BLOCKS == 0).then(|| self.settle_on(at.height, before));
        // The block's own seed is known to its items only from the next block on.
        let (catalog, ops) = self.items(&block.items, at.height)?;
        let credit_before = settled.map(|(_, shared, credit)| {
            self.pot = before - shared;
            std::mem::replace(&mut self.credit, credit)
        });
        // Fees join the pot with the block reward (§14.18).
        self.pot += params::BLOCK_REWARD + at.fees;
        self.blocks.push((at.id, at.seed));
        Ok(Undo { pot: before, credit: credit_before, catalog, ops })
    }

    fn undo(&mut self, u: Self::Undo) {
        self.blocks.pop();
        if let Some(c) = u.credit {
            self.credit = c;
        }
        for op in u.ops.into_iter().rev() {
            match op {
                Op::Committed(id, prev) => {
                    match prev {
                        Some(c) => self.commits.insert(id, c),
                        None => self.commits.remove(&id),
                    };
                }
                Op::Proved(id, c) => {
                    self.replicas.remove(&id);
                    self.commits.insert(id, c);
                }
            }
        }
        self.catalog.undo(u.catalog);
        self.pot = u.pot;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{CatalogEntry, Opened};
    use crate::params::{RANGE, TESTNET};
    use crate::sloth::{DATA_LEAF, SEALED_LEAF};
    use ed25519_dalek::SigningKey;

    const NET: Net = TESTNET;

    fn key(who: u8) -> (SigningKey, Key) {
        let s = SigningKey::from_bytes(&[who; 32]);
        let k = s.verifying_key().to_bytes();
        (s, k)
    }

    fn pay(who: u8) -> Payout {
        let mut a = [who; 21];
        a[0] = NET.p2pkh_version;
        a
    }

    /// A registry whose genesis holder (key 1) has listed one segment of 10,000 heights; the
    /// segment's bytes.
    fn listed() -> (Registry, CatalogEntry, Vec<u8>) {
        let (s1, k1) = key(1);
        let mut r = Registry::new(&NET, &[(k1, 1 << 30, pay(1))], 1).unwrap();
        let data: Vec<u8> = (0..RANGE * 8 + 1_000).map(|i| (i * 7 + 3) as u8).collect();
        let hashes: Vec<Hash> = data.chunks(DATA_LEAF).map(|c| { let mut l = [0u8; DATA_LEAF]; l[..c.len()].copy_from_slice(c); merkle::leaf_hash(&l) }).collect();
        let mut e = CatalogEntry { chain: "peptest".into(), family: 0, first: 0, last: RANGE - 1, bytes: data.len() as u64, manifest: [0; 32], root: merkle::root(&hashes).unwrap(), poster: k1, sig: [0; 64] };
        e.sig = block::sign(&s1, &e.message(&NET));
        r.items(&[Item::CatalogEntry(e.clone())], 1).unwrap();
        r.blocks.push((Hash256([1; 32]), [11; 32]));
        assert!(r.catalog.get(&e.segment(&NET)).is_some(), "the holder alone is all the weight: confirmed");
        (r, e, data)
    }

    fn leaf(data: &[u8], i: u64) -> [u8; DATA_LEAF] {
        let mut l = [0u8; DATA_LEAF];
        let at = (i as usize) * DATA_LEAF;
        let n = (data.len() - at).min(DATA_LEAF);
        l[..n].copy_from_slice(&data[at..at + n]);
        l
    }

    struct Sealed {
        sealed: Vec<[u8; SEALED_LEAF]>,
        root: Hash,
    }

    fn seal(owner: &Key, e: &CatalogEntry, data: &[u8]) -> Sealed {
        let seg = e.segment(&NET);
        let sealed: Vec<[u8; SEALED_LEAF]> = (0..e.leaves()).map(|i| sloth::seal_leaf(owner, &seg, i, &leaf(data, i))).collect();
        let hs: Vec<Hash> = sealed.iter().map(|s| merkle::leaf_hash(s)).collect();
        Sealed { root: merkle::root(&hs).unwrap(), sealed }
    }

    fn commit(r: &Registry, who: u8, e: &CatalogEntry, root: Hash, anchor: u64, bits: u32) -> RegCommit {
        let (s, k) = key(who);
        let (aid, _) = r.block_at(anchor).unwrap();
        let mut c = RegCommit { segment: e.segment(&NET), owner: k, payout: pay(who), replica_root: root, anchor, nonce: 0, sig: [0; 64] };
        while block::work_bits(&c.stamp(&NET, &aid)) < bits {
            c.nonce += 1;
        }
        c.sig = block::sign(&s, &c.message(&NET, &aid));
        c
    }

    fn proof(r: &Registry, replica: Hash, s: &Sealed, e: &CatalogEntry, data: &[u8], challenge: u64) -> RegProof {
        let (_, seed) = r.block_at(challenge).unwrap();
        let sh: Vec<Hash> = s.sealed.iter().map(|x| merkle::leaf_hash(x)).collect();
        let dh: Vec<Hash> = (0..e.leaves()).map(|i| merkle::leaf_hash(&leaf(data, i))).collect();
        let leaves = picks(&NET, &seed, &replica, e.leaves())
            .into_iter()
            .map(|i| Opened { sealed: s.sealed[i as usize], sealed_path: merkle::path(&sh, i).unwrap(), data: leaf(data, i), data_path: merkle::path(&dh, i).unwrap() })
            .collect();
        RegProof { replica, leaves }
    }

    /// Blocks up to `h` (ids and seeds made up).
    fn grow(r: &mut Registry, h: u64) {
        while (r.blocks.len() as u64) < h {
            let n = r.blocks.len() as u8 + 1;
            r.blocks.push((Hash256([n; 32]), [n.wrapping_mul(31); 32]));
        }
    }

    #[test]
    fn anyone_registers_by_sealing_committing_and_proving() {
        let (mut r, e, data) = listed();
        let (_, k2) = key(2);
        assert_eq!(r.weight(&k2), 0);
        let s2 = seal(&k2, &e, &data);
        grow(&mut r, 5);
        // A stamp with too little work, and one that holds.
        assert!(r.items(&[Item::RegCommit(commit(&r, 2, &e, s2.root, 5, 4))], 6).is_err());
        let c = commit(&r, 2, &e, s2.root, 5, STAMP_BITS);
        let id = c.replica(&NET);
        r.items(&[Item::RegCommit(c.clone())], 6).unwrap();
        assert!(r.items(&[Item::RegCommit(c.clone())], 7).is_err(), "one commit waits for a replica at a time");
        // The proof: not before the challenge block's seed is known, then within the window.
        grow(&mut r, 16);
        let p = proof(&r, id, &s2, &e, &data, 16);
        assert!(r.items(&[Item::RegProof(p.clone())], 16).is_err(), "too early");
        // A forged leaf: someone else's sealing doesn't unseal with this owner's key.
        let (_, k3) = key(3);
        let s3 = seal(&k3, &e, &data);
        let mut forged = p.clone();
        forged.leaves[0].sealed = s3.sealed[0];
        assert!(r.items(&[Item::RegProof(forged)], 17).is_err());
        let w_before = r.weight(&key(1).1);
        r.items(&[Item::RegProof(p.clone())], 17).unwrap();
        assert!(r.registered(&id));
        assert_eq!(r.weight(&k2), e.bytes, "the replica counts from its proof");
        assert_eq!(r.weight(&key(1).1), w_before, "the genesis holder keeps its weight");
        // A second replica of the same segment: the bytes are split between the two.
        grow(&mut r, 20);
        let c3 = commit(&r, 3, &e, s3.root, 20, STAMP_BITS);
        r.items(&[Item::RegCommit(c3.clone())], 21).unwrap();
        grow(&mut r, 31);
        r.items(&[Item::RegProof(proof(&r, c3.replica(&NET), &s3, &e, &data, 31))], 32).unwrap();
        assert_eq!(r.weight(&k2), e.bytes / 2);
        assert_eq!(r.weight(&k3), e.bytes / 2);
        // Pay at the epoch's end: shared by weight.
        r.pot = 100_000 * params::COIN;
        let pays = r.payouts(params::EPOCH_BLOCKS);
        assert_eq!(pays.len(), 3);
        assert!(pays.iter().map(|p| p.0).sum::<u64>() <= 100_000 * params::COIN);
        assert_eq!(pays[1].0, pays[2].0, "the two replicas of one segment are paid alike");
        // Too late: a commit's proof window passes.
        grow(&mut r, 40);
        let (_, k4) = key(4);
        let s4 = seal(&k4, &e, &data);
        let c4 = commit(&r, 4, &e, s4.root, 40, STAMP_BITS);
        r.items(&[Item::RegCommit(c4.clone())], 41).unwrap();
        grow(&mut r, 80);
        let late = proof(&r, c4.replica(&NET), &s4, &e, &data, 51);
        assert!(r.items(&[Item::RegProof(late)], 41 + params::CHALLENGE_DELAY + params::ANSWER_WINDOW + 1).is_err());
    }

    /// Three genesis holders with these weights (keys and addresses 1, 2, 3).
    fn three(w: [u64; 3]) -> Registry {
        let hs: Vec<(Key, u64, Payout)> = (0..3).map(|i| (key(i as u8 + 1).1, w[i], pay(i as u8 + 1))).collect();
        Registry::new(&NET, &hs, 1).unwrap()
    }

    #[test]
    fn a_settlement_pays_whole_units_and_the_leftovers_to_the_largest_remainders() {
        let m = params::MIN_PAYOUT;
        // Equal weights, 2 units over: the tie goes to the lower addresses.
        let r = three([1, 1, 1]);
        let (paid, shared, credit) = r.settle_on(100, 3 * m + 2);
        assert_eq!(paid, vec![(pay(1), m + 1), (pay(2), m + 1), (pay(3), m)]);
        assert_eq!((shared, credit.len()), (3 * m + 2, 0));
        // Weights 1, 1, 2 and a pot that leaves remainders 1, 1, 2 (of 4): the one unit left
        // goes to the heaviest, though its address is the highest.
        let r = three([1, 1, 2]);
        let pot = 100 * m + 5;
        let (paid, shared, _) = r.settle_on(100, pot);
        assert_eq!(paid, vec![(pay(1), 25 * m + 1), (pay(2), 25 * m + 1), (pay(3), 50 * m + 3)]);
        assert_eq!(paid.iter().map(|p| p.1).sum::<u64>(), shared);
        // Every unit of the pot is paid or carried, never lost.
        for pot in [0, 1, 2, 7, m - 1, m, 3 * m - 1, 12_345_678_901] {
            for w in [[1, 1, 1], [1, 2, 3], [5, 7, 1 << 40]] {
                let (paid, shared, credit) = three(w).settle_on(100, pot);
                assert_eq!(shared, pot);
                assert_eq!(paid.iter().map(|p| p.1).sum::<u64>() + credit.values().sum::<u64>(), pot, "{pot} {w:?}");
            }
        }
        // Off the epoch's end, nothing is settled.
        let mut r = three([1, 1, 1]);
        r.pot = 9 * m;
        assert_eq!(r.settle(99).0, vec![]);
        assert_eq!(r.settle(100).0.len(), 3);
    }

    #[test]
    fn a_share_too_small_to_pay_is_carried_to_the_next_settlement() {
        let m = params::MIN_PAYOUT;
        // Holder 3 is a thousandth of the weight: its share of a 10 × min pot is under the minimum.
        let mut r = three([500, 499, 1]);
        let (paid, _, credit) = r.settle_on(100, 10 * m);
        assert_eq!(paid.iter().map(|p| p.0).collect::<Vec<_>>(), vec![pay(1), pay(2)]);
        assert_eq!(credit, BTreeMap::from([(pay(3), 10 * m / 1000)]));
        // It builds up epoch by epoch and is paid with the epoch that brings it over.
        r.credit = credit;
        for _ in 0..98 {
            let (paid, _, credit) = r.settle_on(200, 10 * m);
            assert!(paid.iter().all(|p| p.0 != pay(3)));
            r.credit = credit;
        }
        assert_eq!(r.credit_of(&pay(3)), 99 * 10 * m / 1000);
        let (paid, _, credit) = r.settle_on(300, 10 * m);
        assert_eq!(paid.last(), Some(&(pay(3), m)));
        assert!(credit.is_empty());
    }
}
