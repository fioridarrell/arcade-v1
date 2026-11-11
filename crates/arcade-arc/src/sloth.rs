//! Sealing a leaf (docs/ARCADECHAIN.md §3.3): one modular square root to seal, one squaring to
//! unseal. Each leaf stands alone, so the only way to get a sealed leaf without storing it is
//! to take the square root again.
//!
//! The prime is `p = 2^1024 - 105`, the largest prime below 2^1024 with `p ≡ 3 (mod 4)`.
//! "Sloth" (Lenstra & Wesolowski 2015) makes the square root a permutation of `[0, p)`:
//! a square maps to its even root; a non-square `x` maps to the odd root of `-x`.

use num_bigint::BigUint;
use num_traits::{One, Zero};
use sha2::{Digest, Sha512};
use std::sync::OnceLock;

/// Bytes of raw data in one leaf.
pub const DATA_LEAF: usize = 127;
/// Bytes of one sealed leaf.
pub const SEALED_LEAF: usize = 128;
/// `p = 2^1024 - P_SUB`.
pub const P_SUB: u32 = 105;

const MASK_TAG: &[u8] = b"ARCADE/seal/v1";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SealError {
    #[error("sealed leaf is not below the prime")]
    OutOfRange,
    #[error("sealed leaf doesn't unseal to a data leaf")]
    NotData,
}

pub struct Field {
    pub p: BigUint,
    /// `(p + 1) / 4`: raising to it takes a square root.
    root_exp: BigUint,
}

pub fn field() -> &'static Field {
    static F: OnceLock<Field> = OnceLock::new();
    F.get_or_init(|| {
        let p = (BigUint::one() << 1024usize) - BigUint::from(P_SUB);
        let root_exp = (&p + BigUint::one()) >> 2usize;
        Field { p, root_exp }
    })
}

fn is_odd(x: &BigUint) -> bool {
    x.bit(0)
}

/// The slow direction: about 1,023 modular squarings.
pub fn seal_value(x: &BigUint) -> BigUint {
    let f = field();
    debug_assert!(x < &f.p);
    let y = x.modpow(&f.root_exp, &f.p);
    let square = (&y * &y) % &f.p == *x;
    // A square takes its even root; a non-square takes the odd root of its negative.
    if y.is_zero() || is_odd(&y) != square {
        y
    } else {
        &f.p - y
    }
}

/// The fast direction: one modular squaring.
pub fn unseal_value(y: &BigUint) -> BigUint {
    let f = field();
    let sq = (y * y) % &f.p;
    if is_odd(y) && !sq.is_zero() {
        &f.p - sq
    } else {
        sq
    }
}

/// The per-leaf mask `H(owner key ‖ segment id ‖ i)`, as a number below `p`.
pub fn mask(owner: &[u8; 32], segment: &[u8; 32], index: u64) -> BigUint {
    let mut wide = [0u8; 128];
    for (ctr, half) in wide.chunks_mut(64).enumerate() {
        let mut h = Sha512::new();
        h.update(MASK_TAG);
        h.update(owner);
        h.update(segment);
        h.update(index.to_le_bytes());
        h.update([ctr as u8]);
        half.copy_from_slice(&h.finalize()[..]);
    }
    BigUint::from_bytes_be(&wide) % &field().p
}

fn to_sealed(v: &BigUint) -> [u8; SEALED_LEAF] {
    let raw = v.to_bytes_be();
    let mut out = [0u8; SEALED_LEAF];
    out[SEALED_LEAF - raw.len()..].copy_from_slice(&raw);
    out
}

/// Seal one data leaf under an owner key.
pub fn seal_leaf(owner: &[u8; 32], segment: &[u8; 32], index: u64, data: &[u8; DATA_LEAF]) -> [u8; SEALED_LEAF] {
    let f = field();
    let x = (BigUint::from_bytes_be(data) + mask(owner, segment, index)) % &f.p;
    to_sealed(&seal_value(&x))
}

