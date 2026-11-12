//! Merkle trees over leaves (docs/ARCADECHAIN.md §2.4, §3.3, §5.2): the catalog root over a
//! segment's 127-byte data leaves, the replica root over its 128-byte sealed leaves, and the
//! audit root `A`.
//!
//! The tree is RFC 6962's: a leaf hashes as `SHA-256(0x00 ‖ leaf)`, a node as
//! `SHA-256(0x01 ‖ left ‖ right)`, and `n` leaves split at the largest power of two below `n`.
//! No leaf is ever doubled, so two different lists of leaves can't share a root.

use sha2::{Digest, Sha256};

pub type Hash = [u8; 32];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MerkleError {
    #[error("a tree has at least one leaf")]
    Empty,
    #[error("leaf {index} is outside a tree of {leaves} leaves")]
    OutOfRange { index: u64, leaves: u64 },
}

pub fn leaf_hash(leaf: &[u8]) -> Hash {
    let mut h = Sha256::new();
    h.update([0u8]);
    h.update(leaf);
    h.finalize().into()
}

pub fn node_hash(left: &Hash, right: &Hash) -> Hash {
    let mut h = Sha256::new();
    h.update([1u8]);
    h.update(left);
    h.update(right);
    h.finalize().into()
}

/// Builds a root from leaves fed in order, holding one hash per tree level (a segment has
/// about 8.5 million leaves; this never holds them all).
#[derive(Default)]
pub struct RootBuilder {
    /// Complete subtrees, largest first: `(height, hash)`.
    stack: Vec<(u32, Hash)>,
    leaves: u64,
}

impl RootBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push_leaf(&mut self, leaf: &[u8]) {
        self.push_hash(leaf_hash(leaf));
    }

    pub fn push_hash(&mut self, mut hash: Hash) {
        let mut height = 0;
        while let Some(&(top, left)) = self.stack.last() {
            if top != height {
                break;
            }
            self.stack.pop();
            hash = node_hash(&left, &hash);
            height += 1;
        }
        self.stack.push((height, hash));
        self.leaves += 1;
    }

    pub fn leaves(&self) -> u64 {
        self.leaves
    }

    pub fn finish(self) -> Result<Hash, MerkleError> {
        let mut rest = self.stack.into_iter().rev();
        let (_, mut acc) = rest.next().ok_or(MerkleError::Empty)?;
        for (_, left) in rest {
            acc = node_hash(&left, &acc);
        }
        Ok(acc)
    }
}

/// The largest power of two strictly below `n` (`n >= 2`).
fn split(n: u64) -> u64 {
    1u64 << (63 - (n - 1).leading_zeros())
}

/// The root over leaf hashes held in memory.
pub fn root(hashes: &[Hash]) -> Result<Hash, MerkleError> {
    match hashes.len() {
        0 => Err(MerkleError::Empty),
        1 => Ok(hashes[0]),
        n => {
            let k = split(n as u64) as usize;
            Ok(node_hash(&root(&hashes[..k])?, &root(&hashes[k..])?))
        }
    }
}

/// The path from leaf `index` up to the root, lowest level first.
pub fn path(hashes: &[Hash], index: u64) -> Result<Vec<Hash>, MerkleError> {
    let leaves = hashes.len() as u64;
    if leaves == 0 {
        return Err(MerkleError::Empty);
    }
    if index >= leaves {
        return Err(MerkleError::OutOfRange { index, leaves });
    }
    let mut out = Vec::new();
    path_into(hashes, index as usize, &mut out)?;
    Ok(out)
}

fn path_into(hashes: &[Hash], index: usize, out: &mut Vec<Hash>) -> Result<(), MerkleError> {
    if hashes.len() == 1 {
        return Ok(());
    }
    let k = split(hashes.len() as u64) as usize;
    if index < k {
        path_into(&hashes[..k], index, out)?;
        out.push(root(&hashes[k..])?);
    } else {
        path_into(&hashes[k..], index - k, out)?;
        out.push(root(&hashes[..k])?);
    }
    Ok(())
}

/// Does `path` lead from leaf hash `leaf` at `index` to `root`, in a tree of `leaves` leaves?
/// The path's length is fixed by `index` and `leaves`, so a path can't be read two ways.
pub fn verify(root: &Hash, leaves: u64, index: u64, leaf: &Hash, path: &[Hash]) -> bool {
    if index >= leaves {
        return false;
    }
    // RFC 9162 §2.1.3.2.
    let (mut fnode, mut snode) = (index, leaves - 1);
    let mut acc = *leaf;
    for p in path {
        if snode == 0 {
            return false;
        }
        if fnode & 1 == 1 || fnode == snode {
            acc = node_hash(p, &acc);
            while fnode & 1 == 0 && fnode != 0 {
                fnode >>= 1;
                snode >>= 1;
            }
        } else {
            acc = node_hash(&acc, p);
        }
        fnode >>= 1;
        snode >>= 1;
    }
    snode == 0 && acc == *root
}

