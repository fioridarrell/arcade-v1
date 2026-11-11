//! Kotia proof of stake (Blackcoin kernel v3), ported from Kotia Core v2.4
//! (eleonoraandrea/kotiacash, src/pos.cpp, pow.cpp, main.cpp). Every rule here decides whether
//! the network takes a staked block, so each mirrors the C++ exactly, overflow included.
//!
//! - Stake modifier: mod(h) = sha256d(kernel(h) | mod(h-1)), genesis 0. kernel = the
//!   coinstake's first input txid on a PoS block, the block hash (GetHash) on a PoW block.
//! - Kernel: sha256d(mod(prev) | txPrev.nTime u32 | prevout txid | prevout n u32 | nTimeTx u32)
//!   <= target(nBits) * value (256-bit, wrapping as arith_uint256 does).
//! - Block time = coinstake nTime, a multiple of 16 (mask 0xf), > the previous block's time,
//!   <= now + 15 s. The staked coin needs 20 confirmations.
//! - Next PoS target: exponential moving retarget over the last two PoS blocks, spacing
//!   120 s, timespan 600 s, actual spacing capped at 10x, never easier than posLimit.
//! - Reward: at most 20 KOT + fees, paid in the coinstake. The block signature signs the
//!   block hash with the key in coinstake output 1 (P2PK, or OP_RETURN <pubkey>).

use crate::hash::sha256d;
use crate::pow::{bits_from_target, target_from_bits, U256};
use crate::Hash256;

pub const STAKE_TIMESTAMP_MASK: u32 = 0xf;
pub const TARGET_SPACING: i64 = 120;
pub const TARGET_TIMESPAN: i64 = 600;
pub const COINBASE_MATURITY: u32 = 20;
pub const FUTURE_DRIFT: u32 = 15;
/// 20 KOT per staked block (GetProofOfStakeSubsidy).
pub const STAKE_SUBSIDY: u64 = 20 * 100_000_000;
/// posLimit / posLimitV2: 00000fff…ff.
pub fn pos_limit() -> U256 {
    let mut be = [0xffu8; 32];
    be[0] = 0;
    be[1] = 0;
    be[2] = 0x0f;
    U256::from_be_bytes(&be)
}

/// The modifier after a block: sha256d(kernel | previous modifier).
pub fn next_modifier(prev: &Hash256, kernel: &Hash256) -> Hash256 {
    let mut b = [0u8; 64];
    b[..32].copy_from_slice(&kernel.0);
    b[32..].copy_from_slice(&prev.0);
    sha256d(&b)
}

/// The kernel hash of staking `prevout` (from a transaction with time `tx_prev_time`) at
/// `time`, given the previous block's modifier.
pub fn kernel_hash(modifier: &Hash256, tx_prev_time: u32, prevout_txid: &Hash256, prevout_n: u32, time: u32) -> Hash256 {
    let mut b = Vec::with_capacity(76);
    b.extend_from_slice(&modifier.0);
    b.extend_from_slice(&tx_prev_time.to_le_bytes());
    b.extend_from_slice(&prevout_txid.0);
    b.extend_from_slice(&prevout_n.to_le_bytes());
    b.extend_from_slice(&time.to_le_bytes());
    sha256d(&b)
}

/// arith_uint256 *= u64: modulo 2^256.
fn wrapping_mul_u64(a: U256, m: u64) -> U256 {
    let mut r = [0u64; 4];
    let mut carry: u128 = 0;
    for i in 0..4 {
        let v = a.0[i] as u128 * m as u128 + carry;
        r[i] = v as u64;
        carry = v >> 64;
    }
    U256(r)
}

/// Does staking `value` meet `bits` with this kernel hash? (CheckStakeKernelHash, minus the
/// time and maturity checks done by the caller.)
pub fn kernel_meets(hash: &Hash256, bits: u32, value: u64) -> bool {
    if value == 0 {
        return false;
    }
    // SetCompact as arith_uint256 does: a negative or overflowing compact gives what C++ gives;
    // live bits are always plain, so anything else is refused here.
    let Some(t) = target_from_bits(bits) else { return false };
    U256::from_le_bytes(&hash.0) <= wrapping_mul_u64(t, value)
}

