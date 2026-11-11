//! The ARCADE chain's catalog from this arcade's side (docs/ARCADECHAIN.md §2, §14.13, build
//! step 2b): storage items waiting for a block, shared over the mesh like transactions; the
//! segments this arcade cuts from the listed chains it keeps in full; and the catalog entries
//! and attestations it posts for them by itself.
//!
//! - **Items** wait here until a block holds them (or the chain can no longer take them). A
//!   writer puts the ones the next block can carry in it (`FixedHolders::fitting`). Every 30 s
//!   (and when there are new ones) each linked arcade on the chain is sent them all ("ac" frame
//!   op "it", the items one after another as blocks hold them).
//! - **Segments**: for a listed chain this arcade keeps in full, every complete range of
//!   heights (100 blocks below its tip, so a reorg of that chain can't move it) is measured
//!   once, and the chain is cut by the catalog's rule (segment::cut). A segment's root is
//!   made from the blocks as the chain stores them.
//! - **Posting**: with draw weight, this arcade offers the next segment of each such chain when
//!   nobody has, and attests every waiting entry whose root it made itself. It never stands
//!   behind two entries for the same heights (the catalog refuses that anyway).

use crate::node::Node;
use arcade_arc::block::{self, Attest, CatalogEntry, Item};
use arcade_arc::catalog::Catalog;
use arcade_arc::merkle::Hash;
use arcade_arc::params::{Net, RANGE};
use arcade_arc::segment::{self, Rooter, Span};
use arcade_chain::Reader;
use arcade_chain::params::ChainParams;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Most items kept waiting here.
const MAX_ITEMS: usize = 500;
/// Each peer is sent the waiting items this often (and when there are new ones).
const SEND_EVERY: Duration = Duration::from_secs(30);
/// A range is measured only this many blocks below its chain's tip.
const SETTLED: u32 = 100;

struct Pool {
    items: Vec<(Vec<u8>, Item)>,
    /// Bumped on every new item: a peer told an older number is sent them again.
    version: u64,
    told: BTreeMap<String, (u64, Instant)>,
}

static POOL: Mutex<Pool> = Mutex::new(Pool { items: Vec::new(), version: 0, told: BTreeMap::new() });

fn encoded(item: &Item) -> Vec<u8> {
    let mut v = Vec::new();
    item.encode(&mut v);
    v
}

/// Keep `item` until a block holds it. Whether it was new.
pub fn add(item: Item) -> bool {
    let e = encoded(&item);
    let mut p = POOL.lock().unwrap();
    if p.items.iter().any(|(x, _)| *x == e) || p.items.len() >= MAX_ITEMS {
        return false;
    }
    p.items.push((e, item));
    p.version += 1;
    true
}

/// The waiting items, oldest first.
pub fn waiting() -> Vec<Item> {
    POOL.lock().unwrap().items.iter().map(|(_, i)| i.clone()).collect()
}

/// A block holds these, or the chain will never take them: they stop waiting.
pub fn forget(gone: &[Item]) {
    let gone: Vec<Vec<u8>> = gone.iter().map(encoded).collect();
    POOL.lock().unwrap().items.retain(|(e, _)| !gone.contains(e));
}

/// Items for a peer, when it is due them: all of them, one after another.
pub fn due_for(peer: &str) -> Option<Vec<u8>> {
    let mut p = POOL.lock().unwrap();
    if p.items.is_empty() {
        return None;
    }
    let v = p.version;
    let due = p.told.get(peer).is_none_or(|(seen, at)| *seen != v || at.elapsed() >= SEND_EVERY);
    if !due {
        return None;
    }
    p.told.insert(peer.to_string(), (v, Instant::now()));
    Some(p.items.iter().flat_map(|(e, _)| e.clone()).collect())
}

