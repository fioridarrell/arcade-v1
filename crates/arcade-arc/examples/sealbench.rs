//! Measures what docs/ARCADECHAIN.md §3.4 estimates: the cost of sealing and unsealing.
//! Run: cargo run --release -p arcade-arc --example sealbench

use arcade_arc::sloth::{seal_leaf, unseal_leaf, DATA_LEAF};
use std::time::Instant;

fn main() {
    let (owner, seg) = ([7u8; 32], [3u8; 32]);
    let n = 2_000u64;
    let leaves: Vec<[u8; DATA_LEAF]> = (0..n).map(|i| [(i % 251) as u8 ^ 0x5a; DATA_LEAF]).collect();

    let t = Instant::now();
    let sealed: Vec<_> = leaves.iter().enumerate().map(|(i, d)| seal_leaf(&owner, &seg, i as u64, d)).collect();
    let seal_s = t.elapsed().as_secs_f64() / n as f64;

    let t = Instant::now();
    for _ in 0..20 {
        for (i, s) in sealed.iter().enumerate() {
            assert_eq!(&unseal_leaf(&owner, &seg, i as u64, s).unwrap(), &leaves[i]);
        }
    }
    let unseal_s = t.elapsed().as_secs_f64() / (20 * n) as f64;

    let per_gib = (1u64 << 30) as f64 / DATA_LEAF as f64;
    println!("seal:   {:.1} us per leaf  -> {:.2} core-hours per GiB", seal_s * 1e6, seal_s * per_gib / 3600.0);
    println!("unseal: {:.2} us per leaf  -> {:.1} s per GiB on one core", unseal_s * 1e6, unseal_s * per_gib);
    println!("ratio:  {:.0}x", seal_s / unseal_s);
}
