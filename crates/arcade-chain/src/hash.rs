use sha2::{Digest, Sha256};
use std::fmt;

/// A 32-byte hash in internal (wire) byte order.
/// Displayed reversed, as block explorers and RPC show it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Hash256(pub [u8; 32]);

impl Hash256 {
    pub const ZERO: Hash256 = Hash256([0; 32]);

    /// Parse from display (reversed) hex.
    pub fn from_hex(s: &str) -> Result<Self, hex::FromHexError> {
        let mut b = [0u8; 32];
        hex::decode_to_slice(s, &mut b)?;
        b.reverse();
        Ok(Hash256(b))
    }

    pub fn to_hex(&self) -> String {
        let mut b = self.0;
        b.reverse();
        hex::encode(b)
    }

    /// First 12 bytes in wire order: the store's txid key prefix (T12).
    pub fn t12(&self) -> [u8; 12] {
        let mut k = [0u8; 12];
        k.copy_from_slice(&self.0[..12]);
        k
    }
}

impl fmt::Debug for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash256({})", self.to_hex())
    }
}

impl fmt::Display for Hash256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

pub fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// RIPEMD-160 of SHA-256: the hash inside P2PKH / P2SH addresses.
pub fn hash160(data: &[u8]) -> [u8; 20] {
    use ripemd::Digest as _;
    ripemd::Ripemd160::digest(sha256(data)).into()
}

/// Double SHA-256, the hash used for txids, block ids and merkle trees.
pub fn sha256d(data: &[u8]) -> Hash256 {
    Hash256(sha256(&sha256(data)))
}

/// Merkle root as Bitcoin-family chains compute it (last node duplicated on odd levels).
pub fn merkle_root(leaves: &[Hash256]) -> Hash256 {
    if leaves.is_empty() {
        return Hash256::ZERO;
    }
    let mut level: Vec<Hash256> = leaves.to_vec();
    while level.len() > 1 {
        if level.len() % 2 == 1 {
            level.push(*level.last().unwrap());
        }
        level = level
            .chunks(2)
            .map(|p| {
                let mut buf = [0u8; 64];
                buf[..32].copy_from_slice(&p[0].0);
                buf[32..].copy_from_slice(&p[1].0);
                sha256d(&buf)
            })
            .collect();
    }
    level[0]
}

/// Walk a merkle branch from `leaf` at position `index` up to the root.
pub fn merkle_branch_root(leaf: Hash256, branch: &[Hash256], mut index: u32) -> Hash256 {
    let mut h = leaf;
    for sib in branch {
        let mut buf = [0u8; 64];
        if index & 1 == 1 {
            buf[..32].copy_from_slice(&sib.0);
            buf[32..].copy_from_slice(&h.0);
        } else {
            buf[..32].copy_from_slice(&h.0);
            buf[32..].copy_from_slice(&sib.0);
        }
        h = sha256d(&buf);
        index >>= 1;
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_roundtrip_is_reversed() {
        let h = Hash256::from_hex(
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f",
        )
        .unwrap();
        assert_eq!(h.0[31], 0x00);
        assert_eq!(h.0[0], 0x6f);
        assert_eq!(
            h.to_hex(),
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
        );
    }

    #[test]
    fn merkle_single_leaf_is_leaf() {
        let a = sha256d(b"a");
        assert_eq!(merkle_root(&[a]), a);
    }

    #[test]
    fn merkle_branch_matches_tree() {
        let leaves: Vec<_> = (0u8..5).map(|i| sha256d(&[i])).collect();
        let root = merkle_root(&leaves);
        // Branch for leaf 2 in a 5-leaf tree (padded to 6, then 4, then 2).
        let l = &leaves;
        let pair = |a: Hash256, b: Hash256| {
            let mut buf = [0u8; 64];
            buf[..32].copy_from_slice(&a.0);
            buf[32..].copy_from_slice(&b.0);
            sha256d(&buf)
        };
        let n01 = pair(l[0], l[1]);
        let n44 = pair(l[4], l[4]);
        let n4444 = pair(n44, n44);
        let branch = [l[3], n01, n4444];
        assert_eq!(merkle_branch_root(l[2], &branch, 2), root);
    }
}