/// Items a peer sent: each kept when its signature is right (the chain checks the rest).
pub fn on_items(net: &Net, blob: &[u8]) -> usize {
    let mut r = Reader::new(blob);
    let mut n = 0;
    while !r.is_empty() {
        let Ok(item) = Item::decode(&mut r) else { break };
        let signed = match &item {
            Item::CatalogEntry(e) => block::verify(&e.poster, &e.message(net), &e.sig),
            Item::Attest(a) => block::verify(&a.attester, &a.message(net), &a.sig),
            _ => false, // the chain takes nothing else yet
        };
        if signed && add(item) {
            n += 1;
        }
    }
    n
}

// ---------------- segments ----------------

/// Per chain: bytes of the blocks of each complete range, from range 0 on, and the roots made
/// (first height -> (last, bytes, root)).
#[derive(Default)]
struct Cut {
    ranges: Vec<u64>,
    roots: BTreeMap<u64, (u64, u64, Hash)>,
}

static CUTS: Mutex<BTreeMap<u16, Cut>> = Mutex::new(BTreeMap::new());
/// Ranges were left to measure at the last round: the next comes soon.
static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn busy() -> bool {
    BUSY.load(std::sync::atomic::Ordering::Relaxed)
}

fn block_bytes(node: &Node, p: &ChainParams, h: u32) -> Option<Vec<u8>> {
    let id = node.chain.hash_at(p.chain_no, h).ok()??;
    node.chain.block_raw(p.chain_no, &id).ok()?
}

/// Measure the ranges of `p` not measured yet, up to `SETTLED` below its tip (a few at a time:
/// every block is read once). Whether some are left.
fn measure(node: &Node, p: &ChainParams, most: usize) -> bool {
    let tip = node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height).unwrap_or(0);
    let settled = tip.saturating_sub(SETTLED) as u64;
    for _ in 0..most {
        let i = CUTS.lock().unwrap().entry(p.chain_no).or_default().ranges.len() as u64;
        let last = i * RANGE + RANGE - 1;
        if last > settled {
            return false;
        }
        let mut sum = 0u64;
        for h in i * RANGE..=last {
            let Some(b) = block_bytes(node, p, h as u32) else { return false };
            sum += b.len() as u64;
        }
        CUTS.lock().unwrap().entry(p.chain_no).or_default().ranges.push(sum);
    }
    true
}

/// The segments `p` is cut into so far, by the catalog's size.
pub fn spans(c: u16, full: u64) -> Vec<Span> {
    CUTS.lock().unwrap().get(&c).map(|cut| segment::cut(&cut.ranges, full)).unwrap_or_default()
}

/// A segment's (bytes, root), made from the stored blocks (and kept).
pub fn root_of(node: &Node, p: &ChainParams, first: u64, last: u64) -> Option<(u64, Hash)> {
    if let Some((l, bytes, root)) = CUTS.lock().unwrap().get(&p.chain_no).and_then(|c| c.roots.get(&first).copied()) {
        if l == last {
            return Some((bytes, root));
        }
    }
    let mut sizes = Vec::with_capacity((last - first + 1) as usize);
    for h in first..=last {
        sizes.push(block_bytes(node, p, h as u32)?.len() as u64);
    }
    let mut r = Rooter::new();
    r.push(&segment::table(&sizes));
    for h in first..=last {
        r.push(&block_bytes(node, p, h as u32)?);
    }
    let (root, bytes) = r.finish().ok()?;
    CUTS.lock().unwrap().entry(p.chain_no).or_default().roots.insert(first, (last, bytes, root));
    Some((bytes, root))
}

/// The listed chains of `net` this arcade keeps in full.
pub fn kept(node: &Node, net: &Net) -> Vec<&'static ChainParams> {
    net.listed
        .iter()
        .filter_map(|k| node.chain_params(k))
        .filter(|p| node.mode_of(p.key).as_deref() == Some("full") && crate::chains::kept(node, p))
        .collect()
}

