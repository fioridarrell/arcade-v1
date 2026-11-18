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
/// Replicas of each owner audited in an audit period, drawn by bytes (§5.2: sampled audits, Martin 2026-11-12).
pub const AUDIT_SAMPLE: usize = 4;
/// Leaves proved by a registration.
pub const REGISTRATION_LEAVES: usize = 16;
/// Audits in a row a replica may fail before it is removed.
pub const MAX_MISSED_AUDITS: u32 = 3;
/// Blocks after a first passed audit before a replica counts for the writer draw (below rules version 4; see
/// [`draw_weight_wait`]).
pub const DRAW_WEIGHT_WAIT: u64 = 200;
/// An arcade never leaves its chain for one that splits off further back than this.
pub const REORG_LIMIT: u64 = 100;
/// Blocks before the exchange treats a payment as final.
pub const FINAL_AFTER: u64 = 60;
/// Blocks before a coinbase output can be spent.
pub const COINBASE_MATURITY: u64 = 100;

/// Most bytes of coin transactions in a block.
pub const MAX_TX_BYTES: usize = 1_000_000;
/// Most bytes of storage items in a block (§14.19 4: 4 MB, so audits, registrations, verdicts and payouts of 100,000
/// arcades take about a quarter of it; was 1 MB).
pub const MAX_STORAGE_BYTES: usize = 4_000_000;
/// Per-block limits by item kind (§9).
pub const MAX_REGISTRATION_PROOFS: usize = 8;
pub const MAX_VERDICTS: usize = 1_000;
pub const MAX_REGISTRATION_COMMITS: usize = 200;
pub const MAX_CATALOG_ITEMS: usize = 64;
/// Most bytes of leaves a reveal demand may name.
pub const MAX_REVEAL_BYTES: usize = 64 * 1024;
/// Blocks a reveal demand or a transaction challenge waits for its answer.
pub const REVEAL_WINDOW: u64 = 100;

/// Raw bytes in a full segment (Martin 2026-11-12: 16 MiB, so an arcade keeping only a small chain earns from
/// most of it; was 1 GiB).
pub const SEGMENT_BYTES: u64 = 16 << 20;
/// Heights in one range of a source chain.
pub const RANGE: u64 = 10_000;

/// Fee floor and dust, per kB and per output.
pub const MIN_FEE_PER_KB: u64 = COIN / 1_000;
pub const DUST: u64 = COIN / 1_000;
/// Released amounts under this wait in an owed balance.
pub const MIN_PAYOUT: u64 = COIN / 100;

/// Serving checks (§5.4, §14.19 2): checks each owner gets per audit period, about, each on one of its replicas
/// drawn by bytes.
pub const SERVE_CHECKS: u64 = 6;
/// Blocks after its seed's block in which a verdict may be posted.
pub const VERDICT_WINDOW: u64 = 20;
/// An owner's pay is held back at its release with at least this many verdicts since its
/// last release, most of them "not served".
pub const HOLD_VERDICTS: u32 = 5;
/// Held back this many releases in a row, the held pay goes back to the pot.
pub const HOLD_FORFEIT: u32 = 3;
/// What a checker asks for: about this many bytes of plain blocks, answered within this time.
pub const SERVE_PIECE_BYTES: u64 = 8 << 20;
pub const SERVE_SECONDS: u64 = 60;

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
    /// The listed chains (§2.3), by V1.0's keys: only their segments can be in the catalog. Consensus reads them at a
    /// height, through [`Net::listed_at`].
    pub listed: &'static [&'static str],
    /// The chains listed from rules version 4 on ([`rules_v4`], ARCADE issue #6), on top of `listed`.
    pub listed_v4: &'static [&'static str],
    /// Target seconds between blocks, as the difficulty follows the clock: [`BLOCK_SECONDS`]. Only a local
    /// test chain (`--arc-local` settings, `block_seconds`) sets less, so its tests see many epochs and audit
    /// periods in minutes.
    pub block_seconds: u64,
    /// The rules versions and the heights they start at, in order (§14.21): a block carries the version its height
    /// calls for. A rule change ships in a release ahead of its height; an arcade without it stops at that height and
    /// says to update.
    pub rules: &'static [(u32, u64)],
    /// Blocks in an audit period: [`AUDIT_PERIOD_BLOCKS`]; only a local test chain (`audit_period`) sets fewer.
    pub audit_period: u64,
    /// Seconds after which a block may travel without the bytes of its blobs (§15 step 3: full nodes may drop old
    /// blobs' plain copies): [`BLOB_WINDOW`]; only a local test chain (`blob_window`) sets less.
    pub blob_window: u64,
}

/// 30 days.
pub const BLOB_WINDOW: u64 = 30 * 24 * 3600;

