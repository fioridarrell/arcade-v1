//! The genesis list (docs/ARCADECHAIN.md §10): how block 0 gets its first writers without a first-writer grab.
//!
//! Before there is a chain, arcades seal segments and send registration commits to a public list. The list closes at a
//! set time; two Bitcoin block hashes taken after that (beacons nobody could know when it closed) then pick what every
//! listed replica must show. Every check is the chain's own (registry `check_reg_proof`, `check_opening`); only the seeds
//! differ, since there are no blocks yet:
//!
//! ```text
//! anchor of every list commit   list_anchor(net)                    (in place of a recent block's id)
//! proof leaves, audit leaves    proof_seed = H(closed hash, beacon 1)  (in place of the challenge / picking block)
//! audit openings                open_seed  = H(closed hash, beacon 2)  (in place of the block after the commit)
//! ```
//!
//! [`finalize`] keeps every owner whose proof holds and every replica whose full audit passes, and strikes the rest with
//! a reason. Its [`FinalList`] is what block 0 carries (by hash) and what [`crate::registry::Registry::from_list`] starts
//! from. The first epoch's draw seed comes from the *closed* list's hash and beacon 2 (chain.rs `Genesis::draw_seed`),
//! so leaving one's own openings out can't steer it.
//!
//! The assembler (the arcade that keeps the bulletin) signs a [`receipt_message`] for every commit it takes in: a receipt
//! for a commit missing from the list proves it was left out.

use crate::block::{self, AuditCommit, AuditOpening, BlockError, CatalogEntry, Key, Payout, RegCommit, RegProof};
use crate::merkle::Hash;
use crate::params::Net;
use crate::registry::{self, STAMP_BITS};
use crate::tagged;
use arcade_chain::Hash256;
use std::collections::{BTreeMap, BTreeSet};

/// What every list commit is anchored to, in place of a recent block: the network's genesis list itself.
pub fn list_anchor(net: &Net) -> Hash256 {
    Hash256(tagged::hash(net, "genesis list", &[]))
}

/// A commit's id on the bulletin (what a receipt names).
pub fn commit_id(net: &Net, c: &RegCommit) -> Hash {
    let mut pairs = Vec::with_capacity(c.replicas.len() * 64);
    for (seg, root) in &c.replicas {
        pairs.extend_from_slice(seg);
        pairs.extend_from_slice(root);
    }
    tagged::hash(net, "genesis commit", &[&c.owner, &c.payout, &pairs, &c.nonce.to_le_bytes(), &c.sig])
}

/// What the assembler signs for each commit it takes in.
pub fn receipt_message(net: &Net, c: &RegCommit) -> Vec<u8> {
    tagged::message(net, "genesis receipt", &[&commit_id(net, c)])
}

/// A list commit on its own: 1 to MAX_BATCH replicas, each segment once, anchored to the list (anchor 0), signed by the
/// owner, stamped with the chain's work, paying to one of this network's addresses.
pub fn check_commit(net: &Net, c: &RegCommit) -> Result<(), BlockError> {
    if c.replicas.is_empty() || c.replicas.len() > block::MAX_BATCH || c.anchor != 0 {
        return Err(BlockError::Invalid("genesis commit: 1 to MAX_BATCH replicas, anchored to the list"));
    }
    let segs: BTreeSet<&Hash> = c.replicas.iter().map(|r| &r.0).collect();
    if segs.len() != c.replicas.len() {
        return Err(BlockError::Invalid("genesis commit: a segment twice"));
    }
    let anchor = list_anchor(net);
    if !block::verify(&c.owner, &c.message(net, &anchor), &c.sig) {
        return Err(BlockError::BadSignature("genesis commit"));
    }
    if block::work_bits(&c.stamp(net, &anchor)) < STAMP_BITS {
        return Err(BlockError::Invalid("genesis commit: the work stamp is too small"));
    }
    crate::script::pay_to(net, &c.payout).ok_or(BlockError::Invalid("genesis commit: the payout address isn't one of this chain's"))?;
    Ok(())
}

/// The list as it closed (§10, 12-28 00:00): the catalog entries offered and the commits taken in.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClosedList {
    pub entries: Vec<CatalogEntry>,
    pub commits: Vec<RegCommit>,
}