/// One round: measure, then offer the next segment of each kept chain nobody offered, and
/// attest every waiting entry this arcade made the same root for. `catalog` is the chain's
/// now; `weight` this arcade's draw weight.
pub fn round(node: &Node, net: &Net, catalog: &Catalog, weight: u64) {
    let me = node.identity.verifying_key().to_bytes();
    // Items the chain is past: an entry for heights already listed or no longer next, an
    // attestation of an entry that no longer waits (and isn't waiting here either).
    let stale: Vec<Item> = waiting()
        .into_iter()
        .filter(|i| match i {
            Item::CatalogEntry(e) => catalog.next(&e.chain, e.family) > e.first && !catalog.all_waiting().any(|(_, o)| o.entry.chain == e.chain && o.entry.first == e.first),
            Item::Attest(a) => catalog.waiting(&a.segment).is_none() && !waiting().iter().any(|w| matches!(w, Item::CatalogEntry(e) if e.segment(net) == a.segment)),
            _ => true,
        })
        .collect();
    forget(&stale);
    let mut more = false;
    for p in kept(node, net) {
        more |= measure(node, p, 50);
        if weight == 0 {
            continue;
        }
        // Attest what matches our own root.
        for (id, cand) in catalog.all_waiting() {
            let e = &cand.entry;
            if e.chain != p.key || cand.behind.contains(&me) {
                continue;
            }
            // Never behind two entries for the same heights.
            if catalog.all_waiting().any(|(_, o)| o.entry.chain == e.chain && o.entry.first == e.first && o.behind.contains(&me)) {
                continue;
            }
            if root_of(node, p, e.first, e.last) == Some((e.bytes, e.root)) {
                let mut a = Attest { segment: *id, attester: me, sig: [0; 64] };
                a.sig = block::sign(&node.identity, &a.message(net));
                add(Item::Attest(a));
            }
        }
        // Offer the next segment when nobody has.
        let next = catalog.next(p.key, segment::FAMILY_BLOCKS);
        if catalog.all_waiting().any(|(_, o)| o.entry.chain == p.key && o.entry.first == next) {
            continue;
        }
        let Some(span) = spans(p.chain_no, catalog.segment_bytes()).into_iter().find(|s| s.first == next) else { continue };
        let Some((bytes, root)) = root_of(node, p, span.first, span.last) else { continue };
        let mut e = CatalogEntry { chain: p.key.to_string(), family: segment::FAMILY_BLOCKS, first: span.first, last: span.last, bytes, manifest: [0; 32], root, poster: me, sig: [0; 64] };
        e.sig = block::sign(&node.identity, &e.message(net));
        add(Item::CatalogEntry(e));
    }
    BUSY.store(more, std::sync::atomic::Ordering::Relaxed);
}

/// What the Chains & Coins row and arc.status say about it.
pub fn status(node: &Node, net: &Net, full: u64) -> serde_json::Value {
    let chains: Vec<serde_json::Value> = kept(node, net)
        .iter()
        .map(|p| {
            let ranges = CUTS.lock().unwrap().get(&p.chain_no).map(|c| c.ranges.len()).unwrap_or(0);
            serde_json::json!({"chain": p.key, "ranges": ranges, "segments": spans(p.chain_no, full).len()})
        })
        .collect();
    serde_json::json!({"kept": chains, "waiting_items": POOL.lock().unwrap().items.len()})
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcade_arc::params;

    fn entry(first: u64, poster: u8) -> Item {
        Item::CatalogEntry(CatalogEntry { chain: "peptest".into(), family: 0, first, last: first + RANGE - 1, bytes: 9, manifest: [0; 32], root: [poster; 32], poster: [poster; 32], sig: [0; 64] })
    }

    #[test]
    fn items_wait_once_and_go_when_a_block_holds_them() {
        let a = entry(0, 1);
        assert!(add(a.clone()));
        assert!(!add(a.clone()), "the same item once");
        assert!(due_for("peer-x").is_some());
        assert!(due_for("peer-x").is_none(), "not again until something changes or 30 s pass");
        let b = entry(RANGE, 2);
        add(b.clone());
        assert!(due_for("peer-x").is_some(), "a new item: sent again");
        forget(&[a.clone(), b.clone()]);
        assert!(!waiting().contains(&a) && !waiting().contains(&b));
        // Unsigned items from a peer are not kept.
        let mut blob = Vec::new();
        a.encode(&mut blob);
        assert_eq!(on_items(&params::TESTNET, &blob), 0);
    }
}
