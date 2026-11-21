//! `chain.mdbx`: each chain stored once, exactly as on the wire, with its indexes
//! (txid, coins, spends, address coins and history, undo) updated in the same commit
//! as the block. See docs/PLAN.md §4-5.

pub mod addrbuild;
pub mod coreimport;
pub mod copyout;
pub mod db;
pub mod indexer;
pub mod initsync;
pub mod journal;
pub mod rules;
pub mod proven;
pub mod schema;
pub mod split;
pub mod stake;
pub mod sync;
pub mod txruns;
pub mod undo;

pub use db::{ChainDb, Coin, HeaderRec, Tip};
pub use proven::MoneyDepth;
pub use indexer::{BlockCtx, Indexer, Prevout};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod coreimport_tests;
#[cfg(test)]
mod stake_tests;
#[cfg(test)]
mod initsync_tests;