/// Unseal one leaf. Fails when the bytes aren't a sealed data leaf for this owner and place.
pub fn unseal_leaf(
    owner: &[u8; 32],
    segment: &[u8; 32],
    index: u64,
    sealed: &[u8; SEALED_LEAF],
) -> Result<[u8; DATA_LEAF], SealError> {
    let f = field();
    let y = BigUint::from_bytes_be(sealed);
    if y >= f.p {
        return Err(SealError::OutOfRange);
    }
    let x = unseal_value(&y);
    let d = (x + &f.p - mask(owner, segment, index)) % &f.p;
    if d.bits() > (DATA_LEAF * 8) as u64 {
        return Err(SealError::NotData);
    }
    let raw = d.to_bytes_be();
    let mut out = [0u8; DATA_LEAF];
    out[DATA_LEAF - raw.len()..].copy_from_slice(&raw);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rnd(seed: u64, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        let mut ctr = 0u64;
        while out.len() < n {
            let mut h = Sha512::new();
            h.update(seed.to_le_bytes());
            h.update(ctr.to_le_bytes());
            out.extend_from_slice(&h.finalize()[..]);
            ctr += 1;
        }
        out.truncate(n);
        out
    }

    #[test]
    fn the_prime_is_prime_and_3_mod_4() {
        let p = &field().p;
        assert_eq!(p % BigUint::from(4u32), BigUint::from(3u32));
        assert_eq!(p.bits(), 1024);
        // Miller-Rabin with fixed bases.
        let one = BigUint::one();
        let pm1 = p - &one;
        let s = pm1.trailing_zeros().unwrap();
        let d = &pm1 >> s;
        for a in [2u32, 3, 5, 7, 11, 13, 17, 19, 23, 29, 31, 37] {
            let mut x = BigUint::from(a).modpow(&d, p);
            if x == one || x == pm1 {
                continue;
            }
            let mut ok = false;
            for _ in 1..s {
                x = (&x * &x) % p;
                if x == pm1 {
                    ok = true;
                    break;
                }
            }
            assert!(ok, "base {a} says composite");
        }
    }

    #[test]
    fn seal_and_unseal_are_inverse_permutations() {
        let p = &field().p;
        let mut edge = vec![BigUint::zero(), BigUint::one(), BigUint::from(2u32), p - 1u32, p - 2u32];
        for i in 0..40 {
            edge.push(BigUint::from_bytes_be(&rnd(i, 128)) % p);
        }
        let (mut even, mut odd) = (0, 0);
        for x in &edge {
            let y = seal_value(x);
            assert!(&y < p);
            assert_eq!(&unseal_value(&y), x, "unseal(seal(x)) = x");
            assert_eq!(&seal_value(&unseal_value(x)), x, "seal(unseal(y)) = y");
            if is_odd(&y) { odd += 1 } else { even += 1 }
        }
        assert!(even > 5 && odd > 5, "both squares and non-squares were covered");
    }

    #[test]
    fn leaves_round_trip_and_depend_on_owner_segment_and_place() {
        let (o1, o2) = ([1u8; 32], [2u8; 32]);
        let seg = [9u8; 32];
        for i in 0..20u64 {
            let mut d = [0u8; DATA_LEAF];
            d.copy_from_slice(&rnd(100 + i, DATA_LEAF));
            let s = seal_leaf(&o1, &seg, i, &d);
            assert_eq!(unseal_leaf(&o1, &seg, i, &s).unwrap(), d);
            assert_ne!(s, seal_leaf(&o2, &seg, i, &d), "another owner's copy differs");
            assert_ne!(s, seal_leaf(&o1, &[8u8; 32], i, &d), "another segment differs");
            assert_ne!(s, seal_leaf(&o1, &seg, i + 1, &d), "another place differs");
            // Under the wrong owner it unseals to something else, or to nothing.
            assert_ne!(unseal_leaf(&o2, &seg, i, &s).ok(), Some(d));
        }
        for d in [[0u8; DATA_LEAF], [0xffu8; DATA_LEAF]] {
            let s = seal_leaf(&o1, &seg, 7, &d);
            assert_eq!(unseal_leaf(&o1, &seg, 7, &s).unwrap(), d);
        }
    }

    #[test]
    fn bytes_that_are_not_a_sealed_leaf_are_refused() {
        assert_eq!(unseal_leaf(&[1; 32], &[9; 32], 0, &[0xff; SEALED_LEAF]), Err(SealError::OutOfRange));
        // Random bytes below p almost never unseal to a 127-byte leaf (chance 1 in 256).
        let mut refused = 0;
        for i in 0..64u64 {
            let mut s = [0u8; SEALED_LEAF];
            s.copy_from_slice(&rnd(900 + i, SEALED_LEAF));
            s[0] &= 0x7f;
            if unseal_leaf(&[1; 32], &[9; 32], i, &s) == Err(SealError::NotData) {
                refused += 1;
            }
        }
        assert!(refused >= 60);
    }
}