/// The hashes that lead from the run of `count` leaves starting at `first` up to the root: every
/// subtree beside the run, in the order [`verify_run`] meets them (the tree walked from the
/// root, left before right). A run of one is that leaf's [`path`] in another order.
pub fn run_path(hashes: &[Hash], first: u64, count: u64) -> Result<Vec<Hash>, MerkleError> {
    let leaves = hashes.len() as u64;
    if leaves == 0 || count == 0 {
        return Err(MerkleError::Empty);
    }
    let end = first.checked_add(count).filter(|&e| e <= leaves).ok_or(MerkleError::OutOfRange { index: first.saturating_add(count - 1), leaves })?;
    let mut out = Vec::new();
    run_path_into(hashes, first as usize, end as usize, &mut out)?;
    Ok(out)
}

/// `hashes` is a subtree; `[from, to)` is the part of the run inside it (not empty).
fn run_path_into(hashes: &[Hash], from: usize, to: usize, out: &mut Vec<Hash>) -> Result<(), MerkleError> {
    if hashes.len() == 1 {
        return Ok(());
    }
    let k = split(hashes.len() as u64) as usize;
    if to <= k {
        run_path_into(&hashes[..k], from, to, out)?;
        out.push(root(&hashes[k..])?);
    } else if from >= k {
        out.push(root(&hashes[..k])?);
        run_path_into(&hashes[k..], from - k, to - k, out)?;
    } else {
        run_path_into(&hashes[..k], from, k, out)?;
        run_path_into(&hashes[k..], 0, to - k, out)?;
    }
    Ok(())
}

/// Do the leaf hashes `run`, in a row from index `first`, with `path` lead to `root` in a tree
/// of `leaves` leaves? As with [`verify`], the tree's shape fixes how many hashes the path
/// holds and where each goes: one too many or too few is refused.
pub fn verify_run(root: &Hash, leaves: u64, first: u64, run: &[Hash], path: &[Hash]) -> bool {
    if run.is_empty() || first.checked_add(run.len() as u64).is_none_or(|end| end > leaves) {
        return false;
    }
    let mut path = path.iter();
    let top = run_root(leaves, first, run, &mut path);
    path.next().is_none() && top.as_ref() == Some(root)
}