impl ClosedList {
    /// Its hash, published when it closes: over the entries' segment ids and the commits' ids, each sorted, so the order
    /// things arrived in doesn't matter.
    pub fn hash(&self, net: &Net) -> Hash {
        let mut segs: Vec<Hash> = self.entries.iter().map(|e| e.segment(net)).collect();
        let mut commits: Vec<Hash> = self.commits.iter().map(|c| commit_id(net, c)).collect();
        segs.sort_unstable();
        commits.sort_unstable();
        tagged::hash(net, "closed genesis list", &[&segs.concat(), &commits.concat()])
    }
}

/// Where the proofs' leaves and the audits' leaves come from: beacon 1 after the list closed.
pub fn proof_seed(net: &Net, closed: &Hash, beacon_1: &Hash) -> Hash {
    tagged::hash(net, "genesis proof seed", &[closed, beacon_1])
}

/// Where the audits' openings come from: beacon 2, after the commits `A` are in.
pub fn open_seed(net: &Net, closed: &Hash, beacon_2: &Hash) -> Hash {
    tagged::hash(net, "genesis open seed", &[closed, beacon_2])
}

/// The id an audit commit at genesis signs over, in place of its picking block's (`AuditCommit::message`).
pub fn audit_pick_id(seed: &Hash) -> Hash256 {
    Hash256(*seed)
}

/// What one owner posts after the beacons: its batch's registration proof, then for every replica its audit commit `A`
/// (after beacon 1) and its opening (after beacon 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answers {
    pub proof: RegProof,
    pub audits: Vec<(AuditCommit, AuditOpening)>,
}

/// One owner of the final list: where it is paid, and its replicas (segment, root over its sealed leaves).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ListedOwner {
    pub owner: Key,
    pub payout: Payout,
    pub replicas: Vec<(Hash, Hash)>,
}

/// The final list (§10, 12-31): what block 0 carries by hash.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FinalList {
    pub entries: Vec<CatalogEntry>,
    pub owners: Vec<ListedOwner>,
}

impl FinalList {
    pub fn hash(&self, net: &Net) -> Hash {
        let mut segs: Vec<Hash> = self.entries.iter().map(|e| e.segment(net)).collect();
        segs.sort_unstable();
        let mut owners: Vec<Vec<u8>> = self
            .owners
            .iter()
            .map(|o| {
                let mut v = o.owner.to_vec();
                v.extend_from_slice(&o.payout);
                for (s, r) in &o.replicas {
                    v.extend_from_slice(s);
                    v.extend_from_slice(r);
                }
                tagged::hash(net, "final genesis owner", &[&v]).to_vec()
            })
            .collect();
        owners.sort_unstable();
        tagged::hash(net, "final genesis list", &[&segs.concat(), &owners.concat()])
    }
}

/// Why something was left off the final list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Struck {
    pub owner: Key,
    /// The replica's segment; None when the whole batch was struck.
    pub segment: Option<Hash>,
    pub why: String,
}

/// The entries that can stand at block 0: for each chain and family, the run from height 0 with no gap, where no range
/// is claimed by two different entries (two roots for one range: one of them is wrong, and a fraud proof must settle it
/// before the list is final; until then neither stands, nor anything after it).
fn standing(net: &Net, entries: &[CatalogEntry]) -> BTreeMap<Hash, CatalogEntry> {
    let mut by_line: BTreeMap<(String, u8), BTreeMap<u64, Vec<&CatalogEntry>>> = BTreeMap::new();
    for e in entries {
        by_line.entry((e.chain.clone(), e.family)).or_default().entry(e.first).or_default().push(e);
    }
    let mut out = BTreeMap::new();
    for (_, starts) in by_line {
        let mut next = 0u64;
        for (first, es) in starts {
            let ids: BTreeSet<Hash> = es.iter().map(|e| e.segment(net)).collect();
            if first != next || ids.len() != 1 {
                break;
            }
            out.insert(es[0].segment(net), es[0].clone());
            next = es[0].last + 1;
        }
    }
    out
}

