//! The chain as a borrowed copy follows it: headers only (docs/ARCADECHAIN.md §14.14).
//!
//! An arcade that hasn't installed the chain can still use the coin through arcades that
//! have: they lend it their copy. What a lender says is never taken on its word. The borrower
//! follows the headers itself, from block 0, with the very rules a whole chain checks them by
//! ([`crate::chain::check_header`]): each writer's signature, draw and draw weight, the proof
//! of time, the difficulty, each epoch's draw seed. Between two branches it picks as every
//! arcade does ([`Entry::beats`]). A block a lender hands over is then checked by its id
//! against these headers and by its roots against its own header.
//!
//! What a borrowed copy does not check is what needs the coins and the storage records:
//! that no coin is spent twice, and what each coinbase pays. For that it leans on the
//! writers, who are checked by every arcade that has the chain installed.
//!
//! The rules look back no further than the epoch before, so only the newest headers are kept.

use crate::block::Header;
use crate::chain::{check_header, Branch, ChainError, Entry, Genesis};
use crate::params::{self, Net};
use crate::time::TimeProofs;
use arcade_chain::hash::Hash256;
use std::collections::VecDeque;

/// How many of the newest blocks are remembered: enough to check the next header after
/// stepping back as far as an arcade ever does, with room to spare.
/// What a follower's weight function answers for a writer whose draw weight it can't tell (a
/// registered replica's owner: a borrowed copy holds no registry). The header's own claimed
/// weight is taken then, on its source's word, until blocks commit to the weights (an open
/// point of docs/ARCADECHAIN.md §14.14).
pub const UNKNOWN_WEIGHT: u64 = u64::MAX;

pub const KEEP: usize = 400;

/// A header is checked against the blocks back to this many before the one it follows.
const LOOK_BACK: u64 = params::EPOCH_BLOCKS;

/// Where a borrowed copy reads the chain: an arcade that lends it.
pub trait Source {
    /// The headers of the branch it is on from height `from` (1 or later) on, in order: as
    /// many as it hands over at once, and none when it has no block that high.
    fn headers(&self, from: u64) -> Result<Vec<Header>, String>;
    /// The id of its block at `height`.
    fn id_at(&self, height: u64) -> Result<Hash256, String>;
}

/// What a round of following a source changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Round {
    /// The blocks from this height up are new, and `ids` are their ids, in order. Whatever
    /// was at these heights before is off the branch. With no ids nothing changed.
    pub from: u64,
    pub ids: Vec<Hash256>,
    /// The source says it has more.
    pub more: bool,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FollowError {
    /// The source didn't answer, or not with headers.
    #[error("{0}")]
    Source(String),
    #[error("the header it gave for block {0} breaks the rules: {1}")]
    Header(u64, ChainError),
    #[error("it is on a branch that left this arcade's more than {} blocks back", params::REORG_LIMIT)]
    Apart,
}

/// The newest blocks of one branch, each checked as it was added.
#[derive(Clone, Debug)]
pub struct Headers<T: TimeProofs> {
    net: Net,
    proofs: T,
    /// In height order, with no height left out; never empty.
    window: VecDeque<Entry>,
}

struct Window<'a>(&'a VecDeque<Entry>);

impl Branch for Window<'_> {
    fn at(&self, height: u64) -> &Entry {
        &self.0[(height - self.0[0].height) as usize]
    }
}

/// Is `entries` a run of blocks, each on the one before, long enough to check the next?
fn whole(entries: &VecDeque<Entry>) -> bool {
    let Some(first) = entries.front() else { return false };
    entries.iter().zip(entries.iter().skip(1)).all(|(a, b)| b.prev == a.id && b.height == a.height + 1) && (first.height == 0 || entries.len() as u64 > LOOK_BACK)
}

impl<T: TimeProofs> Headers<T> {
    /// Block 0 alone.
    pub fn new(net: Net, genesis: &Genesis, proofs: T) -> Self {
        assert!(proofs.takes_time() || net != params::MAINNET, "the main network runs on real proofs of time");
        let zero = Entry::block_0(&net, genesis);
        Headers { net, proofs, window: VecDeque::from([zero]) }
    }

    /// Picked up where this arcade left off: `entries` are the newest blocks it had checked,
    /// oldest first, from its own store. `None` when they aren't a run this chain can go on
    /// from (then start over from block 0).
    pub fn resume(net: Net, genesis: &Genesis, proofs: T, entries: Vec<Entry>) -> Option<Self> {
        assert!(proofs.takes_time() || net != params::MAINNET, "the main network runs on real proofs of time");
        let window: VecDeque<Entry> = entries.into();
        if !whole(&window) || (window[0].height == 0 && window[0] != Entry::block_0(&net, genesis)) {
            return None;
        }
        Some(Headers { net, proofs, window })
    }