/// The newest rules version this arcade knows. Version 2 (time keepers, §6.2): a block's proof of time may come in
/// up to [`MAX_POT_RUNS`] runs one after another. Version 3 (blobs, §15 step 1): blob records and a block's blob section.
/// Version 4 (Martin 2026-11-16, ARCADE issues #6, #10, #11, #14, #20): see [`rules_v4`].
pub const RULES_KNOWN: u32 = 4;

/// Whether rules version 4 applies to the block at `height` (testnet only so far; the height is set in the release
/// that ships it, about a day ahead): Shibacoin, Bellscoin, Luckycoin and Junkcoin listed and the
/// dogefeed journal family paid (#6; collections stays unpaid: its index isn't built the same on every arcade); a repository push names the manifest it was based on (#10); draw weight from a
/// replica's first passed audit (#11); index catalog entries confirmed by a tenth of the draw weight (#14); the draw
/// seed's proof of time in up to [`MAX_POT_RUNS`] runs (#20).
pub fn rules_v4(net: &Net, height: u64) -> bool {
    net.rules_at(height) >= 4
}

/// The first height where [`rules_v4`] holds, if any (a network's rules versions rise with height).
pub fn rules_v4_from(net: &Net) -> Option<u64> {
    net.rules.iter().find(|(v, _)| *v >= 4).map(|(_, h)| *h)
}

/// The most runs a block's proof of time may come in from rules version 2 (a time keeper serving the 8 lowest
/// candidates of a height; the 8th candidate's proof is 8 runs).
pub const MAX_POT_RUNS: usize = 8;

/// The block writer's own share of each block's 50 from rules version 2 (Martin 2026-11-14: 1 to the writer, 49 to the
/// storage pot, fees still into the pot): a withheld or lost block costs its writer something.
pub const WRITER_REWARD: u64 = COIN;

/// What the writer of the block at `height` is paid in its own coinbase.
pub fn writer_reward(net: &Net, height: u64) -> u64 {
    if net.rules_at(height) >= 2 {
        WRITER_REWARD
    } else {
        0
    }
}

/// The most runs a block at `height` may carry in its proof of time.
pub fn pot_runs(net: &Net, height: u64) -> usize {
    if net.rules_at(height) >= 2 {
        MAX_POT_RUNS
    } else {
        1
    }
}

/// The most runs the proof of time behind the draw seed of the epoch the block at `height` starts may come in: up to
/// [`MAX_POT_RUNS`] from rules version 4 (#20: made in saved pieces, so a restart carries on instead of starting
/// over), one run before.
pub fn seed_runs(net: &Net, height: u64) -> usize {
    if rules_v4(net, height) {
        MAX_POT_RUNS
    } else {
        1
    }
}

/// Blocks after its first passed audit, in the block at `height`, before a replica counts for the writer draw: none
/// from rules version 4 (#11, testnet only: it counts from the next block), [`DRAW_WEIGHT_WAIT`] before. Mainnet's
/// schedule has no version 4, so mainnet keeps the 200-block wait and with it §6.3's guarantee (REORG_LIMIT <
/// DRAW_WEIGHT_WAIT: nothing registered on a secret branch counts for the draw before the branch is too old to take).
/// The testnet gives that guarantee up on purpose (Martin 2026-11-16).
pub fn draw_weight_wait(net: &Net, height: u64) -> u64 {
    if rules_v4(net, height) {
        0
    } else {
        DRAW_WEIGHT_WAIT
    }
}

/// The ARCADE mesh protocol this arcade speaks, and the oldest it takes ARCADE frames from (§14.21).
pub const PROTOCOL: u32 = 1;
pub const MIN_PROTOCOL: u32 = 1;

impl Net {
    /// The rules version a block at `height` carries.
    pub fn rules_at(&self, height: u64) -> u32 {
        self.rules.iter().rev().find(|(_, from)| *from <= height).map_or(1, |(v, _)| *v)
    }