/// The final list from the closed list, the two beacons and the answers posted: who stays, and who was struck and why.
/// `segment_bytes` is the network's segment size (a listed entry must be a full segment). Deterministic: any arcade
/// with the same inputs gets the same list.
pub fn finalize(net: &Net, segment_bytes: u64, closed: &ClosedList, beacon_1: &Hash, beacon_2: &Hash, answers: &BTreeMap<Key, Answers>) -> (FinalList, Vec<Struck>) {
    let closed_hash = closed.hash(net);
    let pseed = proof_seed(net, &closed_hash, beacon_1);
    let oseed = open_seed(net, &closed_hash, beacon_2);
    let pick_id = audit_pick_id(&pseed);
    let mut entries = standing(net, &closed.entries);
    // Entries the catalog wouldn't take at all (not full, bad signature...) can't stand either; checked by listing them.
    entries.retain(|_, e| check_entry_alone(net, segment_bytes, e));
    let mut struck = Vec::new();
    // One batch per owner: the one with the most work in its stamp (then the lower commit id).
    let mut best: BTreeMap<Key, &RegCommit> = BTreeMap::new();
    for c in &closed.commits {
        if let Err(e) = check_commit(net, c) {
            struck.push(Struck { owner: c.owner, segment: None, why: e.to_string() });
            continue;
        }
        let work = |c: &RegCommit| (block::work_bits(&c.stamp(net, &list_anchor(net))), std::cmp::Reverse(commit_id(net, c)));
        match best.get(&c.owner) {
            Some(b) if work(b) >= work(c) => struck.push(Struck { owner: c.owner, segment: None, why: "another batch of this owner has more work".into() }),
            Some(b) => {
                struck.push(Struck { owner: b.owner, segment: None, why: "another batch of this owner has more work".into() });
                best.insert(c.owner, c);
            }
            None => {
                best.insert(c.owner, c);
            }
        }
    }
    let mut owners = Vec::new();
    for (owner, c) in best {
        let strike = |why: &str| Struck { owner, segment: None, why: why.to_string() };
        let batch: Option<Vec<CatalogEntry>> = c.replicas.iter().map(|(seg, _)| entries.get(seg).cloned()).collect();
        let Some(batch) = batch else {
            struck.push(strike("a segment of the batch isn't on the list"));
            continue;
        };
        let Some(a) = answers.get(&owner) else {
            struck.push(strike("no answers posted"));
            continue;
        };
        let roots: Vec<Hash> = c.replicas.iter().map(|r| r.1).collect();
        if a.proof.owner != owner {
            struck.push(strike("the proof is another owner's"));
            continue;
        }
        if let Err(e) = registry::check_reg_proof(net, &pseed, &owner, &batch, &roots, &a.proof.leaves) {
            struck.push(strike(&e.to_string()));
            continue;
        }
        // Every replica's full audit: its commit A signed over the proof seed, its opening against A.
        let mut kept = Vec::new();
        for ((seg, root), entry) in c.replicas.iter().zip(&batch) {
            let id = block::replica_id(net, seg, &owner);
            let why = match a.audits.iter().find(|(ac, _)| ac.replica == id) {
                None => Some("no audit posted".to_string()),
                Some((ac, op)) if op.replica != id => {
                    let _ = ac;
                    Some("the opening is another replica's".to_string())
                }
                Some((ac, _)) if !block::verify(&owner, &ac.message(net, &pick_id), &ac.sig) => Some("the audit commit's signature".to_string()),
                Some((ac, op)) => registry::check_opening(net, &owner, entry, root, &ac.root, &pseed, &oseed, op).err().map(|e| e.to_string()),
            };
            match why {
                None => kept.push((*seg, *root)),
                Some(why) => struck.push(Struck { owner, segment: Some(*seg), why }),
            }
        }
        if !kept.is_empty() {
            owners.push(ListedOwner { owner, payout: c.payout, replicas: kept });
        }
    }
    // Entries with no replica left stand for nothing; a chain's run stops at the first such gap.
    let held: BTreeSet<Hash> = owners.iter().flat_map(|o| o.replicas.iter().map(|r| r.0)).collect();
    let mut by_line: BTreeMap<(String, u8), Vec<CatalogEntry>> = BTreeMap::new();
    for e in entries.values() {
        by_line.entry((e.chain.clone(), e.family)).or_default().push(e.clone());
    }
    let mut final_entries = Vec::new();
    for (_, mut es) in by_line {
        es.sort_by_key(|e| e.first);
        for e in es {
            if !held.contains(&e.segment(net)) {
                break;
            }
            final_entries.push(e);
        }
    }
    let listed: BTreeSet<Hash> = final_entries.iter().map(|e| e.segment(net)).collect();
    for o in &mut owners {
        let owner = o.owner;
        o.replicas.retain(|(seg, _)| {
            let on = listed.contains(seg);
            if !on {
                struck.push(Struck { owner, segment: Some(*seg), why: "its segment comes after a gap in its chain".into() });
            }
            on
        });
    }
    owners.retain(|o| !o.replicas.is_empty());
    (FinalList { entries: final_entries, owners }, struck)
}