/// Full kernel check: time rules and the hash.
#[allow(clippy::too_many_arguments)]
pub fn check_kernel(modifier: &Hash256, bits: u32, tx_prev_time: u32, prevout_txid: &Hash256, prevout_n: u32, value: u64, time: u32) -> bool {
    time >= tx_prev_time && time & STAKE_TIMESTAMP_MASK == 0 && kernel_meets(&kernel_hash(modifier, tx_prev_time, prevout_txid, prevout_n, time), bits, value)
}

/// nBits for the next PoS block, from the last PoS block (bits, time) and the PoS block before
/// it (time). (CalculateNextTargetRequired, protocol V2/V3 as on the live chain.)
pub fn next_bits(last_bits: u32, last_time: u32, prev_time: u32) -> u32 {
    let mut actual = last_time as i64 - prev_time as i64;
    if actual < 0 {
        actual = TARGET_SPACING;
    }
    if actual > TARGET_SPACING * 10 {
        actual = TARGET_SPACING * 10;
    }
    let limit = pos_limit();
    let Some(t) = target_from_bits(last_bits) else { return bits_from_target(limit) };
    let interval = TARGET_TIMESPAN / TARGET_SPACING;
    let num = ((interval - 1) * TARGET_SPACING + actual + actual) as u64;
    let den = ((interval + 1) * TARGET_SPACING) as u64;
    let n = wrapping_mul_u64(t, num).div_u64(den);
    if n.is_zero() || n > limit { bits_from_target(limit) } else { bits_from_target(n) }
}

/// The pubkey a staked block is signed with: coinstake output 1 as P2PK, or OP_RETURN <pubkey>.
pub fn block_signing_key(out1_script: &[u8]) -> Option<&[u8]> {
    let s = out1_script;
    let push = |at: usize| -> Option<&[u8]> {
        let n = *s.get(at)? as usize;
        (n == 33 || n == 65).then(|| s.get(at + 1..at + 1 + n)).flatten()
    };
    // P2PK: <push pubkey> OP_CHECKSIG
    if let Some(pk) = push(0) {
        if s.len() == pk.len() + 2 && s[s.len() - 1] == 0xac {
            return Some(pk);
        }
    }
    // OP_RETURN <push pubkey> (anything after is ignored by GetOp-based parsing)
    if s.first() == Some(&0x6a) {
        return push(1);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pos_limit_compact() {
        assert_eq!(bits_from_target(pos_limit()), 0x1e0fffff);
    }

    #[test]
    fn retarget_on_time_keeps_bits() {
        // On-target spacing: num == den, the target stays (up to compact rounding).
        assert_eq!(next_bits(0x1a08da09, 1_000_120, 1_000_000), 0x1a08da09);
        // Slower blocks: easier (bigger target); faster: harder.
        assert!(target_from_bits(next_bits(0x1a08da09, 1_000_600, 1_000_000)).unwrap() > target_from_bits(0x1a08da09).unwrap());
        assert!(target_from_bits(next_bits(0x1a08da09, 1_000_016, 1_000_000)).unwrap() < target_from_bits(0x1a08da09).unwrap());
    }

    #[test]
    fn signing_key_forms() {
        let pk = [2u8; 33];
        let mut p2pk = vec![33];
        p2pk.extend_from_slice(&pk);
        p2pk.push(0xac);
        assert_eq!(block_signing_key(&p2pk), Some(&pk[..]));
        let mut ret = vec![0x6a, 33];
        ret.extend_from_slice(&pk);
        assert_eq!(block_signing_key(&ret), Some(&pk[..]));
        assert_eq!(block_signing_key(&[0x76, 0xa9]), None);
    }

    #[test]
    fn wrapping_matches_mod_2_256() {
        let a = U256([0, 0, 0, 1 << 63]);
        assert_eq!(wrapping_mul_u64(a, 2), U256::ZERO);
    }
}
