//! The catalog (docs/ARCADECHAIN.md §2.4): which segments of the listed chains exist, as
//! every arcade agrees on them.
//!
//! For each family of each listed chain the catalog holds a row of **spans**, from height 0
//! on, each starting where the one before ended. A span holds one confirmed segment, or
//! nothing where the segment it held was proved wrong and removed.
//!
//! - An arcade with draw weight **offers** an entry for a span that is open: the one after
//!   the last, or a removed entry's (same first and last height).
//! - Arcades with draw weight **attest** to it. Offering an entry is attesting to it. A key
//!   stands behind one entry for a first height at a time.
//! - An entry is **confirmed** in the block that brings the weight behind it over half of all
//!   draw weight, counted as the weights are in that block. The other entries offered for
//!   its span are dropped with their attestations.
//! - From rules version 3 (§15 step 11), a segment of **blocks or coins** — the families a fraud proof can take out —
//!   is confirmed with **more than a tenth** of the draw weight, so a chain kept by few arcades is listed too. Such a
//!   segment is in its **challenge window** for [`CHALLENGE_WINDOW`] blocks: listed, so fraud proofs and reveal demands
//!   can reach it, but no replica of it registers (so it earns nothing) until the window is over.
//! - From rules version 4 (ARCADE issue #14, testnet only: mainnet's schedule has no version 4, so it keeps the
//!   numbers above), **every family** confirms with more than a tenth, the index families too, with the same challenge
//!   window when it is less than half. On today's testnet (two genesis writers of 1 GiB each) one writer alone lists
//!   an index segment; that is intended.
//! - A removed entry is never taken again.
//!
//! What this can't check is whether the root is the right one: that is what the attesters
//! say, and what reveal demands and fraud proofs settle when they lie.

use crate::block::{self, Attest, BlockError, CatalogEntry, Item, Key};
use crate::merkle::Hash;
use crate::params::{self, Net};
use crate::segment;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Blocks a segment confirmed by a tenth of the weight waits before replicas of it register (§15 step 11).
pub const CHALLENGE_WINDOW: u64 = 100;

/// Whether a family's segments confirm with a tenth of the draw weight at `height`: from rules version 3 those a fraud
/// proof can take out (blocks; coins), from version 4 (#14, testnet only; mainnet keeps half for the index families)
/// every family, the index families too. Below a half such a segment has its challenge window ([`Listed::settled`]).
pub fn light_family(net: &Net, family: u8, height: u64) -> bool {
    params::rules_v4(net, height) || net.rules_at(height) >= 3 && (family == segment::FAMILY_BLOCKS || family == segment::FAMILY_COINS)
}

/// The draw weights the catalog counts with.
pub trait Weights {
    fn weight(&self, key: &Key) -> u64;
    /// All draw weight.
    fn total(&self) -> u128;
}

/// A confirmed segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listed {
    /// Its segment id.
    pub id: Hash,
    pub entry: CatalogEntry,
    /// The height of the block that confirmed it.
    pub height: u64,
    /// Confirmed with less than half of the draw weight (a tenth or more): in its challenge window until
    /// `height + CHALLENGE_WINDOW`.
    pub light: bool,
}

impl Listed {
    /// Whether replicas of it may register at `height` (its challenge window over, if it has one).
    pub fn settled(&self, height: u64) -> bool {
        !self.light || height >= self.height + CHALLENGE_WINDOW
    }
}

/// An entry that waits for attestations.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub entry: CatalogEntry,
    /// The keys behind it: its poster, then the attesters in the order they came.
    pub behind: Vec<Key>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Span {
    first: u64,
    last: u64,
    listed: Option<Listed>,
}

/// One family of one chain.
type Line = (String, u8);

fn line_of(e: &CatalogEntry) -> Line {
    (e.chain.clone(), e.family)
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Op {
    Offered(Hash),
    Attested(Hash),
    /// `id` took its span; `dropped` is every entry that was offered for it, `id`'s too.
    Confirmed { id: Hash, dropped: Vec<(Hash, Candidate)>, new_span: bool },
    Removed(Listed),
}

/// What one block did, for taking it back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Undo(Vec<Op>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Catalog {
    net: Net,
    /// Bytes of blocks a segment must hold (`params::SEGMENT_BYTES`; less on local test chains).
    full: u64,
    lines: BTreeMap<Line, Vec<Span>>,
    waiting: BTreeMap<Hash, Candidate>,
    /// Where each confirmed segment is: its line and its span's place in it.
    listed: HashMap<Hash, (Line, usize)>,
    removed: HashSet<Hash>,
}

fn no(what: &'static str) -> BlockError {
    BlockError::Invalid(what)
}

impl Catalog {
    pub fn new(net: Net, full: u64) -> Self {
        Catalog { net, full, lines: BTreeMap::new(), waiting: BTreeMap::new(), listed: HashMap::new(), removed: HashSet::new() }
    }

    pub fn net(&self) -> Net {
        self.net
    }

    pub fn segment_bytes(&self) -> u64 {
        self.full
    }

    /// A confirmed segment by its id.
    pub fn get(&self, id: &Hash) -> Option<&Listed> {
        let (line, at) = self.listed.get(id)?;
        self.lines.get(line)?.get(*at)?.listed.as_ref()
    }

