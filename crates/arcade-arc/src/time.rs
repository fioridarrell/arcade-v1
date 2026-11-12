//! Proofs of time as the chain's rules see them (docs/ARCADECHAIN.md §6.1, §6.2).
//!
//! A proof of time shows that a calculation of a set number of steps, one after another, was
//! run from a 32-byte challenge. The rules need only one thing from it: a yes or no to
//! "does this proof show `steps` steps from this challenge?", with one possible output for
//! each challenge and step count.
//!
//! Two calculations run on the chain:
//!
//! - **Each block's**: `steps(score, D)` steps from the id of the block before. Every
//!   candidate for a height starts from the same challenge, so one run serves them all.
//! - **Each epoch's draw seed**: from the draw seed before and the draws of the writers of
//!   the epoch's first 80 blocks, for as many steps as blocks 66 to 80 of that epoch took
//!   together. Its output gives the next epoch's draw seed.

use crate::block::TimeProof;
use crate::merkle::Hash;
use crate::params::Net;
use crate::tagged;
use crate::vrf::Output;
use arcade_chain::hash::Hash256;
use sha2::{Digest, Sha256};

/// The check every arcade runs on a proof of time.
pub trait TimeProofs {
    /// Does `proof` show `steps` steps from `challenge`?
    fn verify(&self, challenge: &Hash, steps: u64, proof: &TimeProof) -> bool;

    /// Whether a proof takes the time it stands for. Only [`StandIn`] says no.
    fn takes_time(&self) -> bool {
        true
    }
}

/// Where the proof of time of the block after `prev` starts.
pub fn block_challenge(net: &Net, prev: &Hash256) -> Hash {
    tagged::hash(net, "time", &[&prev.0])
}

/// Where the proof of time behind an epoch's draw seed starts: the draw seed of the epoch
/// before, and the draws of the writers of that epoch's first 80 blocks, in block order.
pub fn draw_seed_challenge(net: &Net, before: &Hash, draws: &[Output]) -> Hash {
    let mut parts: Vec<&[u8]> = Vec::with_capacity(draws.len() + 1);
    parts.push(before);
    parts.extend(draws.iter().map(|d| &d[..]));
    tagged::hash(net, "draw seed time", &parts)
}

/// An epoch's draw seed, from the output of its proof of time.
pub fn draw_seed(net: &Net, output: &[u8]) -> Hash {
    tagged::hash(net, "draw seed", &[output])
}

/// **Not a proof of time.** It stands where the real one will (a calculation in a class
/// group, as Chia's) so the rest of the chain can be built and tested: its "proof" is one
/// hash, made in no time at all, so on a chain that uses it anyone can write any block at
/// once. No public network runs on it.
#[derive(Debug, Clone, Copy, Default)]
pub struct StandIn;

impl StandIn {
    pub fn prove(&self, challenge: &Hash, steps: u64) -> TimeProof {
        let mut h = Sha256::new();
        h.update(b"ARCADE stand-in for a proof of time\0");
        h.update(challenge);
        h.update(steps.to_le_bytes());
        TimeProof { output: h.finalize().to_vec(), proof: Vec::new() }
    }
}

impl TimeProofs for StandIn {
    fn verify(&self, challenge: &Hash, steps: u64, proof: &TimeProof) -> bool {
        *proof == self.prove(challenge, steps)
    }

    fn takes_time(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{MAINNET, TESTNET};

    #[test]
    fn a_proof_is_for_one_challenge_and_one_count() {
        let c = block_challenge(&TESTNET, &Hash256([1; 32]));
        let p = StandIn.prove(&c, 1_000);
        assert!(StandIn.verify(&c, 1_000, &p));
        assert!(!StandIn.verify(&c, 999, &p));
        assert!(!StandIn.verify(&block_challenge(&TESTNET, &Hash256([2; 32])), 1_000, &p));
        assert!(!StandIn.verify(&block_challenge(&MAINNET, &Hash256([1; 32])), 1_000, &p));
        // Nothing may ride along in the proof.
        let mut padded = p.clone();
        padded.proof.push(0);
        assert!(!StandIn.verify(&c, 1_000, &padded));
        assert!(!StandIn.takes_time());
    }

    #[test]
    fn the_draw_seed_follows_every_draw_in_order() {
        let net = &TESTNET;
        let (a, b) = ([1u8; 64], [2u8; 64]);
        let c = draw_seed_challenge(net, &[9; 32], &[a, b]);
        assert_ne!(c, draw_seed_challenge(net, &[9; 32], &[b, a]));
        assert_ne!(c, draw_seed_challenge(net, &[9; 32], &[a]));
        assert_ne!(c, draw_seed_challenge(net, &[8; 32], &[a, b]));
        assert_ne!(c, draw_seed_challenge(&MAINNET, &[9; 32], &[a, b]));
        assert_ne!(draw_seed(net, b"x"), draw_seed(net, b"y"));
        assert_ne!(draw_seed(net, b"x"), draw_seed(&MAINNET, b"x"));
    }
}
