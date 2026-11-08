//! Protocol indexes live in chain.mdbx and are written in the same commit as each block
//! (arcade_ledger::index). Here: bring any index that is behind the chain up to date by
//! replaying stored blocks (first start, or an index added to an already-synced chain).

use crate::node::Node;

pub const INDEXES: &[&str] = &["ledger.v1", "profiles.v1"];

/// Backfill lagging indexes a batch at a time. Returns blocks processed.
pub fn catch_up(node: &Node) -> Result<u32, String> {
    let mut n = 0;
    let running = node.chain.indexers_for(node.params.chain_no);
    for name in INDEXES.iter().filter(|i| running.contains(i)) {
        loop {
            let done = node.chain.backfill(node.params, name, 2000).map_err(|e| e.to_string())?;
            n += done;
            if done == 0 {
                break;
            }
        }
    }
    Ok(n)
}
