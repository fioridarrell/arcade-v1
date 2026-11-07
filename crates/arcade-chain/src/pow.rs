//! Proof of work: compact targets, work, and the scrypt / sha256d hash functions.

use crate::hash::{sha256d, Hash256};

/// A proof-of-work hash function.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PowAlgo {
    Scrypt,
    Sha256d,
    // DigiByte multi-algo. Not yet implemented: headers mined with these are accepted
    // on the heaviest-chain rule without a PoW check (as the existing arcade does for DGB).
    Groestl,
    Skein,
    Qubit,
    Odocrypt,
}

impl PowAlgo {
    pub fn checkable(self) -> bool {
        matches!(self, PowAlgo::Scrypt | PowAlgo::Sha256d | PowAlgo::Skein)
    }
}

/// How a chain picks the algorithm for a header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowRule {
    /// One algorithm for every block (Pepecoin, Dogecoin, Litecoin...).
    Single(PowAlgo),
    /// DigiByte: algorithm in the version's BLOCK_VERSION_ALGO bits (0x0F00).
    DigiByte,
    /// Proof of work up to and including `last_pow_height`, proof of stake after (Kotia).
    PowThenStake { algo: PowAlgo, last_pow_height: u32 },
}

/// DigiByte BLOCK_VERSION_ALGO = 15 << 8 (primitives/block.h).
pub const DGB_ALGO_MASK: i32 = 0x0F00;

impl PowRule {
    /// The work algorithm for a header: `None` = unknown algorithm (reject);
    /// `Some(None)` = no proof of work to check (proof of stake).
    pub fn algo(self, version: i32, height: u32) -> Option<Option<PowAlgo>> {
        match self {
            PowRule::Single(a) => Some(Some(a)),
            PowRule::PowThenStake { algo, last_pow_height } => {
                Some((height <= last_pow_height).then_some(algo))
            }
            PowRule::DigiByte => match version & DGB_ALGO_MASK {
                0x0000 => Some(Some(PowAlgo::Scrypt)),
                0x0200 => Some(Some(PowAlgo::Sha256d)),
                0x0400 => Some(Some(PowAlgo::Groestl)),
                0x0600 => Some(Some(PowAlgo::Skein)),
                0x0800 => Some(Some(PowAlgo::Qubit)),
                0x0E00 => Some(Some(PowAlgo::Odocrypt)),
                _ => None,
            },
        }
    }
}

/// The PoW hash of an 80-byte header, or None if the algorithm is not implemented yet.
pub fn try_pow_hash(algo: PowAlgo, header80: &[u8; 80]) -> Option<Hash256> {
    match algo {
        PowAlgo::Sha256d => Some(sha256d(header80)),
        PowAlgo::Scrypt => {
            // Litecoin/Dogecoin scrypt: N=1024, r=1, p=1, password = salt = header.
            let params = scrypt::Params::new(10, 1, 1).expect("valid scrypt params");
            let mut out = [0u8; 32];
            scrypt::scrypt(header80, header80, &params, &mut out).expect("32-byte output");
            Some(Hash256(out))
        }
        PowAlgo::Skein => {
            // DigiByte HashSkein: SHA-256 of Skein-512-512.
            use skein::digest::Digest as _;
            let h512 = skein::Skein512::<skein::consts::U64>::digest(header80);
            Some(Hash256(crate::hash::sha256(&h512)))
        }
        _ => None,
    }
}

/// The PoW hash for an implemented algorithm (panics otherwise).
pub fn pow_hash(algo: PowAlgo, header80: &[u8; 80]) -> Hash256 {
    try_pow_hash(algo, header80).expect("PoW algorithm not implemented")
}

/// 256-bit unsigned integer as 4 little-endian u64 limbs; just enough for targets and work.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct U256(pub [u64; 4]);

impl U256 {
    pub const ZERO: U256 = U256([0; 4]);
    pub const MAX: U256 = U256([u64::MAX; 4]);

    pub fn from_le_bytes(b: &[u8; 32]) -> U256 {
        let mut l = [0u64; 4];
        for (i, limb) in l.iter_mut().enumerate() {
            *limb = u64::from_le_bytes(b[i * 8..i * 8 + 8].try_into().unwrap());
        }
        U256(l)
    }

