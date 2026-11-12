//! The numbers every arcade must agree on (docs/ARCADECHAIN.md §5–§9). Changing one is a rule
//! upgrade. Values the document marks *(proposed)* are still Martin's to change before genesis.

/// 1 ARCADE in its smallest unit (8 decimal places).
pub const COIN: u64 = 100_000_000;
/// Minted by every block, forever.
pub const BLOCK_REWARD: u64 = 50 * COIN;
/// Target seconds between blocks.
pub const BLOCK_SECONDS: u64 = 150;
/// Blocks in an epoch: one pot, one draw seed, one difficulty.
pub const EPOCH_BLOCKS: u64 = 100;
/// The writers of this many blocks at the start of an epoch feed the next draw seed.
pub const DRAW_SEED_BLOCKS: u64 = 80;
/// The draw seed's proof of time is as long as this many recent blocks took.
pub const DRAW_SEED_TIME_BLOCKS: u64 = 15;
/// Difficulty changes by at most this factor per epoch.
pub const RETARGET_CLAMP: u64 = 4;
/// A block's time may be this far ahead of the node's clock.
pub const MAX_FUTURE_SECONDS: u64 = 120;

/// Blocks in an audit period (2 days) while the storage section has room (§9).
pub const AUDIT_PERIOD_BLOCKS: u64 = 1_152;
/// A challenge takes its seed from the block this many after the block holding the commit.
pub const CHALLENGE_DELAY: u64 = 10;
/// Blocks an arcade has to post an audit commit, an opening or a registration proof.
pub const ANSWER_WINDOW: u64 = 20;
/// Leaves opened by an audit.
pub const AUDIT_OPENINGS: usize = 4;
/// Leaves proved by a registration.
pub const REGISTRATION_LEAVES: usize = 16;
/// Audits in a row a replica may fail before it is removed.
pub const MAX_MISSED_AUDITS: u32 = 3;
/// Blocks after a first passed audit before a replica counts for the writer draw.
pub const DRAW_WEIGHT_WAIT: u64 = 200;
/// An arcade never leaves its chain for one that splits off further back than this.
pub const REORG_LIMIT: u64 = 100;
/// Blocks before the exchange treats a payment as final.
pub const FINAL_AFTER: u64 = 60;
/// Blocks before a coinbase output can be spent.
pub const COINBASE_MATURITY: u64 = 100;

/// Most bytes of coin transactions in a block.
pub const MAX_TX_BYTES: usize = 1_000_000;
/// Most bytes of storage items in a block.
pub const MAX_STORAGE_BYTES: usize = 1_000_000;
/// Per-block limits by item kind (§9).
pub const MAX_REGISTRATION_PROOFS: usize = 8;
pub const MAX_VERDICTS: usize = 400;
pub const MAX_REGISTRATION_COMMITS: usize = 200;
pub const MAX_CATALOG_ITEMS: usize = 64;
/// Most bytes of leaves a reveal demand may name.
pub const MAX_REVEAL_BYTES: usize = 64 * 1024;
/// Blocks a reveal demand or a transaction challenge waits for its answer.
pub const REVEAL_WINDOW: u64 = 100;

/// Raw bytes in a full segment.
pub const SEGMENT_BYTES: u64 = 1 << 30;
/// Heights in one range of a source chain.
pub const RANGE: u64 = 10_000;

/// Fee floor and dust, per kB and per output.
pub const MIN_FEE_PER_KB: u64 = COIN / 1_000;
pub const DUST: u64 = COIN / 1_000;
/// Released amounts under this wait in an owed balance.
pub const MIN_PAYOUT: u64 = COIN / 100;

/// One of the two ARCADE networks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Net {
    /// V1.0's key and net name.
    pub key: &'static str,
    /// Store chain number.
    pub chain_no: u16,
    pub p2pkh_version: u8,
    pub p2sh_version: u8,
    pub wif_version: u8,
    /// Mixed into every hash and signature, so nothing made for one network is valid on the other.
    pub tag: &'static [u8],
    /// Block 0's time (seconds since 1970, UTC). `None` until the testnet's date is set.
    pub genesis_time: Option<u64>,
    /// The listed chains (§2.3), by V1.0's keys: only their segments can be in the catalog.
    pub listed: &'static [&'static str],
}

pub const MAINNET: Net = Net {
    key: "arc",
    chain_no: 1000,
    p2pkh_version: 0x17,
    p2sh_version: 0x1c,
    wif_version: 0x97,
    tag: b"ARCADE/arc",
    // 2027-01-01 00:00:00 UTC.
    genesis_time: Some(1_798_761_600),
    listed: &["arc", "pep", "peptest", "doge", "dgb", "flop", "wjk", "kot", "ltc", "btc"],
};

pub const TESTNET: Net = Net {
    key: "arctest",
    chain_no: 1001,
    p2pkh_version: 0x53,
    p2sh_version: 0x57,
    wif_version: 0xd3,
    tag: b"ARCADE/arctest",
    genesis_time: None,
    // The main network's, and the other test chains: small, so cheap to try things on.
    listed: &["arctest", "pep", "peptest", "doge", "dgb", "dgbtest", "flop", "wjk", "kot", "ltc", "ltctest", "btc"],
};

/// The first height of the epoch `height` is in.
pub fn epoch_start(height: u64) -> u64 {
    height - height % EPOCH_BLOCKS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_numbers_fit_together() {
        assert_eq!(BLOCK_REWARD * EPOCH_BLOCKS, 5_000 * COIN);
        assert_eq!(AUDIT_PERIOD_BLOCKS * BLOCK_SECONDS, 2 * 86_400);
        // Nothing registered on a secret branch can count before the branch is too old (§6.3).
        assert!(REORG_LIMIT < DRAW_WEIGHT_WAIT);
        // The draw seed's proof of time ends before the epoch it is for begins (§6.1).
        assert!(DRAW_SEED_BLOCKS + DRAW_SEED_TIME_BLOCKS < EPOCH_BLOCKS);
        assert_eq!(epoch_start(0), 0);
        assert_eq!(epoch_start(199), 100);
        assert_eq!(epoch_start(200), 200);
    }

    #[test]
    fn genesis_is_new_years_day_2027() {
        // Days from 1970-01-01 to 2027-01-01: 57 years, 14 of them leap years (1972..=2024).
        let days = 57 * 365 + 14;
        assert_eq!(MAINNET.genesis_time, Some(days * 86_400));
    }
}