/// An entry's own checks (chain, family, whole ranges, a full segment, its poster's signature), as block 0's catalog makes them.
fn check_entry_alone(net: &Net, segment_bytes: u64, e: &CatalogEntry) -> bool {
    let least = (e.last - e.first + 1).checked_mul(crate::segment::ROW).and_then(|t| t.checked_add(segment_bytes));
    net.is_listed_at(&e.chain, 0)
        && crate::segment::family_at(net, e.family, 0)
        && crate::segment::whole_ranges(e.first, e.last)
        && least.is_some_and(|l| e.bytes >= l)
        && block::verify(&e.poster, &e.message(net), &e.sig)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::Opened;
    use crate::chain::Storage;
    use crate::merkle;
    use crate::params::{self, RANGE, TESTNET};
    use crate::registry::{audit_leaf, picks, Registry};
    use crate::sloth::{self, DATA_LEAF, SEALED_LEAF};
    use crate::draw;
    use ed25519_dalek::SigningKey;

    const NET: Net = TESTNET;
    const SEG_BYTES: u64 = 1;

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

    struct Seg {
        e: CatalogEntry,
        data: Vec<u8>,
    }

    fn leaf(data: &[u8], i: u64) -> [u8; DATA_LEAF] {
        let mut l = [0u8; DATA_LEAF];
        let at = (i as usize) * DATA_LEAF;
        let n = (data.len() - at).min(DATA_LEAF);
        l[..n].copy_from_slice(&data[at..at + n]);
        l
    }

    /// Segment `s` of peptest blocks (posted by key 9), its bytes varied by `salt` (another salt: another root).
    fn seg(s: u64, salt: u8) -> Seg {
        let (s9, k9) = key(9);
        let data: Vec<u8> = (0..RANGE * 8 + 1_000 + s * 300).map(|i| (i * 7 + 3 + s) as u8 ^ salt).collect();
        let hashes: Vec<Hash> = (0..data.len().div_ceil(DATA_LEAF) as u64).map(|i| merkle::leaf_hash(&leaf(&data, i))).collect();
        let mut e = CatalogEntry { chain: "peptest".into(), family: 0, first: s * RANGE, last: s * RANGE + RANGE - 1, bytes: data.len() as u64, manifest: [0; 32], root: merkle::root(&hashes).unwrap(), poster: k9, sig: [0; 64] };
        e.sig = block::sign(&s9, &e.message(&NET));
        Seg { e, data }
    }

    struct Sealed {
        sealed: Vec<[u8; SEALED_LEAF]>,
        root: Hash,
    }

    fn seal(owner: &Key, s: &Seg) -> Sealed {
        let slot = s.e.slot(&NET);
        let sealed: Vec<[u8; SEALED_LEAF]> = (0..s.e.leaves()).map(|i| sloth::seal_leaf(owner, &slot, i, &leaf(&s.data, i))).collect();
        let hs: Vec<Hash> = sealed.iter().map(|x| merkle::leaf_hash(x)).collect();
        Sealed { root: merkle::root(&hs).unwrap(), sealed }
    }

    fn opened(g: &Seg, x: &Sealed, i: u64) -> Opened {
        let sh: Vec<Hash> = x.sealed.iter().map(|y| merkle::leaf_hash(y)).collect();
        let dh: Vec<Hash> = (0..g.e.leaves()).map(|j| merkle::leaf_hash(&leaf(&g.data, j))).collect();
        Opened { sealed: x.sealed[i as usize], sealed_path: merkle::path(&sh, i).unwrap(), data: leaf(&g.data, i), data_path: merkle::path(&dh, i).unwrap() }
    }

    fn commit(who: u8, batch: &[(&Seg, &Sealed)]) -> RegCommit {
        let (s, k) = key(who);
        let mut c = RegCommit { owner: k, payout: pay(who), replicas: batch.iter().map(|(g, x)| (g.e.segment(&NET), x.root)).collect(), anchor: 0, nonce: 0, sig: [0; 64] };
        c.finish(&NET, &list_anchor(&NET), STAMP_BITS, &s);
        c
    }

    /// An owner's honest answers to both beacons.
    fn answers(who: u8, batch: &[(&Seg, &Sealed)], pseed: &Hash, oseed: &Hash) -> Answers {
        let (s, k) = key(who);
        let counts: Vec<u64> = batch.iter().map(|(g, _)| g.e.leaves()).collect();
        let leaves = picks(&NET, pseed, &k, &counts).into_iter().map(|(at, i)| opened(batch[at].0, batch[at].1, i)).collect();
        let audits = batch
            .iter()
            .map(|(g, x)| {
                let id = block::replica_id(&NET, &g.e.segment(&NET), &k);
                let ah: Vec<Hash> = x.sealed.iter().enumerate().map(|(i, y)| audit_leaf(&NET, pseed, i as u64, y)).collect();
                let mut ac = AuditCommit { replica: id, pick: 0, root: merkle::root(&ah).unwrap(), sig: [0; 64] };
                ac.sig = block::sign(&s, &ac.message(&NET, &audit_pick_id(pseed)));
                let n = g.e.leaves();
                let ls = (0..params::AUDIT_OPENINGS as u32)
                    .map(|j| {
                        let i = draw::leaf_pick(&NET, oseed, &id, j, n);
                        (opened(g, x, i), merkle::path(&ah, i).unwrap())
                    })
                    .collect();
                (ac, AuditOpening { replica: id, leaves: ls })
            })
            .collect();
        Answers { proof: RegProof { owner: k, leaves }, audits }
    }

    #[test]
    fn the_list_keeps_who_proves_and_passes_and_strikes_the_rest() {
        let (g0, g1) = (seg(0, 0), seg(1, 0));
        let (_, k1) = key(1);
        let (_, k2) = key(2);
        let (_, k3) = key(3);
        let (x10, x11, x20, x21, x30) = (seal(&k1, &g0), seal(&k1, &g1), seal(&k2, &g0), seal(&k2, &g1), seal(&k3, &g0));
        let b1 = [(&g0, &x10), (&g1, &x11)];
        let b2 = [(&g0, &x20), (&g1, &x21)];
        let b3 = [(&g0, &x30)];
        let closed = ClosedList { entries: vec![g1.e.clone(), g0.e.clone()], commits: vec![commit(1, &b1), commit(2, &b2), commit(3, &b3)] };
        // Order of arrival doesn't change the closed hash.
        let shuffled = ClosedList { entries: vec![g0.e.clone(), g1.e.clone()], commits: closed.commits.iter().rev().cloned().collect() };
        assert_eq!(closed.hash(&NET), shuffled.hash(&NET));
        let (beacon_1, beacon_2) = ([21u8; 32], [22u8; 32]);
        let h = closed.hash(&NET);
        let (ps, os) = (proof_seed(&NET, &h, &beacon_1), open_seed(&NET, &h, &beacon_2));
        let mut posted = BTreeMap::new();
        posted.insert(k1, answers(1, &b1, &ps, &os));
        // Owner 2 opens one replica with a leaf of the wrong place: that replica is struck, the other stays.
        let mut a2 = answers(2, &b2, &ps, &os);
        let wrong = a2.audits[1].1.leaves[0].0.sealed_path.clone();
        a2.audits[1].1.leaves[0].0.sealed_path = a2.audits[1].1.leaves[1].0.sealed_path.clone();
        assert_ne!(wrong, a2.audits[1].1.leaves[0].0.sealed_path);
        posted.insert(k2, a2);
        // Owner 3 posts nothing.
        let (list, struck) = finalize(&NET, SEG_BYTES, &closed, &beacon_1, &beacon_2, &posted);
        assert_eq!(list.entries.len(), 2);
        let mut got: Vec<(Key, usize)> = list.owners.iter().map(|o| (o.owner, o.replicas.len())).collect();
        let mut want = vec![(k1, 2), (k2, 1)];
        got.sort();
        want.sort();
        assert_eq!(got, want);
        assert!(struck.iter().any(|s| s.owner == k2 && s.segment == Some(g1.e.segment(&NET))));
        assert!(struck.iter().any(|s| s.owner == k3 && s.segment.is_none() && s.why == "no answers posted"));
        // Answers for the other beacon fail: nobody could have made them before it.
        let mut early = BTreeMap::new();
        early.insert(k1, answers(1, &b1, &proof_seed(&NET, &h, &[99; 32]), &os));
        let (l2, _) = finalize(&NET, SEG_BYTES, &closed, &beacon_1, &beacon_2, &early);
        assert!(l2.owners.is_empty() && l2.entries.is_empty(), "no replica left, so no entry stands");
        // Block 0's registry: every listed replica counts for the draw from block 1; a segment's bytes are split among
        // its holders (segment 0 has two).
        let r = Registry::from_list(&NET, &[], &list, SEG_BYTES).unwrap();
        assert_eq!(r.weight(&k1), g0.e.bytes / 2 + g1.e.bytes);
        assert_eq!(r.weight(&k2), g0.e.bytes / 2);
        assert_eq!(r.weight(&k3), 0);
        assert_eq!(r.total_weight(), (g0.e.bytes + g1.e.bytes) as u128);
        // The final hash is the list's, not the order it was built in.
        let mut other = list.clone();
        other.owners.reverse();
        assert_eq!(list.hash(&NET), other.hash(&NET));
    }

    #[test]
    fn a_range_claimed_twice_stands_for_neither_nor_anything_after_it() {
        let (g0, g0b, g1) = (seg(0, 0), seg(0, 0x55), seg(1, 0));
        assert_ne!(g0.e.segment(&NET), g0b.e.segment(&NET));
        let (_, k1) = key(1);
        let (x0, x1) = (seal(&k1, &g0), seal(&k1, &g1));
        let b = [(&g0, &x0), (&g1, &x1)];
        let closed = ClosedList { entries: vec![g0.e.clone(), g0b.e.clone(), g1.e.clone()], commits: vec![commit(1, &b)] };
        let (b1, b2) = ([1u8; 32], [2u8; 32]);
        let h = closed.hash(&NET);
        let mut posted = BTreeMap::new();
        posted.insert(k1, answers(1, &b, &proof_seed(&NET, &h, &b1), &open_seed(&NET, &h, &b2)));
        let (list, struck) = finalize(&NET, SEG_BYTES, &closed, &b1, &b2, &posted);
        assert!(list.entries.is_empty() && list.owners.is_empty());
        assert_eq!(struck[0].why, "a segment of the batch isn't on the list");
        // With the wrong root out of the list (a fraud proof settled it), both stand.
        let fixed = ClosedList { entries: vec![g0.e.clone(), g1.e.clone()], ..closed };
        let h = fixed.hash(&NET);
        posted.insert(k1, answers(1, &b, &proof_seed(&NET, &h, &b1), &open_seed(&NET, &h, &b2)));
        let (list, _) = finalize(&NET, SEG_BYTES, &fixed, &b1, &b2, &posted);
        assert_eq!((list.entries.len(), list.owners.len()), (2, 1));
    }

    #[test]
    fn a_commit_must_be_anchored_to_the_list_signed_and_stamped() {
        let g = seg(0, 0);
        let (_, k1) = key(1);
        let x = seal(&k1, &g);
        let c = commit(1, &[(&g, &x)]);
        assert!(check_commit(&NET, &c).is_ok());
        assert!(check_commit(&NET, &RegCommit { anchor: 5, ..c.clone() }).is_err());
        assert!(check_commit(&NET, &RegCommit { payout: pay(7), ..c.clone() }).is_err(), "the signature covers the payout");
        let mut weak = c.clone();
        weak.nonce = weak.nonce.wrapping_add(1);
        assert!(check_commit(&NET, &weak).is_err());
        assert_ne!(receipt_message(&NET, &c), receipt_message(&NET, &weak));
    }
}