    pub fn to_be_bytes(&self) -> [u8; 32] {
        let mut b = [0u8; 32];
        for i in 0..4 {
            b[(3 - i) * 8..(4 - i) * 8].copy_from_slice(&self.0[i].to_be_bytes());
        }
        b
    }

    pub fn from_be_bytes(b: &[u8; 32]) -> U256 {
        let mut l = [0u64; 4];
        for i in 0..4 {
            l[i] = u64::from_be_bytes(b[(3 - i) * 8..(4 - i) * 8].try_into().unwrap());
        }
        U256(l)
    }

    pub fn is_zero(&self) -> bool {
        self.0 == [0; 4]
    }

    fn shl(self, s: u32) -> U256 {
        if s >= 256 {
            return U256::ZERO;
        }
        let (w, b) = ((s / 64) as usize, s % 64);
        let mut r = [0u64; 4];
        for i in (w..4).rev() {
            r[i] = self.0[i - w] << b;
            if b > 0 && i > w {
                r[i] |= self.0[i - w - 1] >> (64 - b);
            }
        }
        U256(r)
    }

    pub fn checked_add(self, o: U256) -> Option<U256> {
        let mut r = [0u64; 4];
        let mut carry = false;
        for i in 0..4 {
            let (a, c1) = self.0[i].overflowing_add(o.0[i]);
            let (a, c2) = a.overflowing_add(carry as u64);
            r[i] = a;
            carry = c1 || c2;
        }
        (!carry).then_some(U256(r))
    }

    fn sub(self, o: U256) -> U256 {
        let mut r = [0u64; 4];
        let mut borrow = false;
        for i in 0..4 {
            let (a, b1) = self.0[i].overflowing_sub(o.0[i]);
            let (a, b2) = a.overflowing_sub(borrow as u64);
            r[i] = a;
            borrow = b1 || b2;
        }
        U256(r)
    }

    fn bits(&self) -> u32 {
        for i in (0..4).rev() {
            if self.0[i] != 0 {
                return 64 * i as u32 + 64 - self.0[i].leading_zeros();
            }
        }
        0
    }

    /// Integer division (shift-subtract; fine for per-header work).
    pub fn div(self, d: U256) -> U256 {
        assert!(!d.is_zero(), "division by zero");
        let mut q = U256::ZERO;
        let mut r = self;
        if r < d {
            return q;
        }
        let shift = r.bits() - d.bits();
        let mut dd = d.shl(shift);
        for s in (0..=shift).rev() {
            if r >= dd {
                r = r.sub(dd);
                q.0[(s / 64) as usize] |= 1 << (s % 64);
            }
            dd = U256(dd.0).shr1();
        }
        q
    }

    /// self * m (None on overflow).
    pub fn mul_u64(self, m: u64) -> Option<U256> {
        let mut r = [0u64; 4];
        let mut carry: u128 = 0;
        for i in 0..4 {
            let v = self.0[i] as u128 * m as u128 + carry;
            r[i] = v as u64;
            carry = v >> 64;
        }
        (carry == 0).then_some(U256(r))
    }

    /// self / d (truncating).
    pub fn div_u64(self, d: u64) -> U256 {
        let mut r = [0u64; 4];
        let mut rem: u128 = 0;
        for i in (0..4).rev() {
            let cur = (rem << 64) | self.0[i] as u128;
            r[i] = (cur / d as u128) as u64;
            rem = cur % d as u128;
        }
        U256(r)
    }

    fn shr1(self) -> U256 {
        let mut r = [0u64; 4];
        for i in 0..4 {
            r[i] = self.0[i] >> 1;
            if i < 3 {
                r[i] |= self.0[i + 1] << 63;
            }
        }
        U256(r)
    }
}