    pub fn net(&self) -> &Net {
        &self.net
    }

    /// The newest block.
    pub fn tip(&self) -> &Entry {
        self.window.back().expect("never empty")
    }

    /// The height of the oldest block still remembered.
    pub fn base(&self) -> u64 {
        self.window[0].height
    }

    /// The block at `height`, while it is remembered.
    pub fn at(&self, height: u64) -> Option<&Entry> {
        self.window.get(usize::try_from(height.checked_sub(self.base())?).ok()?)
    }

    /// The blocks remembered, oldest first.
    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &Entry> {
        self.window.iter()
    }

    /// Take the next header: the block after the tip. `weight` is the draw weight its writer
    /// has (0 for a key with none); `now` this arcade's clock. Nothing changes on an error.
    pub fn push(&mut self, header: &Header, weight: u64, now: u64) -> Result<&Entry, ChainError> {
        header.check(&self.net)?;
        let entry = check_header(&self.net, &self.proofs, self.tip(), &Window(&self.window), header, weight, None, now)?;
        if header.weight != weight {
            return Err(ChainError::Weight);
        }
        self.window.push_back(entry);
        while self.window.len() > KEEP {
            self.window.pop_front();
        }
        Ok(self.tip())
    }

    /// Take the tip back off, to follow another branch. `None`, and nothing done, when that
    /// would leave too few blocks to check the next header by.
    pub fn pop(&mut self) -> Option<Entry> {
        let left = self.window.len() - 1;
        if left == 0 || (self.base() > 0 && left as u64 <= LOOK_BACK) {
            return None;
        }
        self.window.pop_back()
    }
}

