//! The `H(...)` of docs/ARCADECHAIN.md: SHA-256 with the network and a purpose mixed in, so a
//! hash or signature made for one use, or for the testnet, means nothing anywhere else.
//!
//! `H(label; parts)` = `SHA-256(net tag ‖ "/" ‖ label ‖ 0x00 ‖ parts joined)`. Every part has a
//! fixed length for its label, or is given with its length in front, so two different lists
//! of parts can't join into the same bytes.

use crate::merkle::Hash;
use crate::params::Net;
use sha2::{Digest, Sha256};

fn prefix(net: &Net, label: &str) -> Vec<u8> {
    debug_assert!(!label.as_bytes().contains(&0));
    let mut m = Vec::with_capacity(net.tag.len() + label.len() + 2);
    m.extend_from_slice(net.tag);
    m.push(b'/');
    m.extend_from_slice(label.as_bytes());
    m.push(0);
    m
}

pub fn hash(net: &Net, label: &str, parts: &[&[u8]]) -> Hash {
    let mut h = Sha256::new();
    h.update(prefix(net, label));
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// The bytes an owner key signs for `label`.
pub fn message(net: &Net, label: &str, parts: &[&[u8]]) -> Vec<u8> {
    let mut m = prefix(net, label);
    for p in parts {
        m.extend_from_slice(p);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{MAINNET, TESTNET};

    #[test]
    fn the_net_and_the_label_both_count() {
        let a = hash(&MAINNET, "replica", &[b"x"]);
        assert_ne!(a, hash(&TESTNET, "replica", &[b"x"]));
        assert_ne!(a, hash(&MAINNET, "segment", &[b"x"]));
        // A label can't borrow bytes from the parts.
        assert_ne!(hash(&MAINNET, "ab", &[b"c"]), hash(&MAINNET, "a", &[b"bc"]));
        // Parts are joined: callers keep them fixed-length.
        assert_eq!(hash(&MAINNET, "a", &[b"bc", b"d"]), hash(&MAINNET, "a", &[b"bcd"]));
        assert_eq!(message(&MAINNET, "block", &[b"id"]), b"ARCADE/arc/block\0id".to_vec());
    }
}
