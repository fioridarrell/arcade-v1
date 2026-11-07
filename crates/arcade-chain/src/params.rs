//! Per-chain parameters. A chain is data, not code.
//!
//! Sources (via the Arcade v1.0 session, 2026-11-07): Pepecoin Core chainparams.cpp
//! (master 4fb5a0cd93), the GX10's live testnet node, and the Python arcade's
//! config.py / rawchain.py.

use crate::hash::Hash256;
use crate::header::Header;
use crate::pow::{meets_target, pow_hash, target_from_bits, PowAlgo, U256};

#[derive(Clone, Debug)]
pub struct ChainParams {
    /// Short key, as the arcade names it ("test", "main", ...).
    pub key: &'static str,
    /// Store chain number (2 bytes in every key). Assigned once, never reused.
    pub chain_no: u16,
    pub name: &'static str,
    pub magic: [u8; 4],
    pub p2p_port: u16,
    pub genesis: &'static str,
    /// Raw genesis header hex (80 bytes).
    pub genesis_header: &'static str,
    pub pow: PowAlgo,
    pub pow_limit_bits: u32,
    /// A block at or below powLimit difficulty is always accepted (testnet rule).
    pub min_difficulty_blocks: bool,
    /// AuxPoW chain id to check in merge-mined headers; None = not checked (as the arcade does on testnet).
    pub auxpow_chain_id: Option<u32>,
    pub auxpow_strict_chain_id: bool,
    pub p2pkh_version: u8,
    pub p2sh_version: u8,
    pub wif_version: u8,
    pub bip44_coin_type: u32,
    pub protocol_version: i32,
    pub dns_seeds: &'static [&'static str],
    pub fixed_peers: &'static [&'static str],
    pub checkpoints: &'static [(u32, &'static str)],
    pub coinbase_maturity: u32,
}

impl ChainParams {
    pub fn genesis_hash(&self) -> Hash256 {
        Hash256::from_hex(self.genesis).expect("valid genesis hex")
    }

    pub fn genesis_header(&self) -> Header {
        let raw: [u8; 80] = hex::decode(self.genesis_header)
            .expect("hex")
            .try_into()
            .expect("80 bytes");
        Header::parse(&raw)
    }
}

/// The arcade's header-link rule (rawchain.py MAX_EASIER): a block's target may be at most
/// 4x its parent's; getting harder is never limited; on min-difficulty chains a block at or
/// under powLimit is always fine.
pub const MAX_EASIER: u64 = 4;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HeaderError {
    #[error("prev hash does not match parent")]
    BadLink,
    #[error("invalid bits")]
    BadBits,
    #[error("proof of work does not meet target")]
    BadPow,
    #[error("difficulty drops more than {MAX_EASIER}x from parent")]
    TooEasy,
    #[error("AuxPoW flag set but no AuxPoW data")]
    MissingAuxPow,
}

pub fn too_easy(params: &ChainParams, bits: u32, parent_bits: u32) -> bool {
    let (Some(t), Some(pt)) = (target_from_bits(bits), target_from_bits(parent_bits)) else {
        return true;
    };
    if params.min_difficulty_blocks {
        if let Some(limit) = target_from_bits(params.pow_limit_bits) {
            if t <= limit {
                return false;
            }
        }
    }
    // t > pt * 4  (saturating: if pt*4 overflows, nothing is too easy)
    let mut pt4 = pt;
    for _ in 0..2 {
        pt4 = match pt4.checked_add(pt4) {
            Some(x) => x,
            None => return false,
        };
    }
    t > pt4
}

/// Check a plain (non-AuxPoW) header against its parent: link, difficulty step and PoW.
pub fn check_header(params: &ChainParams, h: &Header, parent: &Header) -> Result<(), HeaderError> {
    if h.prev != parent.hash() {
        return Err(HeaderError::BadLink);
    }
    let t = target_from_bits(h.bits).ok_or(HeaderError::BadBits)?;
    if t == U256::ZERO {
        return Err(HeaderError::BadBits);
    }
    if too_easy(params, h.bits, parent.bits) {
        return Err(HeaderError::TooEasy);
    }
    if h.is_auxpow() {
        // Caller must check the AuxPoW (its parent header carries the work).
        return Ok(());
    }
    if !meets_target(&pow_hash(params.pow, &h.encode()), h.bits) {
        return Err(HeaderError::BadPow);
    }
    Ok(())
}