    /// An entry that waits, by its segment id.
    pub fn waiting(&self, id: &Hash) -> Option<&Candidate> {
        self.waiting.get(id)
    }

    /// Every entry that waits, in id order.
    pub fn all_waiting(&self) -> impl Iterator<Item = (&Hash, &Candidate)> {
        self.waiting.iter()
    }

    /// The spans of one family of one chain, in height order: (first, last, its segment).
    pub fn spans(&self, chain: &str, family: u8) -> impl Iterator<Item = (u64, u64, Option<&Listed>)> {
        self.lines.get(&(chain.to_string(), family)).into_iter().flatten().map(|s| (s.first, s.last, s.listed.as_ref()))
    }

    /// The first height of the next segment of this family of this chain.
    pub fn next(&self, chain: &str, family: u8) -> u64 {
        self.lines.get(&(chain.to_string(), family)).and_then(|spans| spans.last()).map_or(0, |s| s.last + 1)
    }

    /// Confirmed segments, and the bytes in them.
    pub fn totals(&self) -> (usize, u64) {
        let all = self.lines.values().flatten().filter_map(|s| s.listed.as_ref());
        all.fold((0, 0), |(n, bytes), l| (n + 1, bytes + l.entry.bytes))
    }

    /// Was this segment id confirmed once and then proved wrong?
    pub fn was_removed(&self, id: &Hash) -> bool {
        self.removed.contains(id)
    }

    /// Take in a block's catalog entries and attestations, in the block's order. Other items
    /// are someone else's. On an error nothing has changed.
    pub fn apply(&mut self, items: &[Item], height: u64, weights: &impl Weights) -> Result<Undo, BlockError> {
        let mut undo = Undo::default();
        for item in items {
            let done = match item {
                Item::CatalogEntry(e) => self.offer(e, height, weights, &mut undo),
                Item::Attest(a) => self.attest(a, height, weights, &mut undo),
                _ => Ok(()),
            };
            if let Err(e) = done {
                self.undo(undo);
                return Err(e);
            }
        }
        Ok(undo)
    }

    /// May an entry be offered for `first..=last` of this line?
    fn open(&self, line: &Line, first: u64, last: u64) -> bool {
        let spans = self.lines.get(line).map(Vec::as_slice).unwrap_or(&[]);
        if first == spans.last().map_or(0, |s| s.last + 1) {
            return true;
        }
        match spans.binary_search_by_key(&first, |s| s.first) {
            Ok(at) => spans[at].listed.is_none() && spans[at].last == last,
            Err(_) => false,
        }
    }

    /// Does `key` stand behind an entry that waits for the span starting at `first`?
    fn stands(&self, line: &Line, first: u64, key: &Key) -> bool {
        self.waiting.values().any(|c| c.entry.first == first && c.entry.family == line.1 && c.entry.chain == line.0 && c.behind.contains(key))
    }

    fn offer(&mut self, e: &CatalogEntry, height: u64, weights: &impl Weights, undo: &mut Undo) -> Result<(), BlockError> {
        if !self.net.is_listed_at(&e.chain, height) {
            return Err(no("catalog entry: not a listed chain"));
        }
        if !segment::family_at(&self.net, e.family, height) {
            return Err(no("catalog entry: no such family"));
        }
        if !segment::whole_ranges(e.first, e.last) {
            return Err(no("catalog entry: not whole ranges"));
        }
        // The table, then at least a full segment of blocks.
        let least = (e.last - e.first + 1).checked_mul(segment::ROW).and_then(|table| table.checked_add(self.full));
        if least.map_or(true, |least| e.bytes < least) {
            return Err(no("catalog entry: not a full segment"));
        }
        if !block::verify(&e.poster, &e.message(&self.net), &e.sig) {
            return Err(BlockError::BadSignature("catalog entry"));
        }
        if weights.weight(&e.poster) == 0 {
            return Err(no("catalog entry: its poster has no draw weight"));
        }
        let id = e.segment(&self.net);
        if self.removed.contains(&id) {
            return Err(no("catalog entry: proved wrong before"));
        }
        if self.waiting.contains_key(&id) {
            return Err(no("catalog entry: offered already"));
        }
        let line = line_of(e);
        if !self.open(&line, e.first, e.last) {
            return Err(no("catalog entry: not the next segment of its chain"));
        }
        if self.stands(&line, e.first, &e.poster) {
            return Err(no("catalog entry: its poster stands behind another for these heights"));
        }
        self.waiting.insert(id, Candidate { entry: e.clone(), behind: vec![e.poster] });
        undo.0.push(Op::Offered(id));
        self.settle(&id, height, weights, undo);
        Ok(())
    }

    fn attest(&mut self, a: &Attest, height: u64, weights: &impl Weights, undo: &mut Undo) -> Result<(), BlockError> {
        let Some(c) = self.waiting.get(&a.segment) else {
            return Err(no("attestation: no such entry waits"));
        };
        if !block::verify(&a.attester, &a.message(&self.net), &a.sig) {
            return Err(BlockError::BadSignature("attestation"));
        }
        if weights.weight(&a.attester) == 0 {
            return Err(no("attestation: its signer has no draw weight"));
        }
        if self.stands(&line_of(&c.entry), c.entry.first, &a.attester) {
            return Err(no("attestation: its signer stands behind an entry for these heights already"));
        }
        self.waiting.get_mut(&a.segment).expect("found above").behind.push(a.attester);
        undo.0.push(Op::Attested(a.segment));
        self.settle(&a.segment, height, weights, undo);
        Ok(())
    }