/// The root of a subtree of `n` leaves that holds all of `run` from its leaf `first` on.
fn run_root(n: u64, first: u64, run: &[Hash], path: &mut std::slice::Iter<Hash>) -> Option<Hash> {
    if n == 1 {
        return Some(run[0]);
    }
    let k = split(n);
    let end = first + run.len() as u64;
    Some(if end <= k {
        let left = run_root(k, first, run, path)?;
        node_hash(&left, path.next()?)
    } else if first >= k {
        let left = path.next()?;
        node_hash(left, &run_root(n - k, first - k, run, path)?)
    } else {
        let (a, b) = run.split_at((k - first) as usize);
        let left = run_root(k, first, a, path)?;
        node_hash(&left, &run_root(n - k, 0, b, path)?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hashes(n: usize) -> Vec<Hash> {
        (0..n).map(|i| leaf_hash(&(i as u64).to_le_bytes())).collect()
    }

    #[test]
    fn rfc_6962_known_roots() {
        // RFC 6962 test vectors (certificate-transparency-go, merkle tree test data).
        let leaves: [&[u8]; 3] = [&[], &[0x00], &[0x10]];
        let want = [
            "6e340b9cffb37a989ca544e6bb780a2c78901d3fb33738768511a30617afa01d",
            "fac54203e7cc696cf0dfcb42c92a1d9dbaf70ad9e621f4bd8d98662f00e3c125",
            "aeb6bcfe274b70a14fb067a5e5578264db0fa9b51af5e0ba159158f329e06e77",
        ];
        let mut hs = Vec::new();
        for (leaf, want) in leaves.iter().zip(want) {
            hs.push(leaf_hash(leaf));
            assert_eq!(hex::encode(root(&hs).unwrap()), want);
        }
    }

    #[test]
    fn the_builder_and_the_slice_agree_for_every_size() {
        for n in 1..=130 {
            let hs = hashes(n);
            let mut b = RootBuilder::new();
            for h in &hs {
                b.push_hash(*h);
            }
            assert_eq!(b.leaves(), n as u64);
            assert_eq!(b.finish().unwrap(), root(&hs).unwrap(), "{n} leaves");
        }
        assert_eq!(RootBuilder::new().finish(), Err(MerkleError::Empty));
        assert_eq!(root(&[]), Err(MerkleError::Empty));
    }

    #[test]
    fn every_path_verifies_and_nothing_else_does() {
        for n in [1usize, 2, 3, 4, 5, 7, 8, 9, 33, 100] {
            let hs = hashes(n);
            let r = root(&hs).unwrap();
            for i in 0..n {
                let p = path(&hs, i as u64).unwrap();
                assert!(verify(&r, n as u64, i as u64, &hs[i], &p), "{n} leaves, leaf {i}");
                // Wrong place, wrong leaf, wrong size, a longer or shorter path: all refused.
                if n > 1 {
                    let j = (i + 1) % n;
                    assert!(!verify(&r, n as u64, j as u64, &hs[i], &p), "{n}/{i} at place {j}");
                    assert!(!verify(&r, n as u64, i as u64, &hs[j], &p));
                    assert!(!verify(&r, n as u64, i as u64, &hs[i], &p[..p.len() - 1]));
                }
                let mut longer = p.clone();
                longer.push([0u8; 32]);
                assert!(!verify(&r, n as u64, i as u64, &hs[i], &longer));
            }
            assert!(!verify(&r, n as u64, n as u64, &hs[0], &[]));
            assert_eq!(path(&hs, n as u64), Err(MerkleError::OutOfRange { index: n as u64, leaves: n as u64 }));
        }
    }

    #[test]
    fn every_run_verifies_and_nothing_else_does() {
        for n in [1usize, 2, 3, 4, 5, 7, 8, 9, 13, 33] {
            let hs = hashes(n);
            let r = root(&hs).unwrap();
            for first in 0..n {
                for count in 1..=n - first {
                    let run = &hs[first..first + count];
                    let p = run_path(&hs, first as u64, count as u64).unwrap();
                    let ok = |first: usize, run: &[Hash], p: &[Hash]| verify_run(&r, n as u64, first as u64, run, p);
                    assert!(ok(first, run, &p), "{n} leaves, {count} from {first}");
                    // The whole tree needs no path; one leaf needs as many hashes as its path.
                    if count == n {
                        assert!(p.is_empty());
                    }
                    if count == 1 {
                        assert_eq!(p.len(), path(&hs, first as u64).unwrap().len());
                    }
                    // Moved, cut short, a leaf changed, a path too long or too short: refused.
                    if first + count < n {
                        assert!(!ok(first + 1, run, &p), "{n}/{first}+{count} moved up");
                    }
                    if first > 0 {
                        assert!(!ok(first - 1, run, &p), "{n}/{first}+{count} moved down");
                    }
                    if count > 1 {
                        assert!(!ok(first, &run[..count - 1], &p));
                        assert!(!ok(first, &run[1..], &p));
                    }
                    let mut wrong = run.to_vec();
                    wrong[count / 2] = leaf_hash(b"another");
                    assert!(!ok(first, &wrong, &p));
                    let mut longer = p.clone();
                    longer.push([0u8; 32]);
                    assert!(!ok(first, run, &longer));
                    if !p.is_empty() {
                        assert!(!ok(first, run, &p[..p.len() - 1]));
                        let mut bent = p.clone();
                        bent[0][0] ^= 1;
                        assert!(!ok(first, run, &bent));
                    }
                }
            }
            assert!(!verify_run(&r, n as u64, 0, &[], &[]));
            assert!(!verify_run(&r, n as u64, n as u64, &hs[..1], &[]));
            assert!(!verify_run(&r, n as u64, u64::MAX, &hs[..1], &[]));
            assert!(run_path(&hs, 0, 0).is_err());
            assert!(run_path(&hs, 0, n as u64 + 1).is_err());
            assert!(run_path(&hs, u64::MAX, 2).is_err());
        }
    }

    #[test]
    fn a_leaf_can_not_pass_as_a_node() {
        // Two leaves' root must differ from one leaf holding their two hashes.
        let hs = hashes(2);
        let mut joined = Vec::new();
        joined.extend_from_slice(&hs[0]);
        joined.extend_from_slice(&hs[1]);
        assert_ne!(root(&hs).unwrap(), leaf_hash(&joined));
    }
}