pub static PEPECOIN_TEST: ChainParams = ChainParams {
    key: "test",
    chain_no: 1,
    name: "Pepecoin testnet",
    magic: [0xfe, 0xc1, 0xdb, 0xcc],
    p2p_port: 44874,
    genesis: "f9f4ea4ae7f6ea4c55040ede2019ba0a53e262f46ec9bce3dcda2cb11f96fc52",
    genesis_header: "0100000000000000000000000000000000000000000000000000000000000000000000001265bca4002feac94c0c06971f12aa8b2c82fb3e93244690d5cb399aa51b2ad2dc1daf65f0ff0f1e940d0000",
    pow: PowAlgo::Scrypt,
    pow_limit_bits: 0x1e0fffff,
    min_difficulty_blocks: true,
    auxpow_chain_id: None,
    auxpow_strict_chain_id: false,
    p2pkh_version: 0x71,
    p2sh_version: 0xc4,
    wif_version: 0xf1,
    bip44_coin_type: 1,
    protocol_version: 70016,
    dns_seeds: &["seeds-testnet.pepecoin.org", "seeds-testnet.pepeblocks.com"],
    fixed_peers: &[
        "3.216.226.159:44874",
        "34.198.62.248:44874",
        "135.181.168.83:44874",
        "207.244.232.43:44874",
        "147.124.204.26:44874",
    ],
    checkpoints: &[
        (0, "f9f4ea4ae7f6ea4c55040ede2019ba0a53e262f46ec9bce3dcda2cb11f96fc52"),
        (42001, "1b111a1eb8cb3e35d0aa6b89a06245574a78ce65996e5f2932f0df17027a5cd1"),
    ],
    coinbase_maturity: 240,
};

pub static PEPECOIN_MAIN: ChainParams = ChainParams {
    key: "main",
    chain_no: 2,
    name: "Pepecoin",
    magic: [0xc0, 0xa0, 0xf0, 0xe0],
    p2p_port: 33874,
    genesis: "37981c0c48b8d48965376c8a42ece9a0838daadb93ff975cb091f57f8c2a5faa",
    genesis_header: "", // to fetch when mainnet is added
    pow: PowAlgo::Scrypt,
    pow_limit_bits: 0x1e0fffff,
    min_difficulty_blocks: false,
    auxpow_chain_id: Some(0x3f),
    auxpow_strict_chain_id: false,
    p2pkh_version: 56,
    p2sh_version: 22,
    wif_version: 0x9e,
    bip44_coin_type: 3,
    protocol_version: 70016,
    dns_seeds: &[],
    fixed_peers: &[],
    checkpoints: &[],
    coinbase_maturity: 240,
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pow::bits_from_target;

    #[test]
    fn pepecoin_testnet_genesis() {
        let p = &PEPECOIN_TEST;
        let g = p.genesis_header();
        assert_eq!(g.hash(), p.genesis_hash());
        assert_eq!(
            g.merkle_root.to_hex(),
            "d22a1ba59a39cbd5904624933efb822c8baa121f97060c4cc9ea2f00a4bc6512"
        );
        assert_eq!((g.time, g.nonce, g.bits, g.version), (1705975260, 3476, 0x1e0ffff0, 1));
        assert!(meets_target(&pow_hash(PowAlgo::Scrypt, &g.encode()), g.bits));
    }

    #[test]
    fn max_easier_rule() {
        let p = &PEPECOIN_MAIN; // no min-difficulty exemption
        let parent = 0x1c0fffff;
        let t = target_from_bits(parent).unwrap();
        let x4 = bits_from_target(t.checked_add(t).unwrap().checked_add(t).unwrap().checked_add(t).unwrap());
        assert!(!too_easy(p, x4, parent), "exactly 4x easier is allowed");
        assert!(too_easy(p, 0x1d0fffff, parent), "256x easier is not");
        assert!(!too_easy(p, 0x1b0fffff, parent), "harder is always fine");
        // Testnet: powLimit blocks are always fine.
        assert!(!too_easy(&PEPECOIN_TEST, 0x1e0fffff, 0x1b0fffff));
    }

    #[test]
    fn check_header_rejects_bad_link() {
        let g = PEPECOIN_TEST.genesis_header();
        let mut h = g;
        h.prev = Hash256::ZERO;
        assert_eq!(check_header(&PEPECOIN_TEST, &h, &g), Err(HeaderError::BadLink));
    }
}