impl PartialOrd for U256 {
    fn partial_cmp(&self, o: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for U256 {
    fn cmp(&self, o: &Self) -> std::cmp::Ordering {
        for i in (0..4).rev() {
            match self.0[i].cmp(&o.0[i]) {
                std::cmp::Ordering::Equal => continue,
                x => return x,
            }
        }
        std::cmp::Ordering::Equal
    }
}

/// Decode compact `bits` into a target. None if negative or overflowing.
pub fn target_from_bits(bits: u32) -> Option<U256> {
    let exp = bits >> 24;
    let mant = bits & 0x007f_ffff;
    if bits & 0x0080_0000 != 0 && mant != 0 {
        return None;
    }
    let t = if exp <= 3 {
        U256([(mant >> (8 * (3 - exp))) as u64, 0, 0, 0])
    } else {
        let t = U256([mant as u64, 0, 0, 0]);
        let s = 8 * (exp - 3);
        if mant != 0 && t.bits() + s > 256 {
            return None;
        }
        t.shl(s)
    };
    Some(t)
}

/// Encode a target into compact bits (Bitcoin's GetCompact).
pub fn bits_from_target(t: U256) -> u32 {
    let mut size = t.bits().div_ceil(8);
    let be = t.to_be_bytes();
    let mut mant: u32 = 0;
    for i in 0..3 {
        let idx = 32 - size as i64 + i as i64;
        mant <<= 8;
        if (0..32).contains(&idx) {
            mant |= be[idx as usize] as u32;
        }
    }
    if mant & 0x0080_0000 != 0 {
        mant >>= 8;
        size += 1;
    }
    (size << 24) | mant
}

/// Does `hash` meet the target in `bits`?
pub fn meets_target(hash: &Hash256, bits: u32) -> bool {
    match target_from_bits(bits) {
        Some(t) if !t.is_zero() => U256::from_le_bytes(&hash.0) <= t,
        _ => false,
    }
}

/// Work represented by a block at `bits`: 2^256 / (target + 1).
pub fn work_from_bits(bits: u32) -> U256 {
    let Some(t) = target_from_bits(bits) else { return U256::ZERO };
    if t.is_zero() {
        return U256::ZERO;
    }
    // (~t / (t+1)) + 1  ==  2^256 / (t+1)
    let not_t = U256([!t.0[0], !t.0[1], !t.0[2], !t.0[3]]);
    let t1 = t.checked_add(U256([1, 0, 0, 0])).unwrap_or(U256::MAX);
    not_t.div(t1).checked_add(U256([1, 0, 0, 0])).unwrap_or(U256::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn btc_genesis_meets_target() {
        let raw: [u8; 80] = hex::decode("0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c").unwrap().try_into().unwrap();
        let h = pow_hash(PowAlgo::Sha256d, &raw);
        assert!(meets_target(&h, 0x1d00ffff));
        assert!(!meets_target(&h, 0x1b00ffff));
    }

    #[test]
    fn compact_roundtrip() {
        for bits in [0x1d00ffffu32, 0x1e0ffff0, 0x1b0404cb, 0x207fffff] {
            assert_eq!(bits_from_target(target_from_bits(bits).unwrap()), bits);
        }
    }

    #[test]
    fn genesis_work_is_2_pow_32_ish() {
        // Bitcoin genesis (bits 0x1d00ffff) has work 0x100010001.
        assert_eq!(work_from_bits(0x1d00ffff), U256([0x1_0001_0001, 0, 0, 0]));
    }

    #[test]
    fn scrypt_is_deterministic_and_differs() {
        let a = pow_hash(PowAlgo::Scrypt, &[0u8; 80]);
        let b = pow_hash(PowAlgo::Scrypt, &[0u8; 80]);
        let c = pow_hash(PowAlgo::Scrypt, &[1u8; 80]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}

#[cfg(test)]
mod ltc_tests {
    use super::*;
    use crate::hash::Hash256;
    use crate::header::Header;

    /// Litecoin genesis: a real scrypt block, checks our scrypt PoW end to end.
    #[test]
    fn litecoin_genesis_scrypt_pow() {
        let h = Header {
            version: 1,
            prev: Hash256::ZERO,
            merkle_root: Hash256::from_hex(
                "97ddfbbae6be97fd6cdf3e7ca13232a3afff2353e29badfab7f73011edd4ced9",
            )
            .unwrap(),
            time: 1317972665,
            bits: 0x1e0ffff0,
            nonce: 2084524493,
        };
        assert_eq!(
            h.hash().to_hex(),
            "12a765e31ffd4059bada1e25190f6e98c99d9714d334efa41a195a7e7e04bfe2"
        );
        let pow = pow_hash(PowAlgo::Scrypt, &h.encode());
        assert!(meets_target(&pow, h.bits), "scrypt hash {pow} misses target");
        assert!(!meets_target(&pow_hash(PowAlgo::Sha256d, &h.encode()), h.bits));
    }
}
