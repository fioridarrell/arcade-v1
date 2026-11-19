//! This machine's disk budget (Martin 2026-11-19: "each machine should keep a dynamic amount"): what Chains & Coins
//! uses now and will grow to, what the ARCADE keep share holds, and what is free on the data volume. The keep share's
//! suggestion and its share-of-free-space presets come from it, so a fully borrowed arcade on small hardware keeps
//! nothing and isn't asked to, and a frontier one with many full chains and indexes is offered what's left after them.
//!
//! - **Chains & Coins now**: this arcade's own measurement of each chain it keeps (crate::sizes, every six hours).
//! - **Will grow to**: per kept chain, the size arcades report for its mode plus each index it keeps (medians), or what
//!   it uses now when that's more; chains go on growing, so 10% of that on top.
//! - **Reserve**: what the chains will still take (grow-to minus now). Suggestions never use it, nor the last 20 GB.
//! - **Suggested keep**: a quarter of the free space past the reserve and the 20 GB floor, in whole GB (none under 1 GB).
//!
//! A closed test net can give an arcade a made-up budget (`ARCADE_DISK_BUDGET_TEST` = {"free", "chains_now",
//! "chains_expected"} in bytes), to show and test tiny, medium and large machines on one computer.

use crate::node::Node;
use serde_json::{json, Value};

/// The free space suggestions always leave: 20 GB.
pub const FREE_FLOOR: u64 = 20 << 30;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Budget {
    /// Free bytes on the data volume.
    pub free: u64,
    /// Bytes Chains & Coins uses now, and will grow to (with headroom).
    pub chains_now: u64,
    pub chains_expected: u64,
    /// Chains kept here (not borrowed), and those borrowed.
    pub kept_chains: usize,
    pub borrowed_chains: usize,
}

impl Budget {
    /// What the chains will still take of the free space.
    pub fn reserve(&self) -> u64 {
        self.chains_expected.saturating_sub(self.chains_now)
    }

    /// Free space past the reserve and the floor: what a keep share may use.
    pub fn spare(&self) -> u64 {
        self.free.saturating_sub(self.reserve()).saturating_sub(FREE_FLOOR)
    }

    /// The keep share suggested for this machine: a quarter of the spare space, whole GB, none under 1 GB.
    pub fn suggested(&self) -> u64 {
        let q = self.spare() / 4;
        if q < 1 << 30 { 0 } else { q - q % (1 << 30) }
    }
}

fn test_budget() -> Option<Value> {
    if !crate::arcsvc::local() {
        return None;
    }
    std::env::var("ARCADE_DISK_BUDGET_TEST").ok().and_then(|v| serde_json::from_str(&v).ok())
}

/// A closed test net's stand-in for "Earn ARCADE is on for a coin" (`"earn": true` in the test budget), so the keep
/// prompt can be tried without syncing a real coin.
pub fn test_earn() -> Option<bool> {
    test_budget().and_then(|t| t["earn"].as_bool())
}

/// Free bytes on the data volume (the nearest existing folder of the blob store's), or the test budget's.
pub fn free_bytes() -> u64 {
    if let Some(t) = test_budget().and_then(|t| t["free"].as_u64()) {
        return t;
    }
    let d = crate::arcblob::dir();
    d.ancestors().find(|p| p.exists()).map_or(0, crate::diskfree::free_bytes)
}

/// The machine's budget now.
pub fn budget(node: &Node) -> Budget {
    let mut b = Budget { free: free_bytes(), ..Default::default() };
    let mine = node.my_sizes.lock().unwrap().clone();
    let arc = crate::arcsvc::defined().map(|p| p.key);
    for p in crate::chains::all() {
        if !crate::chains::kept(node, p) {
            continue;
        }
        // The ARCADE chain runs on every arcade: its bytes count, but it isn't one of the chains chosen.
        let own = Some(p.key) == arc;
        if crate::borrow::is_borrowed(p.key) && !own {
            b.borrowed_chains += 1;
            continue;
        }
        b.kept_chains += !own as usize;
        let now: u64 = mine.get(p.net).and_then(|v| v["groups"].as_object().map(|g| g.values().filter_map(|x| x.as_u64()).sum())).unwrap_or(0);
        let sizes = crate::sizes::for_chain(node, p.key);
        let mode = node.mode_of(p.key).unwrap_or_else(|| "full".into());
        let base = sizes["modes"][mode.as_str()].as_u64().unwrap_or(0);
        let ix: u64 = crate::chains::indexes_of(node, p)
            .iter()
            .filter_map(|i| {
                let x = &sizes["indexes"][i.as_str()];
                if mode == "shared" { x["shared_bytes"].as_u64().or(x["bytes"].as_u64()) } else { x["bytes"].as_u64() }
            })
            .sum();
        b.chains_now += now;
        let grow = (base + ix).max(now);
        b.chains_expected += grow + grow / 10;
    }
    if let Some(t) = test_budget() {
        b.chains_now = t["chains_now"].as_u64().unwrap_or(b.chains_now);
        b.chains_expected = t["chains_expected"].as_u64().unwrap_or(b.chains_expected);
    }
    b
}

/// The budget as `arc.keep` gives it (`keep_bytes`: what the keep share holds now).
pub fn json(b: &Budget, keep_bytes: u64, keep_target: u64) -> Value {
    json!({"free_bytes": b.free, "chains_bytes": b.chains_now, "chains_expected_bytes": b.chains_expected, "reserve_bytes": b.reserve(),
           "spare_bytes": b.spare(), "suggested_bytes": b.suggested(), "suggested_mb": b.suggested() >> 20, "free_floor_bytes": FREE_FLOOR,
           "kept_chains": b.kept_chains, "borrowed_chains": b.borrowed_chains, "keep_bytes": keep_bytes, "keep_target_bytes": keep_target})
}

#[cfg(test)]
mod tests {
    use super::*;
    const GB: u64 = 1 << 30;

    #[test]
    fn suggestions_follow_the_machine() {
        // Tiny, every chain borrowed: 8 GB free, nothing kept: nothing suggested.
        let tiny = Budget { free: 8 * GB, ..Default::default() };
        assert_eq!(tiny.suggested(), 0);
        // Medium: 120 GB free, 30 GB of chains that will grow to 60: (120 - 30 - 20) / 4 = 17.5 -> 17 GB.
        let medium = Budget { free: 120 * GB, chains_now: 30 * GB, chains_expected: 60 * GB, kept_chains: 2, ..Default::default() };
        assert_eq!(medium.reserve(), 30 * GB);
        assert_eq!(medium.suggested(), 17 * GB);
        // Frontier: 4 TB free, many full chains and indexes growing by 1.5 TB: what's left after them.
        let large = Budget { free: 4096 * GB, chains_now: 900 * GB, chains_expected: 2400 * GB, kept_chains: 9, ..Default::default() };
        assert_eq!(large.suggested(), (4096 - 1500 - 20) / 4 * GB);
        // Chains that will take all the free space leave nothing to suggest.
        let full = Budget { free: 200 * GB, chains_now: 100 * GB, chains_expected: 400 * GB, kept_chains: 5, ..Default::default() };
        assert_eq!(full.suggested(), 0);
    }
}