impl<T: TimeProofs + Clone> Headers<T> {
    /// One round of following `source`, which says its tip is at height `their`: take the
    /// headers it has past this arcade's tip, or, when it is on another branch, move to that
    /// branch if it is the better one ([`Entry::beats`]). `weight` gives a writer's draw weight.
    ///
    /// Every header is checked as [`Headers::push`] checks it, and nothing changes unless all
    /// of a round's headers hold: on an error this arcade is where it was.
    pub fn follow(&mut self, source: &dyn Source, their: u64, weight: &dyn Fn(&[u8; 32]) -> u64, now: u64) -> Result<Round, FollowError> {
        let tip = self.tip().height;
        let nothing = Round { from: tip + 1, ids: Vec::new(), more: false };
        let mut next = Vec::new();
        if their > tip {
            next = source.headers(tip + 1).map_err(FollowError::Source)?;
        } else {
            match self.at(their) {
                Some(ours) if source.id_at(their).map_err(FollowError::Source)? != ours.id => {}
                // Behind this arcade on its own branch (or too far behind to tell).
                _ => return Ok(nothing),
            }
        }
        let mut to = self.clone();
        let mut from = tip + 1;
        if !next.first().is_some_and(|h| h.prev == self.tip().id) {
            // Not on this arcade's tip. Read its branch from as far back as an arcade ever
            // steps, and find the first block that isn't this arcade's.
            let start = tip.min(their).saturating_sub(params::REORG_LIMIT).max(self.base() + 1);
            next = source.headers(start).map_err(FollowError::Source)?;
            let same = next.iter().zip(start..).take_while(|(h, at)| self.at(*at).is_some_and(|e| e.id == h.id())).count();
            next.drain(..same);
            from = start + same as u64;
            let Some(first) = next.first() else { return Ok(nothing) };
            if self.at(from - 1).map(|e| e.id) != Some(first.prev) {
                return Err(FollowError::Apart);
            }
            while to.tip().height >= from {
                to.pop().ok_or(FollowError::Apart)?;
            }
        }
        let mut ids = Vec::new();
        // A branch that isn't the better one yet may be once more of it is read.
        for round in 0.. {
            for h in &next {
                let at = to.tip().height + 1;
                // `UNKNOWN_WEIGHT`: a writer whose weight the follower can't tell (a registered
                // replica's owner); the header's own claim is taken, on the source's word.
                let w = match weight(&h.writer) {
                    UNKNOWN_WEIGHT => h.weight,
                    w => w,
                };
                to.push(h, w, now).map_err(|e| FollowError::Header(at, e))?;
                ids.push(to.tip().id);
            }
            if next.is_empty() || round == 3 || to.tip().beats(self.tip()) || to.tip().height >= their {
                break;
            }
            next = source.headers(to.tip().height + 1).map_err(FollowError::Source)?;
        }
        if !to.tip().beats(self.tip()) {
            return Ok(nothing);
        }
        let more = to.tip().height < their;
        *self = to;
        Ok(Round { from, ids, more })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::BlockError;
    use crate::chain::tests::{chain, grow, grow_to, second_best_block, TestChain, LATE};
    use crate::chain::Storage;
    use crate::params::TESTNET;
    use crate::time::StandIn;

    fn genesis(c: &TestChain) -> Genesis {
        // The test chain's block 0, read back from what the chain made of it.
        let zero = c.entry(&c.id_at(0).unwrap()).unwrap();
        Genesis { time: zero.time, closed_list: [1; 32], final_list: [2; 32], beacon_1: [3; 32], beacon_2: [4; 32], difficulty: zero.difficulty }
    }

    fn light(c: &TestChain) -> Headers<StandIn> {
        let l = Headers::new(TESTNET, &genesis(c), StandIn);
        assert_eq!(l.tip(), c.entry(&c.id_at(0).unwrap()).unwrap(), "the test's block 0 is the chain's");
        l
    }

    fn weight(c: &TestChain, h: &Header) -> u64 {
        c.storage().weight(&h.writer)
    }

    /// A lender: the headers of its branch from block 1 on, handed over a few at a time.
    struct Lender(Vec<Header>, usize);

    impl Lender {
        /// The branch `c` is on.
        fn of(c: &TestChain, batch: usize) -> Lender {
            Lender((1..=c.tip().height).map(|h| c.block(&c.id_at(h).unwrap()).unwrap().header.clone()).collect(), batch)
        }
        fn tip(&self) -> u64 {
            self.0.len() as u64
        }
    }

    impl Source for Lender {
        fn headers(&self, from: u64) -> Result<Vec<Header>, String> {
            assert!(from >= 1);
            Ok(self.0.iter().skip(from as usize - 1).take(self.1).cloned().collect())
        }
        fn id_at(&self, height: u64) -> Result<Hash256, String> {
            self.0.get(height as usize - 1).map(Header::id).ok_or("no such block".into())
        }
    }

    #[test]
    fn a_lender_is_followed_round_by_round() {
        let mut c = chain();
        grow_to(&mut c, 130);
        let w = |k: &[u8; 32]| c.storage().weight(k);
        let lender = Lender::of(&c, 50);
        let mut l = light(&c);
        let first = l.follow(&lender, 130, &w, LATE).unwrap();
        assert_eq!((first.from, first.ids.len(), first.more), (1, 50, true));
        assert_eq!(first.ids[49], c.id_at(50).unwrap());
        assert!(l.follow(&lender, 130, &w, LATE).unwrap().more);
        let last = l.follow(&lender, 130, &w, LATE).unwrap();
        assert_eq!((last.from, last.ids.len(), last.more), (101, 30, false));
        assert_eq!(l.tip(), c.tip());
        // Nothing new: nothing changes, whatever height the lender names.
        for their in [130, 100, 131, 5_000] {
            let r = l.follow(&lender, their, &w, LATE).unwrap();
            assert_eq!((r.from, r.ids.len(), r.more), (131, 0, false));
        }
        assert_eq!(l.tip(), c.tip());
    }

    #[test]
    fn the_better_branch_is_followed_whoever_lends_it() {
        let mut c = chain();
        grow_to(&mut c, 30);
        // Two branches from block 30: one block of the worse, ten of the better.
        let mut worse = Lender::of(&c, 500);
        worse.0.push(second_best_block(&c).header);
        grow_to(&mut c, 40);
        let better = Lender::of(&c, 500);
        let w = |k: &[u8; 32]| c.storage().weight(k);

        let mut l = light(&c);
        l.follow(&worse, worse.tip(), &w, LATE).unwrap();
        assert_eq!((l.tip().height, l.tip().id), (31, worse.0[30].id()));
        // The better branch: back to block 30, then on.
        let r = l.follow(&better, 40, &w, LATE).unwrap();
        assert_eq!((r.from, r.ids.len(), r.more), (31, 10, false));
        assert_eq!(r.ids[0], c.id_at(31).unwrap());
        assert_eq!(l.tip(), c.tip());
        // A lender on the worse branch moves nothing.
        let r = l.follow(&worse, 31, &w, LATE).unwrap();
        assert!(r.ids.is_empty() && !r.more);
        assert_eq!(l.tip(), c.tip());

        // Level at one block each, the better block wins from either side.
        let mut one = Lender::of(&c, 500);
        one.0.truncate(31);
        let mut a = light(&c);
        a.follow(&worse, 31, &w, LATE).unwrap();
        assert_eq!(a.follow(&one, 31, &w, LATE).unwrap().from, 31);
        assert_eq!(a.tip().id, c.id_at(31).unwrap());
        let mut b = light(&c);
        b.follow(&one, 31, &w, LATE).unwrap();
        assert!(b.follow(&worse, 31, &w, LATE).unwrap().ids.is_empty());
        assert_eq!(b.tip().id, c.id_at(31).unwrap());
    }

    #[test]
    fn a_lender_cannot_talk_a_borrower_onto_a_branch_that_breaks_the_rules() {
        let mut c = chain();
        grow_to(&mut c, 60);
        let w = |k: &[u8; 32]| c.storage().weight(k);
        let good = Lender::of(&c, 500);
        let mut l = light(&c);
        l.follow(&Lender(good.0[..40].to_vec(), 500), 40, &w, LATE).unwrap();
        let before = l.tip().clone();
        assert_eq!(before.height, 40);

        // A header changed after it was signed, deep in the batch: none of the batch is taken.
        let mut bad = Lender::of(&c, 500);
        bad.0[54].pot.output[0] ^= 1;
        assert_eq!(l.follow(&bad, 60, &w, LATE).unwrap_err(), FollowError::Header(55, ChainError::Block(BlockError::BadSignature("block"))));
        assert_eq!(l.tip(), &before);
        // Headers of a chain with another block 0.
        let mut other = Lender::of(&c, 500);
        other.0.drain(..10);
        let mut fresh = light(&c);
        assert_eq!(fresh.follow(&other, 50, &w, LATE).unwrap_err(), FollowError::Apart);
        assert_eq!(fresh.tip().height, 0);
        // Writers given weight they don't have.
        assert!(matches!(l.follow(&good, 60, &|_| 1, LATE).unwrap_err(), FollowError::Header(41, ChainError::Weight)));
        // A lender that doesn't answer.
        struct Silent;
        impl Source for Silent {
            fn headers(&self, _: u64) -> Result<Vec<Header>, String> {
                Err("timed out".into())
            }
            fn id_at(&self, _: u64) -> Result<Hash256, String> {
                Err("timed out".into())
            }
        }
        assert_eq!(l.follow(&Silent, 60, &w, LATE).unwrap_err(), FollowError::Source("timed out".into()));
        assert_eq!(l.tip(), &before);

        l.follow(&good, 60, &w, LATE).unwrap();
        assert_eq!(l.tip(), c.tip());
    }

    #[test]
    fn headers_alone_give_what_the_whole_chain_gives() {
        let mut c = chain();
        let mut l = light(&c);
        for b in grow_to(&mut c, 250) {
            let e = l.push(&b.header, weight(&c, &b.header), LATE).unwrap().clone();
            assert_eq!(Some(&e), c.entry(&b.id()));
        }
        assert_eq!(l.tip(), c.tip());
        assert_eq!((l.base(), l.at(0).map(|e| e.id)), (0, c.id_at(0)));
        assert_eq!(l.at(137).map(|e| e.id), c.id_at(137));
        assert!(l.at(251).is_none());
    }

    #[test]
    fn only_the_newest_blocks_are_kept_and_they_are_enough() {
        // Past 400 blocks the oldest are dropped; epochs still start as the rules say.
        let mut c = chain();
        let mut l = light(&c);
        for b in grow_to(&mut c, 620) {
            l.push(&b.header, weight(&c, &b.header), LATE).unwrap();
        }
        assert_eq!(l.tip(), c.tip());
        assert_eq!(l.base(), 620 - KEEP as u64 + 1);
        assert!(l.at(l.base() - 1).is_none());

        // Stopped and started again from the arcade's own store.
        let kept: Vec<Entry> = l.entries().map(|e| Entry::decode(&e.encode()).unwrap()).collect();
        let mut again = Headers::resume(TESTNET, &genesis(&c), StandIn, kept.clone()).unwrap();
        for b in grow_to(&mut c, 710) {
            again.push(&b.header, weight(&c, &b.header), LATE).unwrap();
        }
        assert_eq!(again.tip(), c.tip());

        // Not from a run with a block missing, nor from one too short to check by.
        let mut gap = kept.clone();
        gap.remove(7);
        assert!(Headers::resume(TESTNET, &genesis(&c), StandIn, gap).is_none());
        assert!(Headers::resume(TESTNET, &genesis(&c), StandIn, kept[kept.len() - 100..].to_vec()).is_none());
        assert!(Headers::resume(TESTNET, &genesis(&c), StandIn, kept[kept.len() - 101..].to_vec()).is_some());
        assert!(Headers::resume(TESTNET, &genesis(&c), StandIn, Vec::new()).is_none());
        // Nor from another block 0.
        let mut other = genesis(&c);
        other.beacon_2 = [9; 32];
        assert!(Headers::resume(TESTNET, &other, StandIn, vec![Entry::block_0(&TESTNET, &genesis(&c))]).is_none());
        assert!(Headers::resume(TESTNET, &genesis(&c), StandIn, vec![Entry::block_0(&TESTNET, &genesis(&c))]).is_some());
    }

    #[test]
    fn a_header_that_breaks_the_rules_is_refused() {
        let mut c = chain();
        let mut l = light(&c);
        for b in grow_to(&mut c, 99) {
            l.push(&b.header, weight(&c, &b.header), LATE).unwrap();
        }
        let before = l.tip().clone();
        let b = grow(&mut c);
        let w = weight(&c, &b.header);
        assert!(b.header.draw_seed.is_some());

        // Anything changed breaks the writer's signature.
        let mut h = b.header.clone();
        h.pot.output[0] ^= 1;
        assert_eq!(l.push(&h, w, LATE).unwrap_err(), ChainError::Block(BlockError::BadSignature("block")));
        // More weight than the writer has, or less: not the weight the chain gives it.
        assert_eq!(l.push(&b.header, w - 1, LATE).unwrap_err(), ChainError::Weight);
        assert_eq!(l.push(&b.header, w + 1, LATE).unwrap_err(), ChainError::Weight);
        assert_eq!(l.push(&b.header, 0, LATE).unwrap_err(), ChainError::Weight);
        // Ahead of this arcade's clock.
        assert_eq!(l.push(&b.header, w, b.header.time - params::MAX_FUTURE_SECONDS - 1).unwrap_err(), ChainError::AheadOfClock);
        // Not on the tip.
        let next = grow(&mut c);
        assert_eq!(l.push(&next.header, weight(&c, &next.header), LATE).unwrap_err(), ChainError::Orphan);
        assert_eq!(l.tip(), &before);

        l.push(&b.header, w, LATE).unwrap();
        l.push(&next.header, weight(&c, &next.header), LATE).unwrap();
        assert_eq!(l.tip(), c.tip());
    }

    #[test]
    fn a_branch_is_left_for_a_better_one() {
        let mut c = chain();
        let mut l = light(&c);
        for b in grow_to(&mut c, 30) {
            l.push(&b.header, weight(&c, &b.header), LATE).unwrap();
        }
        // Two blocks on block 30: the lender's first word is the worse of them.
        let worse = second_best_block(&c);
        let better = grow(&mut c);
        let mut other = l.clone();
        l.push(&worse.header, weight(&c, &worse.header), LATE).unwrap();
        other.push(&better.header, weight(&c, &better.header), LATE).unwrap();
        assert!(other.tip().beats(l.tip()) && !l.tip().beats(other.tip()));

        assert_eq!(l.pop().map(|e| e.id), Some(worse.id()));
        l.push(&better.header, weight(&c, &better.header), LATE).unwrap();
        assert_eq!(l.tip(), c.tip());
        // Block 0 stays.
        let mut l = light(&c);
        assert!(l.pop().is_none());
        assert_eq!(l.tip().height, 0);
    }

    #[test]
    fn stepping_back_stops_while_the_next_header_can_still_be_checked() {
        let mut c = chain();
        let mut l = light(&c);
        for b in grow_to(&mut c, 620) {
            l.push(&b.header, weight(&c, &b.header), LATE).unwrap();
        }
        let mut back = 0;
        while l.pop().is_some() {
            back += 1;
        }
        // 400 kept, 101 needed: far more than the 100 an arcade ever goes back.
        assert_eq!(back, KEEP as u64 - LOOK_BACK - 1);
        assert!(back > params::REORG_LIMIT);
        let at = l.tip().height;
        let again = c.block(&c.id_at(at + 1).unwrap()).unwrap().header.clone();
        l.push(&again, weight(&c, &again), LATE).unwrap();
        assert_eq!(l.tip().id, c.id_at(at + 1).unwrap());
    }
}