    /// Confirm the waiting entry `id` if more than half of all draw weight is behind it (a tenth, for the families
    /// fraud proofs reach from rules version 3, and for every family from version 4).
    fn settle(&mut self, id: &Hash, height: u64, weights: &impl Weights, undo: &mut Undo) {
        let c = &self.waiting[id];
        let behind: u128 = c.behind.iter().map(|key| weights.weight(key) as u128).sum();
        let half = behind * 2 > weights.total();
        let light = !half && light_family(&self.net, c.entry.family, height) && behind * 10 > weights.total();
        if !half && !light {
            return;
        }
        let (line, first, last) = (line_of(&c.entry), c.entry.first, c.entry.last);
        let rivals: Vec<Hash> = self.waiting.iter().filter(|(_, o)| o.entry.first == first && line_of(&o.entry) == line).map(|(id, _)| *id).collect();
        let dropped: Vec<(Hash, Candidate)> = rivals.into_iter().map(|id| (id, self.waiting.remove(&id).expect("just listed"))).collect();
        let entry = dropped.iter().find(|(other, _)| other == id).expect("its own").1.entry.clone();
        let listed = Some(Listed { id: *id, entry, height, light });
        let spans = self.lines.entry(line.clone()).or_default();
        let (at, new_span) = match spans.binary_search_by_key(&first, |s| s.first) {
            Ok(at) => {
                spans[at].listed = listed;
                (at, false)
            }
            Err(_) => {
                spans.push(Span { first, last, listed });
                (spans.len() - 1, true)
            }
        };
        self.listed.insert(*id, (line, at));
        undo.0.push(Op::Confirmed { id: *id, dropped, new_span });
    }

    /// Block 0's catalog (§10): the final genesis list's entries, confirmed at height 0 with no weight behind them yet
    /// (the list itself is what stands behind them: every one has a replica that passed a full audit). Each is checked
    /// as an offer is (a listed chain and family, whole ranges, a full segment, its poster's signature), and each
    /// chain's entries must run from height 0 with no gap or overlap.
    pub fn list_at_genesis(&mut self, entries: &[CatalogEntry]) -> Result<(), BlockError> {
        if !self.lines.is_empty() {
            return Err(no("genesis catalog: only an empty catalog"));
        }
        let mut sorted: Vec<&CatalogEntry> = entries.iter().collect();
        sorted.sort_by(|a, b| line_of(a).cmp(&line_of(b)).then(a.first.cmp(&b.first)));
        for e in sorted {
            if !self.net.is_listed_at(&e.chain, 0) || !segment::family_at(&self.net, e.family, 0) || !segment::whole_ranges(e.first, e.last) {
                return Err(no("genesis catalog: not a listed chain, family or whole ranges"));
            }
            let least = (e.last - e.first + 1).checked_mul(segment::ROW).and_then(|table| table.checked_add(self.full));
            if least.map_or(true, |least| e.bytes < least) {
                return Err(no("genesis catalog: not a full segment"));
            }
            if !block::verify(&e.poster, &e.message(&self.net), &e.sig) {
                return Err(BlockError::BadSignature("genesis catalog entry"));
            }
            let line = line_of(e);
            let id = e.segment(&self.net);
            let spans = self.lines.entry(line.clone()).or_default();
            if e.first != spans.last().map_or(0, |s| s.last + 1) || self.listed.contains_key(&id) {
                return Err(no("genesis catalog: a chain's entries must run from 0 with no gap or overlap"));
            }
            spans.push(Span { first: e.first, last: e.last, listed: Some(Listed { id, entry: e.clone(), height: 0, light: false }) });
            let at = spans.len() - 1;
            self.listed.insert(id, (line, at));
        }
        Ok(())
    }

    /// Take a confirmed segment out: it was proved wrong, or its holders didn't answer. Its
    /// span stays where it is and is open for another entry. `false` when `id` isn't listed.
    pub fn remove(&mut self, id: &Hash, undo: &mut Undo) -> bool {
        let Some((line, at)) = self.listed.remove(id) else { return false };
        let listed = self.lines.get_mut(&line).expect("listed")[at].listed.take().expect("listed");
        self.removed.insert(*id);
        undo.0.push(Op::Removed(listed));
        true
    }