    /// The listed chains (§2.3) for the block at `height`: `listed`, and `listed_v4` where [`rules_v4`] holds. A catalog
    /// entry is judged at its block's height; views ask for the next block's.
    pub fn listed_at(&self, height: u64) -> impl Iterator<Item = &'static str> + '_ {
        let more: &'static [&'static str] = if rules_v4(self, height) { self.listed_v4 } else { &[] };
        self.listed.iter().chain(more).copied()
    }

    /// Whether `chain` (V1.0's key) is listed for the block at `height`.
    pub fn is_listed_at(&self, chain: &str, height: u64) -> bool {
        self.listed_at(height).any(|c| c == chain)
    }
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
    // Mainnet's list is unchanged by rules version 4 (#6 lists Shibacoin, Bellscoin, Luckycoin and Junkcoin on the
    // testnet only, and mainnet has no version 4 height).
    listed_v4: &[],
    block_seconds: BLOCK_SECONDS,
    rules: &[(2, 0)],
    audit_period: AUDIT_PERIOD_BLOCKS,
    blob_window: BLOB_WINDOW,
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
    // Rules version 4 (#6): Shibacoin, Bellscoin, Luckycoin and Junkcoin, from its height.
    listed_v4: &["shic", "bel", "lky", "jkc"],
    block_seconds: BLOCK_SECONDS,
    // Restarted 2026-11-15 (Martin: "restart + carry ledger"): version 3 (blobs, the tenth with its challenge window, on
    // top of version 2's time keepers and writer's share) from block 0. The chain before ran 1 from 0 and 2 from 1,065.
    // Version 4 from block 1,000 (Martin 2026-11-16, ARCADE issues #6, #10, #11, #14, #20; released in v0.1.106).
    rules: &[(3, 0), (4, 1_000)],
    audit_period: AUDIT_PERIOD_BLOCKS,
    blob_window: BLOB_WINDOW,
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
        // Nothing registered on a secret branch can count before the branch is too old (§6.3). Mainnet has no rules
        // version 4, so this holds there at every height; the testnet drops the wait from version 4 (#11).
        assert!(REORG_LIMIT < DRAW_WEIGHT_WAIT);
        assert!((0..1_000_000).step_by(997).all(|h| REORG_LIMIT < draw_weight_wait(&MAINNET, h)));
        let v4 = Net { rules: &[(3, 0), (4, 500)], ..TESTNET };
        assert_eq!((draw_weight_wait(&v4, 499), draw_weight_wait(&v4, 500)), (DRAW_WEIGHT_WAIT, 0));
        // The draw seed's proof of time ends before the epoch it is for begins (§6.1).
        assert!(DRAW_SEED_BLOCKS + DRAW_SEED_TIME_BLOCKS < EPOCH_BLOCKS);
        assert_eq!(epoch_start(0), 0);
        assert_eq!(epoch_start(199), 100);
        assert_eq!(epoch_start(200), 200);
    }

    #[test]
    fn a_block_carries_the_rules_version_of_its_height() {
        let net = Net { rules: &[(1, 0), (2, 5_000), (3, 9_000)], ..TESTNET };
        assert_eq!((net.rules_at(0), net.rules_at(4_999), net.rules_at(5_000), net.rules_at(10_000)), (1, 1, 2, 3));
        assert_eq!(rules_v4_from(&net), None);
        let v4 = Net { rules: &[(3, 0), (4, 700)], ..TESTNET };
        assert_eq!(rules_v4_from(&v4), Some(700));
        assert!(!rules_v4(&v4, 699) && rules_v4(&v4, 700) && rules_v4(&v4, 10_000));
        // Every version scheduled on the real networks is one this arcade knows.
        for n in [MAINNET, TESTNET] {
            assert!(n.rules.iter().all(|(v, _)| *v <= RULES_KNOWN));
            assert!(n.rules.windows(2).all(|w| w[0].0 < w[1].0 && w[0].1 < w[1].1));
        }
    }

    #[test]
    fn shibacoin_bellscoin_luckycoin_and_junkcoin_are_listed_from_rules_version_4_on_the_testnet() {
        let four = ["shic", "bel", "lky", "jkc"];
        let v4 = Net { rules: &[(3, 0), (4, 700)], ..TESTNET };
        for c in four {
            assert!(!v4.is_listed_at(c, 0) && !v4.is_listed_at(c, 699), "{c} below the v4 height");
            assert!(v4.is_listed_at(c, 700) && v4.is_listed_at(c, 10_000), "{c} from the v4 height");
        }
        assert_eq!(v4.listed_at(699).collect::<Vec<_>>(), TESTNET.listed);
        assert_eq!(v4.listed_at(700).count(), TESTNET.listed.len() + 4);
        // The chains listed before stay listed at every height.
        assert!(TESTNET.listed.iter().all(|c| v4.is_listed_at(c, 0) && v4.is_listed_at(c, 700)));
        // Mainnet's list doesn't change, even at a version 4 height.
        let main4 = Net { rules: &[(2, 0), (4, 700)], ..MAINNET };
        for c in four {
            assert!(!MAINNET.is_listed_at(c, u64::MAX) && !main4.is_listed_at(c, 700) && !main4.is_listed_at(c, u64::MAX));
        }
        assert_eq!(main4.listed_at(700).collect::<Vec<_>>(), MAINNET.listed);
    }

    #[test]
    fn genesis_is_new_years_day_2027() {
        // Days from 1970-01-01 to 2027-01-01: 57 years, 14 of them leap years (1972..=2024).
        let days = 57 * 365 + 14;
        assert_eq!(MAINNET.genesis_time, Some(days * 86_400));
    }
}
