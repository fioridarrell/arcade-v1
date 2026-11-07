//! AuxPoW (merge mining), in the Dogecoin/Namecoin layout.
//!
//! Serialized after the header when the header's version has VERSION_AUXPOW set:
//!   parent coinbase tx | parent block hash | coinbase merkle branch + index
//!   | chain merkle branch + index | parent header (80 bytes)
//!
//! NOTE: the exact Pepecoin rules (chain id, start height, strictness) are still
//! to be confirmed against the existing arcade's `rawchain.py`.

use crate::encode::{DecodeError, Reader};
use crate::hash::{merkle_branch_root, Hash256};
use crate::header::Header;
use crate::tx::Tx;

/// Magic marking the merged-mining commitment in the parent coinbase.
pub const MERGED_MINING_HEADER: [u8; 4] = [0xfa, 0xbe, b'm', b'm'];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuxPow {
    pub coinbase: Tx,
    pub parent_hash: Hash256,
    pub coinbase_branch: Vec<Hash256>,
    pub coinbase_index: u32,
    pub chain_branch: Vec<Hash256>,
    pub chain_index: u32,
    pub parent: Header,
    /// Exact serialized bytes.
    pub raw: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AuxPowError {
    #[error("coinbase branch index must be 0")]
    CoinbaseIndex,
    #[error("parent coinbase not in parent block's merkle root")]
    CoinbaseNotInParent,
    #[error("parent uses our own chain id")]
    SameChainId,
    #[error("chain merkle root not found in parent coinbase")]
    NoCommitment,
    #[error("more than one merged-mining header in coinbase")]
    MultipleHeaders,
    #[error("merged-mining header not just before the chain root")]
    HeaderNotBeforeRoot,
    #[error("chain merkle root too late in coinbase script")]
    RootTooLate,
    #[error("missing tree size / nonce after chain root")]
    MissingSizeNonce,
    #[error("tree size does not match chain branch")]
    TreeSize,
    #[error("wrong chain index for our chain id")]
    ChainIndex,
    #[error("chain branch too long")]
    BranchTooLong,
}

fn decode_branch(r: &mut Reader) -> Result<(Vec<Hash256>, u32), DecodeError> {
    let n = r.len()?;
    if n > 64 {
        return Err(DecodeError::Invalid("merkle branch too long"));
    }
    let mut v = Vec::with_capacity(n);
    for _ in 0..n {
        v.push(r.hash()?);
    }
    let idx = r.u32()?;
    Ok((v, idx))
}

impl AuxPow {
    pub fn decode(r: &mut Reader) -> Result<AuxPow, DecodeError> {
        let start = r.pos();
        let coinbase = Tx::decode(r)?;
        let parent_hash = r.hash()?;
        let (coinbase_branch, coinbase_index) = decode_branch(r)?;
        let (chain_branch, chain_index) = decode_branch(r)?;
        let parent = Header::decode(r)?;
        let raw = r.since(start).to_vec();
        Ok(AuxPow {
            coinbase,
            parent_hash,
            coinbase_branch,
            coinbase_index,
            chain_branch,
            chain_index,
            parent,
            raw,
        })
    }

    /// Check this AuxPoW commits to `aux_hash` (our block's hash) for `chain_id`.
    /// Proof of work itself is checked separately on `self.parent`.
    pub fn check(&self, aux_hash: Hash256, chain_id: u32, strict: bool) -> Result<(), AuxPowError> {
        if self.coinbase_index != 0 {
            return Err(AuxPowError::CoinbaseIndex);
        }
        if strict && self.parent.chain_id() == chain_id {
            return Err(AuxPowError::SameChainId);
        }
        if self.chain_branch.len() > 30 {
            return Err(AuxPowError::BranchTooLong);
        }
        let root =
            merkle_branch_root(self.coinbase.txid, &self.coinbase_branch, self.coinbase_index);
        if root != self.parent.merkle_root {
            return Err(AuxPowError::CoinbaseNotInParent);
        }

        // Chain merkle root, as it appears (big-endian / reversed) in the coinbase script.
        let chain_root = merkle_branch_root(aux_hash, &self.chain_branch, self.chain_index);
        let mut needle = chain_root.0;
        needle.reverse();

        let script = &self.coinbase.inputs.first().ok_or(AuxPowError::NoCommitment)?.script_sig;
        let pc = find(script, &needle).ok_or(AuxPowError::NoCommitment)?;
        match find(script, &MERGED_MINING_HEADER) {
            Some(ph) => {
                if find(&script[ph + 1..], &MERGED_MINING_HEADER).is_some() {
                    return Err(AuxPowError::MultipleHeaders);
                }
                if ph + MERGED_MINING_HEADER.len() != pc {
                    return Err(AuxPowError::HeaderNotBeforeRoot);
                }
            }
            None => {
                if pc > 20 {
                    return Err(AuxPowError::RootTooLate);
                }
            }
        }
        let after = pc + 32;
        if script.len() < after + 8 {
            return Err(AuxPowError::MissingSizeNonce);
        }
        let size = u32::from_le_bytes(script[after..after + 4].try_into().unwrap());
        let nonce = u32::from_le_bytes(script[after + 4..after + 8].try_into().unwrap());
        if size != 1u32 << self.chain_branch.len() {
            return Err(AuxPowError::TreeSize);
        }
        if self.chain_index != expected_index(nonce, chain_id, self.chain_branch.len()) {
            return Err(AuxPowError::ChainIndex);
        }
        Ok(())
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Namecoin's pseudo-random slot for a chain in the merged-mining tree.
pub fn expected_index(nonce: u32, chain_id: u32, h: usize) -> u32 {
    let mut rand = nonce;
    rand = rand.wrapping_mul(1103515245).wrapping_add(12345);
    rand = rand.wrapping_add(chain_id);
    rand = rand.wrapping_mul(1103515245).wrapping_add(12345);
    if h >= 32 {
        rand
    } else {
        rand % (1u32 << h)
    }
}