    /// Take back what one block did; blocks are taken back newest first.
    pub fn undo(&mut self, undo: Undo) {
        for op in undo.0.into_iter().rev() {
            match op {
                Op::Offered(id) => {
                    self.waiting.remove(&id);
                }
                Op::Attested(id) => {
                    self.waiting.get_mut(&id).expect("it waited").behind.pop();
                }
                Op::Confirmed { id, dropped, new_span } => {
                    let (line, at) = self.listed.remove(&id).expect("it was listed");
                    let spans = self.lines.get_mut(&line).expect("its line");
                    if new_span {
                        spans.pop();
                        if spans.is_empty() {
                            self.lines.remove(&line);
                        }
                    } else {
                        spans[at].listed = None;
                    }
                    self.waiting.extend(dropped);
                }
                Op::Removed(listed) => {
                    let line = line_of(&listed.entry);
                    let spans = self.lines.get_mut(&line).expect("its line");
                    let at = spans.binary_search_by_key(&listed.entry.first, |s| s.first).expect("its span");
                    self.removed.remove(&listed.id);
                    self.listed.insert(listed.id, (line, at));
                    spans[at].listed = Some(listed);
                }
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::params::{MAINNET, RANGE, TESTNET};
    use ed25519_dalek::SigningKey;

    // The testnet as it ran before its 2026-11-15 restart (rules 1, then 2 from 1,065): what these tests were written for.
    const NET: Net = Net { rules: &[(1, 0), (2, 1_065)], ..TESTNET };
    const FULL: u64 = 1_000;

    struct Held(Vec<(Key, u64)>);

    impl Weights for Held {
        fn weight(&self, key: &Key) -> u64 {
            self.0.iter().find(|(k, _)| k == key).map_or(0, |(_, w)| *w)
        }
        fn total(&self) -> u128 {
            self.0.iter().map(|(_, w)| *w as u128).sum()
        }
    }

    fn secret(who: u8) -> SigningKey {
        SigningKey::from_bytes(&[who; 32])
    }

    fn key(who: u8) -> Key {
        secret(who).verifying_key().to_bytes()
    }

    /// Arcades 1 to 4 hold 10, 20, 30 and 40; arcade 9 holds nothing.
    fn held() -> Held {
        Held((1..=4).map(|who| (key(who), who as u64 * 10)).collect())
    }

    /// `who`'s entry for ranges `from..=to` of `chain`; `root` tells entries apart.
    pub(crate) fn entry_on(net: &Net, who: &SigningKey, chain: &str, from: u64, to: u64, bytes: u64, root: u8) -> CatalogEntry {
        let mut e = CatalogEntry {
            chain: chain.into(),
            family: segment::FAMILY_BLOCKS,
            first: from * RANGE,
            last: (to + 1) * RANGE - 1,
            bytes,
            manifest: [root; 32],
            root: [root; 32],
            poster: who.verifying_key().to_bytes(),
            sig: [0; 64],
        };
        e.sig = block::sign(who, &e.message(net));
        e
    }

    pub(crate) fn attest_on(net: &Net, who: &SigningKey, segment: Hash) -> Attest {
        let mut a = Attest { segment, attester: who.verifying_key().to_bytes(), sig: [0; 64] };
        a.sig = block::sign(who, &a.message(net));
        a
    }

    /// One range holds this many bytes when it is just full.
    fn just_full(ranges: u64) -> u64 {
        ranges * RANGE * segment::ROW + FULL
    }

    fn entry(who: u8, from: u64, to: u64, root: u8) -> CatalogEntry {
        entry_on(&NET, &secret(who), "doge", from, to, just_full(to - from + 1), root)
    }

    fn attest(who: u8, segment: Hash) -> Attest {
        attest_on(&NET, &secret(who), segment)
    }

    fn refused(c: &mut Catalog, item: Item) -> BlockError {
        let before = c.clone();
        let e = c.apply(&[item], 7, &held()).unwrap_err();
        assert_eq!(*c, before);
        e
    }

    #[test]
    fn an_entry_is_confirmed_by_more_than_half_the_weight() {
        let mut c = Catalog::new(NET, FULL);
        let empty = c.clone();
        let e = entry(1, 0, 0, 1);
        let id = e.segment(&NET);

        // Offered by 10 of 100: it waits.
        let u1 = c.apply(&[Item::CatalogEntry(e.clone())], 5, &held()).unwrap();
        assert_eq!(c.waiting(&id).unwrap().behind, vec![key(1)]);
        assert_eq!((c.get(&id), c.next("doge", 0)), (None, 0));
        let offered = c.clone();

        // 10 + 40 is exactly half: not enough. With 20 more it is.
        let u2 = c.apply(&[Item::Attest(attest(4, id))], 6, &held()).unwrap();
        assert!(c.get(&id).is_none());
        let half = c.clone();
        let u3 = c.apply(&[Item::Attest(attest(2, id))], 7, &held()).unwrap();
        assert_eq!(c.get(&id), Some(&Listed { id, entry: e.clone(), height: 7, light: false }));
        assert!(c.waiting(&id).is_none());
        assert_eq!(c.next("doge", 0), RANGE);
        assert_eq!(c.spans("doge", 0).map(|(first, last, l)| (first, last, l.is_some())).collect::<Vec<_>>(), vec![(0, RANGE - 1, true)]);
        assert_eq!(c.totals(), (1, e.bytes));
        // Another chain's row is its own.
        assert_eq!(c.next("pep", 0), 0);

        // Block by block, back to nothing.
        c.undo(u3);
        assert_eq!(c, half);
        c.undo(u2);
        assert_eq!(c, offered);
        c.undo(u1);
        assert_eq!(c, empty);

        // All in one block is the same as one by one; a holder of more than half needs nobody.
        let u = c.apply(&[Item::CatalogEntry(e.clone()), Item::Attest(attest(4, id)), Item::Attest(attest(2, id))], 7, &held()).unwrap();
        assert!(c.get(&id).is_some());
        c.undo(u);
        assert_eq!(c, empty);
        let alone = Held(vec![(key(1), 51), (key(2), 49)]);
        c.apply(&[Item::CatalogEntry(e)], 5, &alone).unwrap();
        assert!(c.get(&id).is_some());
    }

    #[test]
    fn from_rules_version_3_a_tenth_lists_blocks_with_a_challenge_window() {
        const NET3: Net = Net { rules: &[(1, 0), (2, 1), (3, 2)], ..TESTNET };
        let mut c = Catalog::new(NET3, FULL);
        let e = entry_on(&NET3, &secret(1), "doge", 0, 0, just_full(1), 1);
        let id = e.segment(&NET3);
        // 10 of 100 is a tenth, not more: it waits. Arcade 2's 20 more make 30: listed, light.
        c.apply(&[Item::CatalogEntry(e.clone())], 5, &held()).unwrap();
        assert!(c.get(&id).is_none());
        c.apply(&[Item::Attest(attest_on(&NET3, &secret(2), id))], 6, &held()).unwrap();
        let l = c.get(&id).unwrap();
        assert!(l.light && l.height == 6);
        assert!(!l.settled(6) && !l.settled(6 + CHALLENGE_WINDOW - 1) && l.settled(6 + CHALLENGE_WINDOW));
        // Before rules version 3 (height 1 here) the same weight isn't enough.
        let mut old = Catalog::new(NET3, FULL);
        old.apply(&[Item::CatalogEntry(e.clone()), Item::Attest(attest_on(&NET3, &secret(2), id))], 1, &held()).unwrap();
        assert!(old.get(&id).is_none());
        // An index family still needs more than half.
        let mut ix = entry_on(&NET3, &secret(1), "doge", 0, 0, just_full(1), 2);
        ix.family = segment::FAMILY_ORD;
        ix.sig = block::sign(&secret(1), &ix.message(&NET3));
        let ix_id = ix.segment(&NET3);
        let mut c2 = Catalog::new(NET3, FULL);
        c2.apply(&[Item::CatalogEntry(ix), Item::Attest(attest_on(&NET3, &secret(2), ix_id))], 6, &held()).unwrap();
        assert!(c2.get(&ix_id).is_none());
        // A tenth lists it at once, so later attestations find nothing waiting; a poster with more than half lists it
        // with no window.
        assert!(c.apply(&[Item::Attest(attest_on(&NET3, &secret(4), id))], 7, &held()).is_err());
        let mut c3 = Catalog::new(NET3, FULL);
        c3.apply(&[Item::CatalogEntry(e)], 6, &Held(vec![(key(1), 51), (key(2), 49)])).unwrap();
        assert!(c3.get(&id).is_some_and(|l| !l.light && l.settled(6)));
    }

    #[test]
    fn from_rules_version_4_a_tenth_lists_index_segments_too() {
        const NET4: Net = Net { rules: &[(3, 0), (4, 10)], ..TESTNET };
        let index = |who: u8, root: u8| {
            let mut e = entry_on(&NET4, &secret(who), "doge", 0, 0, just_full(1), root);
            e.family = segment::FAMILY_ORD;
            e.sig = block::sign(&secret(who), &e.message(&NET4));
            e
        };
        let tenth = Held(vec![(key(1), 10), (key(2), 90)]);
        let over = Held(vec![(key(1), 11), (key(2), 89)]);
        let e = index(1, 1);
        let id = e.segment(&NET4);
        // From version 4 (height 10): a tenth isn't enough, just over a tenth lists it, in its challenge window.
        let mut c = Catalog::new(NET4, FULL);
        c.apply(&[Item::CatalogEntry(e.clone())], 10, &tenth).unwrap();
        assert!(c.get(&id).is_none() && c.waiting(&id).is_some());
        let mut c = Catalog::new(NET4, FULL);
        c.apply(&[Item::CatalogEntry(e.clone())], 10, &over).unwrap();
        let l = c.get(&id).unwrap();
        assert!(l.light && !l.settled(10 + CHALLENGE_WINDOW - 1) && l.settled(10 + CHALLENGE_WINDOW));
        // Before it (height 9) an index family still needs more than half: 49 of 100 waits, 51 lists it, no window.
        let mut c = Catalog::new(NET4, FULL);
        c.apply(&[Item::CatalogEntry(e.clone())], 9, &over).unwrap();
        assert!(c.get(&id).is_none());
        let mut c = Catalog::new(NET4, FULL);
        c.apply(&[Item::CatalogEntry(e.clone())], 9, &Held(vec![(key(1), 49), (key(2), 51)])).unwrap();
        assert!(c.get(&id).is_none());
        c.apply(&[Item::CatalogEntry(index(2, 2))], 9, &Held(vec![(key(1), 49), (key(2), 51)])).unwrap();
        assert!(c.get(&index(2, 2).segment(&NET4)).is_some_and(|l| !l.light));
        // Blocks keep what version 3 gave them: more than a tenth, with the window; a tenth waits.
        let b = entry_on(&NET4, &secret(1), "doge", 0, 0, just_full(1), 3);
        let bid = b.segment(&NET4);
        let mut c = Catalog::new(NET4, FULL);
        c.apply(&[Item::CatalogEntry(b.clone())], 10, &tenth).unwrap();
        assert!(c.get(&bid).is_none());
        c.apply(&[Item::Attest(attest_on(&NET4, &secret(2), bid))], 11, &tenth).unwrap();
        assert!(c.get(&bid).is_some_and(|l| !l.light), "everybody behind it: more than half");
        let mut c = Catalog::new(NET4, FULL);
        c.apply(&[Item::CatalogEntry(b)], 10, &over).unwrap();
        assert!(c.get(&bid).is_some_and(|l| l.light));
        // Which families a tenth lists, by version; mainnet has no version 4, so its index families need half.
        for f in segment::FAMILIES.iter().copied() {
            let raw = f == segment::FAMILY_BLOCKS || f == segment::FAMILY_COINS;
            assert_eq!(light_family(&NET4, f, 9), raw);
            assert!(light_family(&NET4, f, 10));
            assert!((0..1_000_000).step_by(997).all(|h| !light_family(&MAINNET, f, h) || raw));
        }
    }

    #[test]
    fn the_dogefeed_family_is_taken_from_rules_version_4_only() {
        const NET4: Net = Net { rules: &[(3, 0), (4, 10)], ..TESTNET };
        let of = |net: &Net, family: u8| {
            let mut e = entry_on(net, &secret(1), "doge", 0, 0, just_full(1), 1);
            e.family = family;
            e.sig = block::sign(&secret(1), &e.message(net));
            e
        };
        let unknown = || no("catalog entry: no such family");
        let offer = |c: &mut Catalog, e: &CatalogEntry, h: u64| {
            let before = c.clone();
            let r = c.apply(&[Item::CatalogEntry(e.clone())], h, &held());
            if r.is_err() {
                assert_eq!(*c, before);
            }
            r
        };
        // Below version 4 (and on mainnet, which has none): refused as an unknown family is (13 is one).
        let d = of(&NET4, segment::FAMILY_DOGEFEED);
        assert_eq!(offer(&mut Catalog::new(NET4, FULL), &d, 9), Err(unknown()));
        assert_eq!(offer(&mut Catalog::new(NET4, FULL), &of(&NET4, 13), 9), Err(unknown()));
        assert_eq!(offer(&mut Catalog::new(MAINNET, FULL), &of(&MAINNET, segment::FAMILY_DOGEFEED), 500_000), Err(unknown()));
        assert!(Catalog::new(NET4, FULL).list_at_genesis(&[d.clone()]).is_err(), "nor in block 0's list");
        // From version 4: taken as any family; an unknown family and collections (11, kept, not listed) still aren't.
        let mut c = Catalog::new(NET4, FULL);
        offer(&mut c, &d, 10).unwrap();
        assert!(c.waiting(&d.segment(&NET4)).is_some() || c.get(&d.segment(&NET4)).is_some());
        assert_eq!(offer(&mut Catalog::new(NET4, FULL), &of(&NET4, 13), 10), Err(unknown()));
        assert_eq!(offer(&mut Catalog::new(NET4, FULL), &of(&NET4, segment::FAMILY_COLLECTIONS), 10), Err(unknown()));
        // The families before version 4 are taken at every height, as before.
        for f in [0u8, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10] {
            assert!((0..20).all(|h| segment::family_at(&NET4, f, h)) && segment::family_at(&MAINNET, f, 0), "family {f}");
        }
        assert!(!segment::family_at(&NET4, segment::FAMILY_DOGEFEED, 9) && segment::family_at(&NET4, segment::FAMILY_DOGEFEED, 10));
    }

    #[test]
    fn the_winner_drops_its_rivals_and_the_next_span_opens() {
        let mut c = Catalog::new(NET, FULL);
        let (right, wrong, longer) = (entry(1, 0, 0, 1), entry(2, 0, 0, 2), entry(3, 0, 1, 3));
        let ids = [right.segment(&NET), wrong.segment(&NET), longer.segment(&NET)];
        // Three entries for the heights from 0: two roots, and one that takes two ranges.
        c.apply(&[Item::CatalogEntry(right.clone()), Item::CatalogEntry(wrong), Item::CatalogEntry(longer)], 5, &held()).unwrap();
        assert_eq!(c.all_waiting().count(), 3);
        // The second range can't be offered before the first is settled.
        assert_eq!(refused(&mut c, Item::CatalogEntry(entry(4, 1, 1, 4))), no("catalog entry: not the next segment of its chain"));
        let before = c.clone();
        // 4 joins 1: 50 of 100, not enough. A key stands behind one entry for a first height.
        c.apply(&[Item::Attest(attest(4, ids[0]))], 6, &held()).unwrap();
        assert_eq!(refused(&mut c, Item::Attest(attest(4, ids[1]))), no("attestation: its signer stands behind an entry for these heights already"));
        assert_eq!(refused(&mut c, Item::Attest(attest(4, ids[0]))), no("attestation: its signer stands behind an entry for these heights already"));
        assert_eq!(refused(&mut c, Item::Attest(attest(2, ids[0]))), no("attestation: its signer stands behind an entry for these heights already"));
        assert_eq!(refused(&mut c, Item::CatalogEntry(entry(1, 0, 0, 9))), no("catalog entry: its poster stands behind another for these heights"));
        c.undo(Undo(vec![Op::Attested(ids[0])]));
        assert_eq!(c, before);

        // 3 and 4 hold 70: 4's attestation to 3's entry confirms it, and the others are gone.
        let u = c.apply(&[Item::Attest(attest(4, ids[2]))], 6, &held()).unwrap();
        assert_eq!(c.get(&ids[2]).unwrap().entry.last, 2 * RANGE - 1);
        assert_eq!(c.all_waiting().count(), 0);
        assert_eq!(c.next("doge", 0), 2 * RANGE);
        assert_eq!(refused(&mut c, Item::Attest(attest(4, ids[0]))), no("attestation: no such entry waits"));
        assert_eq!(refused(&mut c, Item::CatalogEntry(right)), no("catalog entry: not the next segment of its chain"));
        // Everybody is free again for the next span.
        c.apply(&[Item::CatalogEntry(entry(1, 2, 2, 5)), Item::CatalogEntry(entry(2, 2, 2, 6))], 8, &held()).unwrap();
        assert_eq!(c.all_waiting().count(), 2);
        // Taking the confirming block back brings the three back as they were.
        let mut back = Catalog::new(NET, FULL);
        back.apply(&[Item::CatalogEntry(entry(1, 0, 0, 1)), Item::CatalogEntry(entry(2, 0, 0, 2)), Item::CatalogEntry(entry(3, 0, 1, 3))], 5, &held()).unwrap();
        let mut again = back.clone();
        let u2 = again.apply(&[Item::Attest(attest(4, ids[2]))], 6, &held()).unwrap();
        assert_eq!(u, u2);
        again.undo(u2);
        assert_eq!(again, back);
    }

    #[test]
    fn what_an_entry_must_be() {
        let mut c = Catalog::new(NET, FULL);
        let good = entry(1, 0, 0, 1);
        let resigned = |change: &dyn Fn(&mut CatalogEntry)| {
            let mut e = good.clone();
            change(&mut e);
            e.sig = block::sign(&secret(1), &e.message(&NET));
            Item::CatalogEntry(e)
        };
        assert_eq!(refused(&mut c, resigned(&|e| e.chain = "mine".into())), no("catalog entry: not a listed chain"));
        assert_eq!(refused(&mut c, resigned(&|e| e.family = 11)), no("catalog entry: no such family"));
        for change in [(|e| e.first = 1) as fn(&mut CatalogEntry), |e| e.last -= 1, |e| e.last = u64::MAX, |e| (e.first, e.last) = (RANGE, RANGE - 1)] {
            assert_eq!(refused(&mut c, resigned(&change)), no("catalog entry: not whole ranges"));
        }
        assert_eq!(refused(&mut c, resigned(&|e| e.bytes -= 1)), no("catalog entry: not a full segment"));
        assert_eq!(refused(&mut c, resigned(&|e| e.bytes = 0)), no("catalog entry: not a full segment"));
        // A second range with only one range's bytes isn't full either.
        assert_eq!(refused(&mut c, resigned(&|e| e.last += RANGE)), no("catalog entry: not a full segment"));
        assert_eq!(refused(&mut c, Item::CatalogEntry(entry(1, 1, 1, 1))), no("catalog entry: not the next segment of its chain"));
        assert_eq!(refused(&mut c, Item::CatalogEntry(entry(9, 0, 0, 1))), no("catalog entry: its poster has no draw weight"));
        // Signed for the other network, by another key, or changed after signing.
        assert_eq!(refused(&mut c, Item::CatalogEntry(entry_on(&MAINNET, &secret(1), "doge", 0, 0, just_full(1), 1))), BlockError::BadSignature("catalog entry"));
        let mut stolen = good.clone();
        stolen.poster = key(2);
        assert_eq!(refused(&mut c, Item::CatalogEntry(stolen)), BlockError::BadSignature("catalog entry"));
        let mut pointer = good.clone();
        pointer.manifest[0] ^= 1;
        assert_eq!(refused(&mut c, Item::CatalogEntry(pointer)), BlockError::BadSignature("catalog entry"));

        // The good one waits; the same segment can't be offered again, by anyone.
        c.apply(&[Item::CatalogEntry(good.clone())], 5, &held()).unwrap();
        assert_eq!(refused(&mut c, Item::CatalogEntry(good.clone())), no("catalog entry: offered already"));
        let mut copy = good.clone();
        copy.poster = key(2);
        copy.sig = block::sign(&secret(2), &copy.message(&NET));
        assert_eq!(refused(&mut c, Item::CatalogEntry(copy)), no("catalog entry: offered already"));

        // Attestations: for an entry that waits, signed, by a key with weight.
        let id = good.segment(&NET);
        assert_eq!(refused(&mut c, Item::Attest(attest(2, [7; 32]))), no("attestation: no such entry waits"));
        assert_eq!(refused(&mut c, Item::Attest(attest(9, id))), no("attestation: its signer has no draw weight"));
        assert_eq!(refused(&mut c, Item::Attest(attest_on(&MAINNET, &secret(2), id))), BlockError::BadSignature("attestation"));
        let mut other = attest(2, id);
        other.attester = key(3);
        assert_eq!(refused(&mut c, Item::Attest(other)), BlockError::BadSignature("attestation"));

        // One bad item and the whole block's items are as if never seen.
        let before = c.clone();
        assert!(c.apply(&[Item::Attest(attest(2, id)), Item::Attest(attest(4, id)), Item::CatalogEntry(entry(9, 1, 1, 2))], 6, &held()).is_err());
        assert_eq!(c, before);
    }

    #[test]
    fn a_removed_entry_leaves_its_span_open() {
        let mut c = Catalog::new(NET, FULL);
        let w = Held(vec![(key(1), 60), (key(2), 40)]);
        // Three segments: ranges 0, 1 to 2, and 3.
        let entries = [entry(1, 0, 0, 1), entry(1, 1, 2, 2), entry(1, 3, 3, 3)];
        let ids: Vec<Hash> = entries.iter().map(|e| e.segment(&NET)).collect();
        for e in &entries {
            c.apply(&[Item::CatalogEntry(e.clone())], 5, &w).unwrap();
        }
        assert_eq!(c.totals().0, 3);
        let whole = c.clone();

        // The middle one is proved wrong.
        let mut u = Undo::default();
        assert!(c.remove(&ids[1], &mut u));
        assert!(!c.remove(&ids[1], &mut u));
        assert!(!c.remove(&[9; 32], &mut u));
        assert!(c.get(&ids[1]).is_none() && c.was_removed(&ids[1]));
        assert!(c.get(&ids[0]).is_some() && c.get(&ids[2]).is_some());
        assert_eq!(c.spans("doge", 0).map(|(first, _, l)| (first / RANGE, l.is_some())).collect::<Vec<_>>(), vec![(0, true), (1, false), (3, true)]);
        assert_eq!(c.next("doge", 0), 4 * RANGE);
        let holed = c.clone();

        // It is never taken again. Its span takes another root for the same heights only.
        let again = |c: &mut Catalog, e: CatalogEntry| c.apply(&[Item::CatalogEntry(e)], 9, &w).map(|_| ());
        assert_eq!(again(&mut c, entries[1].clone()), Err(no("catalog entry: proved wrong before")));
        assert_eq!(again(&mut c, entry(1, 1, 1, 4)), Err(no("catalog entry: not the next segment of its chain")));
        assert_eq!(again(&mut c, entry(1, 1, 3, 4)), Err(no("catalog entry: not the next segment of its chain")));
        assert_eq!(again(&mut c, entry(1, 2, 2, 4)), Err(no("catalog entry: not the next segment of its chain")));
        assert_eq!(c, holed);
        // 2 offers the right one (it waits), and the row goes on at its end meanwhile.
        let right = entry(2, 1, 2, 5);
        let u2 = c.apply(&[Item::CatalogEntry(right.clone()), Item::CatalogEntry(entry(1, 4, 4, 6))], 9, &w).unwrap();
        assert_eq!(c.next("doge", 0), 5 * RANGE);
        let u3 = c.apply(&[Item::Attest(attest(1, right.segment(&NET)))], 10, &w).unwrap();
        assert_eq!(c.get(&right.segment(&NET)).unwrap().height, 10);
        assert_eq!(c.spans("doge", 0).filter(|(_, _, l)| l.is_some()).count(), 4);
        assert_eq!(c.spans("doge", 0).map(|(first, _, _)| first / RANGE).collect::<Vec<_>>(), vec![0, 1, 3, 4]);

        // And all of it back.
        c.undo(u3);
        c.undo(u2);
        assert_eq!(c, holed);
        c.undo(u);
        assert_eq!(c, whole);
    }

    #[test]
    fn a_shibacoin_segment_is_listed_from_rules_version_4_on_the_testnet() {
        const NET4: Net = Net { rules: &[(1, 0), (2, 1), (3, 2), (4, 100)], ..TESTNET };
        let e = entry_on(&NET4, &secret(1), "shic", 0, 0, just_full(1), 1);
        let offer = |c: &mut Catalog, height: u64| c.apply(&[Item::CatalogEntry(e.clone())], height, &held()).map(|_| ());
        // Below the version 4 height Shibacoin isn't listed: the entry is refused and changes nothing.
        let mut c = Catalog::new(NET4, FULL);
        assert_eq!(offer(&mut c, 99), Err(no("catalog entry: not a listed chain")));
        assert_eq!(c, Catalog::new(NET4, FULL));
        // From it, the same entry is taken.
        offer(&mut c, 100).unwrap();
        let id = e.segment(&NET4);
        assert!(c.waiting(&id).is_some() || c.get(&id).is_some());
        // Mainnet never lists it, whatever the height.
        let main = Net { rules: &[(2, 0), (4, 100)], ..MAINNET };
        let mut m = Catalog::new(main, FULL);
        let e = entry_on(&main, &secret(1), "shic", 0, 0, just_full(1), 1);
        assert_eq!(m.apply(&[Item::CatalogEntry(e)], 100, &held()).map(|_| ()), Err(no("catalog entry: not a listed chain")));
    }
}
