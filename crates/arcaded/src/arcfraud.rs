//! Fraud proofs from this arcade's side (docs/ARCADECHAIN.md §2.4, §14.22 item 5).
//!
//! A listed segment of blocks whose root isn't the one this arcade makes from its own copy of the chain is wrong (or
//! ours is: the proof settles it against the source headers). This arcade asks a holder of it for its leaf hashes
//! (checked against the listed root), finds the first leaf that differs from its own, walks the segment's own table
//! to the block holding it, and fetches that block's header with its table row. A header that isn't the source
//! chain's is posted as a fraud proof (Item::BadHeader): the chain takes the segment out and its replicas stop
//! earning. A difference only in a block's transactions waits for the second kind of proof (not built yet).
//!
//! Holders answer from their sealed copy over the ARCADE mesh: "lq" asks for a page of leaf hashes or a run of up to
//! MAX_PROOF_LEAVES leaves with its path; "la" answers.

use crate::node::Node;
use arcade_arc::block::{self, BadHeader, Item, RevealAnswer, RevealDemand, TxAnswer, TxChallenge};
use arcade_arc::merkle::{self, Hash};
use arcade_arc::params::Net;
use arcade_arc::segment::{self, Run};
use arcade_arc::sloth::DATA_LEAF;
use arcade_chain::Hash256;
use arcade_mesh::link::{Frame, Link, LinkError};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Leaf hashes in one answer (under the mesh's 64 kB frame).
pub const HASHES_PER_PAGE: u64 = 1_900;
/// Leaves in one run answered (their bytes under the mesh's 64 kB frame): also the largest transaction challenged.
pub const LEAVES_PER_RUN: u64 = 500;
/// How long an ask waits for its answer.
const WAIT: Duration = Duration::from_secs(30);

static NEXT: AtomicU64 = AtomicU64::new(1);
/// Asks waiting to go out on each peer's link.
static OUTBOX: Mutex<Option<HashMap<String, VecDeque<Value>>>> = Mutex::new(None);
/// Answers come in: ask id -> (head, blob).
static INBOX: Mutex<Option<HashMap<u64, (Value, Vec<u8>)>>> = Mutex::new(None);
/// Segments checked against our own root: right, or dealt with.
static CHECKED: Mutex<BTreeSet<Hash>> = Mutex::new(BTreeSet::new());
/// Challenges this arcade couldn't answer (said once).
static UNANSWERED: Mutex<BTreeSet<Hash>> = Mutex::new(BTreeSet::new());
/// Merge-mined segments left undemanded (logged once each).
static NOT_DEMANDED: Mutex<BTreeSet<Hash>> = Mutex::new(BTreeSet::new());

// ---------------- the mesh ----------------

/// On the link's tick: the asks waiting for this peer go out.
pub fn tick(link: &mut Link, frame: impl Fn(&str, Value) -> Frame) -> Result<(), LinkError> {
    let asks: Vec<Value> = OUTBOX.lock().unwrap().get_or_insert_with(HashMap::new).get_mut(&link.peer_id).map(|q| q.drain(..).collect()).unwrap_or_default();
    for a in asks {
        link.send(&frame("lq", a))?;
    }
    Ok(())
}

/// An answer to one of our asks.
pub fn on_answer(head: &Value, blob: &[u8]) {
    if let Some(id) = head["id"].as_u64() {
        let mut inbox = INBOX.lock().unwrap();
        let m = inbox.get_or_insert_with(HashMap::new);
        if m.len() < 64 {
            m.insert(id, (head.clone(), blob.to_vec()));
        }
    }
}

/// A peer's ask about a segment we hold: a page of its leaf hashes, or a run of its leaves with the path, from our
/// sealed copy. Nothing when we don't hold it.
pub fn serve(node: &Node, net: &Net, link: &mut Link, head: &Value, frame: impl Fn(&str, Value) -> Frame) -> Result<(), LinkError> {
    let id = head["id"].as_u64().unwrap_or(0);
    let Some(seg) = head["seg"].as_str().and_then(|s| hex::decode(s).ok()).and_then(|b| <Hash>::try_from(b).ok()) else { return Ok(()) };
    let Some(r) = crate::arcseal::replicas(node, net).into_iter().find(|r| r.segment == seg) else { return Ok(()) };
    let (first, count) = (head["first"].as_u64().unwrap_or(0), head["count"].as_u64().unwrap_or(0));
    let Ok(hashes) = std::fs::read(r.dir.join("data.hashes")) else { return Ok(()) };
    let all: Vec<Hash> = hashes.chunks_exact(32).map(|c| c.try_into().expect("32 bytes")).collect();
    let me = node.identity.verifying_key().to_bytes();
    match head["kind"].as_str() {
        Some("h") => {
            let end = first.saturating_add(count.min(HASHES_PER_PAGE)).min(all.len() as u64);
            if first >= end {
                return Ok(());
            }
            let mut f = frame("la", json!({"id": id, "leaves": all.len()}));
            f.blob = all[first as usize..end as usize].iter().flatten().copied().collect();
            link.send(&f)
        }
        Some("r") if (1..=LEAVES_PER_RUN).contains(&count) && first + count <= all.len() as u64 => {
            let mut blob = Vec::with_capacity(count as usize * DATA_LEAF);
            for i in first..first + count {
                match r.data_leaf(&me, i) {
                    Ok(l) => blob.extend_from_slice(&l),
                    Err(_) => return Ok(()),
                }
            }
            let Ok(path) = merkle::run_path(&all, first, count) else { return Ok(()) };
            let mut f = frame("la", json!({"id": id, "first": first, "path": path.iter().map(hex::encode).collect::<Vec<_>>()}));
            f.blob = blob;
            link.send(&f)
        }
        _ => Ok(()),
    }
}

/// Ask `peer` and wait for the answer.
fn ask(peer: &str, mut req: Value) -> Option<(Value, Vec<u8>)> {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    req["id"] = json!(id);
    OUTBOX.lock().unwrap().get_or_insert_with(HashMap::new).entry(peer.to_string()).or_default().push_back(req);
    let t0 = Instant::now();
    while t0.elapsed() < WAIT {
        std::thread::sleep(Duration::from_millis(200));
        if let Some(a) = INBOX.lock().unwrap().get_or_insert_with(HashMap::new).remove(&id) {
            return Some(a);
        }
    }
    None
}

/// A run of `count` leaves from `first` of segment `seg`, from `peer`, checked against the segment's root.
fn run_from(peer: &str, seg: &Hash, root: &Hash, bytes: u64, first: u64, count: u64) -> Option<Run> {
    let (h, blob) = ask(peer, json!({"seg": hex::encode(seg), "kind": "r", "first": first, "count": count}))?;
    if blob.len() != count as usize * DATA_LEAF {
        return None;
    }
    let leaves: Vec<[u8; DATA_LEAF]> = blob.chunks_exact(DATA_LEAF).map(|c| c.try_into().expect("a leaf")).collect();
    let path: Vec<Hash> = h["path"].as_array()?.iter().map(|p| hex::decode(p.as_str()?).ok()?.try_into().ok()).collect::<Option<_>>()?;
    let run = Run { first: h["first"].as_u64()?, leaves, path };
    (run.first == first && run.leaves.len() as u64 == count && run.proves(root, bytes)).then_some(run)
}

/// The run of leaves that holds bytes `at .. at + len` of the segment.
fn bytes_from(peer: &str, seg: &Hash, root: &Hash, total: u64, at: u64, len: u64) -> Option<Run> {
    let (first, n) = segment::leaves_of(at, len);
    run_from(peer, seg, root, total, first, n)
}

// ---------------- the watch ----------------

/// The leaf hashes of segment bytes held in memory.
fn leaf_hashes(data: &[u8]) -> Vec<Hash> {
    data.chunks(DATA_LEAF)
        .map(|c| {
            let mut l = [0u8; DATA_LEAF];
            l[..c.len()].copy_from_slice(c);
            merkle::leaf_hash(&l)
        })
        .collect()
}

/// Where two lists of leaf hashes first differ.
pub fn first_difference(ours: &[Hash], theirs: &[Hash]) -> Option<u64> {
    let n = ours.len().min(theirs.len());
    (0..n).find(|&i| ours[i] != theirs[i]).map(|i| i as u64).or((ours.len() != theirs.len()).then_some(n as u64))
}

/// The height whose block holds byte `at` of a segment, by its own table (`row(h)` reads the row of height `h`):
/// the last height whose block starts at or before it.
pub fn height_holding(first: u64, last: u64, at: u64, mut row: impl FnMut(u64) -> Option<u64>) -> Option<u64> {
    let (mut lo, mut hi) = (first, last);
    if row(first)? > at {
        return Some(first);
    }
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if row(mid)? <= at {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    Some(lo)
}

/// Every ten minutes: listed segments of blocks of the chains kept here, against our own roots.
pub fn start(node: std::sync::Arc<Node>) {
    let _ = std::thread::Builder::new().name("arc fraud watch".into()).spawn(move || loop {
        // Every 10 minutes, or 10 blocks on a faster test chain: well inside a segment's challenge window (§15 step 11).
        let every = crate::arcsvc::svc().map_or(600, |s| (s.net().block_seconds * 10).clamp(10, 600));
        std::thread::sleep(Duration::from_secs(every));
        if let Err(e) = watch(&node) {
            eprintln!("arc: fraud watch: {e}");
        }
    });
}

fn watch(node: &Node) -> Result<(), String> {
    let Some(svc) = crate::arcsvc::svc() else { return Ok(()) };
    let net = svc.net();
    let reg = svc.registry();
    for p in crate::arcseg::kept(node, &net, svc.next_height()) {
        if crate::arcsvc::runs(p) || crate::borrow::is_borrowed(p.key) {
            continue;
        }
        // The coins family (§15 step 12): a listed record that isn't ours, proved wrong by its own making transaction.
        for (first, last, l) in reg.catalog().spans(p.key, segment::FAMILY_COINS) {
            let Some(l) = l else { continue };
            if CHECKED.lock().unwrap().contains(&l.id) {
                continue;
            }
            let Some((bytes, root)) = crate::arcseg::root_of(node, p, segment::FAMILY_COINS, first, last) else { continue };
            if (bytes, root) == (l.entry.bytes, l.entry.root) {
                CHECKED.lock().unwrap().insert(l.id);
                continue;
            }
            eprintln!("arc: the listed {} coins segment {first}..={last} isn't the one this arcade makes: looking for the proof", p.name);
            let mut holders: Vec<String> = reg.all_replicas().iter().filter(|r| r.1 == l.id).map(|r| hex::encode(r.2)).collect();
            let poster = hex::encode(l.entry.poster);
            if !holders.contains(&poster) {
                holders.push(poster);
            }
            for peer in holders {
                if let Some(proof) = investigate_coins(node, p, &l.entry, &l.id, &peer) {
                    eprintln!("arc: {} coins record of block {} is wrong (entry {}): fraud proof posted", p.name, proof.height, proof.entry);
                    crate::arcseg::add(Item::BadCoins(Box::new(proof)));
                    CHECKED.lock().unwrap().insert(l.id);
                    break;
                }
            }
        }
        // The ord journal (§15 step 13): a listed record that isn't ours, proved wrong where its row is a one-piece
        // inscription's.
        for (first, last, l) in reg.catalog().spans(p.key, segment::FAMILY_ORD) {
            let Some(l) = l else { continue };
            if CHECKED.lock().unwrap().contains(&l.id) {
                continue;
            }
            let Some((bytes, root)) = crate::arcseg::root_of(node, p, segment::FAMILY_ORD, first, last) else { continue };
            CHECKED.lock().unwrap().insert(l.id);
            if (bytes, root) == (l.entry.bytes, l.entry.root) {
                continue;
            }
            eprintln!("arc: the listed {} ord segment {first}..={last} isn't the one this arcade makes: looking for the proof", p.name);
            let mut holders: Vec<String> = reg.all_replicas().iter().filter(|r| r.1 == l.id).map(|r| hex::encode(r.2)).collect();
            let poster = hex::encode(l.entry.poster);
            if !holders.contains(&poster) {
                holders.push(poster);
            }
            for peer in holders {
                if let Some(proof) = investigate_ord(node, p, &l.entry, &l.id, &peer) {
                    eprintln!("arc: {} ord record of block {} is wrong (row {}): fraud proof posted", p.name, proof.height, proof.entry);
                    crate::arcseg::add(Item::BadOrd(Box::new(proof)));
                    break;
                }
            }
        }
        for (first, last, l) in reg.catalog().spans(p.key, segment::FAMILY_BLOCKS) {
            let Some(l) = l else { continue };
            if CHECKED.lock().unwrap().contains(&l.id) {
                continue;
            }
            let Some((bytes, root)) = crate::arcseg::root_of(node, p, segment::FAMILY_BLOCKS, first, last) else { continue };
            if (bytes, root) == (l.entry.bytes, l.entry.root) {
                CHECKED.lock().unwrap().insert(l.id);
                continue;
            }
            eprintln!("arc: the listed {} segment {first}..={last} isn't the one this arcade makes: looking for the proof", p.name);
            // Its replicas' owners, and its poster: in its challenge window nobody is registered yet, but the poster
            // made the root from the data.
            let mut holders: Vec<String> = reg.all_replicas().iter().filter(|r| r.1 == l.id).map(|r| hex::encode(r.2)).collect();
            let poster = hex::encode(l.entry.poster);
            if !holders.contains(&poster) {
                holders.push(poster);
            }
            // A holder that hands out nothing can't be checked against: its leaves are demanded on the chain (§2.4).
            let reachable = holders.iter().any(|peer| ask(peer, json!({"seg": hex::encode(l.id), "kind": "h", "first": 0, "count": 1})).is_some_and(|a| !a.1.is_empty()));
            // On a merge-mined chain one block can carry different, equally valid AuxPoW (its hash is the same), so a
            // root that differs from ours isn't a sign of a bad segment until the leaves show it; and an answered
            // demand silences this arcade for an audit period (ARCADE #11: the JB20B, for Flopcoin 539930). Ask the
            // holders again next round instead of demanding.
            if !reachable && p.format.auxpow {
                if NOT_DEMANDED.lock().unwrap().insert(l.id) {
                    eprintln!("arc: no holder of the listed {} segment {first}..={last} hands out its leaves; merge-mined, so not demanding them (AuxPoW may differ): asking again later", p.name);
                }
                continue;
            }
            if !reachable {
                eprintln!("arc: no holder of the listed {} segment {first}..={last} hands out its leaves: demanding them on the chain", p.name);
                let _ = demand(node, &net, &l.id, 0, 16);
                CHECKED.lock().unwrap().insert(l.id);
                continue;
            }
            for peer in holders {
                match investigate(node, p, &l.entry, &l.id, &peer) {
                    Some(proof) => {
                        eprintln!("arc: {} block {} in the listed segment isn't the source chain's: fraud proof posted", p.name, proof.height);
                        crate::arcseg::add(Item::BadHeader(Box::new(proof)));
                        CHECKED.lock().unwrap().insert(l.id);
                        break;
                    }
                    None => continue,
                }
            }
        }
    }
    Ok(())
}

/// The entries of a coins record, each with its end (from the record's start): (entry, bytes from the start to its end).
fn coins_entries(rec: &[u8]) -> Option<Vec<(arcade_arc::registry::CoinsEntry, u64)>> {
    let run = Run::of_bytes(rec);
    let n = u32::from_le_bytes(rec.get(..4)?.try_into().ok()?);
    let mut out = Vec::new();
    let mut at = 4u64;
    for k in 0..n {
        let e = arcade_arc::registry::coins_entry(&run, 0, k)?;
        at += 53 + e.script.len() as u64;
        out.push((e, at));
    }
    Some(out)
}

/// From one holder: the proof that a coins record in the segment is wrong, if one is (§15 step 12). The first leaf
/// that differs from ours, the record holding it, its first entry unlike ours, and that coin's making transaction
/// with its block's header and Merkle path, from this arcade's own copy of the chain.
/// A listed segment of a record family (coins, a journal) whose root isn't ours: from `peer`, the first height whose
/// record differs, with its table row run, the record's start, their record (as far as a proof can carry) and ours.
struct Differing {
    height: u64,
    row_run: Run,
    start: u64,
    theirs: Vec<u8>,
    ours: Vec<u8>,
}

fn differing_record(node: &Node, p: &arcade_chain::params::ChainParams, e: &block::CatalogEntry, seg: &Hash, peer: &str, family: u8) -> Option<Differing> {
    let n = e.bytes.div_ceil(DATA_LEAF as u64);
    let mut theirs: Vec<Hash> = Vec::with_capacity(n as usize);
    while (theirs.len() as u64) < n {
        let (_, blob) = ask(peer, json!({"seg": hex::encode(seg), "kind": "h", "first": theirs.len(), "count": HASHES_PER_PAGE}))?;
        if blob.is_empty() || blob.len() % 32 != 0 {
            return None;
        }
        theirs.extend(blob.chunks_exact(32).map(|c| <Hash>::try_from(c).expect("32 bytes")));
    }
    if merkle::root(&theirs).ok()? != e.root {
        return None;
    }
    let mut data = Vec::new();
    let mut sizes = Vec::new();
    let mut ours_by_h = Vec::new();
    for h in e.first..=e.last {
        let u = crate::arcseg::unit_bytes(node, p, family, h as u32)?;
        sizes.push(u.len() as u64);
        data.extend_from_slice(&u);
        ours_by_h.push(u);
    }
    data.extend(segment::table(&sizes));
    let at = first_difference(&leaf_hashes(&data), &theirs)? * DATA_LEAF as u64;
    let data_bytes = e.bytes.checked_sub(segment::table_bytes(e.last - e.first + 1))?;
    let row = |h: u64| bytes_from(peer, seg, &e.root, e.bytes, segment::row_at(data_bytes, e.first, h), segment::ROW).and_then(|r| r.row(segment::row_at(data_bytes, e.first, h)));
    let height = if at >= data_bytes { (e.first + (at - data_bytes) / segment::ROW).min(e.last) } else { height_holding(e.first, e.last, at, row)? };
    let rows_at = segment::row_at(data_bytes, e.first, height);
    let row_run = bytes_from(peer, seg, &e.root, e.bytes, rows_at, if height == e.last { segment::ROW } else { 2 * segment::ROW })?;
    let start = row_run.row(rows_at)?;
    let end = if height == e.last { data_bytes } else { row_run.row(rows_at + segment::ROW)? };
    let most = (block::MAX_COINS_PROOF_LEAVES as u64 - 2) * DATA_LEAF as u64;
    let take = (end - start).min(most);
    let rec_run = bytes_from(peer, seg, &e.root, e.bytes, start, take)?;
    let theirs_rec = rec_run.bytes(start, take)?;
    Some(Differing { height, row_run, start, theirs: theirs_rec, ours: ours_by_h[(height - e.first) as usize].clone() })
}

/// The making transaction of `txid` from this arcade's copy of the chain: (raw, height, its block's header, index,
/// Merkle path).
fn made_by(node: &Node, p: &arcade_chain::params::ChainParams, txid: &Hash256) -> Option<(Vec<u8>, u32, Vec<u8>, usize, Vec<Hash>)> {
    let (raw, bh) = node.chain.tx_raw(p, txid).ok().flatten()?;
    let made_at = node.chain.block_height_of(p.chain_no, bh).ok()??;
    let bid = node.chain.hash_at(p.chain_no, made_at).ok()??;
    let braw = node.chain.block_raw(p.chain_no, &bid).ok()??;
    let blk = arcade_chain::Block::parse_with(&braw, &p.format).ok()?;
    let ids: Vec<Hash256> = blk.txs.iter().map(|x| x.txid).collect();
    let index = ids.iter().position(|x| x == txid)?;
    Some((raw, made_at, braw[..80].to_vec(), index, merkle_path(&ids, index)))
}

/// From one holder: the proof that an ord journal row in the segment is wrong, if one is provable (§15 step 13): the
/// first row of the first differing record that isn't ours, when it is a one-piece inscription's row.
fn investigate_ord(node: &Node, p: &arcade_chain::params::ChainParams, e: &block::CatalogEntry, seg: &Hash, peer: &str) -> Option<block::BadOrd> {
    let d = differing_record(node, p, e, seg, peer, segment::FAMILY_ORD)?;
    let (theirs, ours) = (Run::of_bytes(&d.theirs), Run::of_bytes(&d.ours));
    let n = u32::from_le_bytes(d.theirs.get(..4)?.try_into().ok()?);
    let mut at = 4u64;
    for k in 0..n {
        let Some(t) = arcade_arc::registry::journal_row(&theirs, 0, k) else { break };
        at += 1 + t.0.len() as u64 + 4 + t.1.len() as u64 + 1 + t.2.as_ref().map_or(0, |v| 4 + v.len() as u64);
        if arcade_arc::registry::journal_row(&ours, 0, k).as_ref() == Some(&t) {
            continue;
        }
        let claim = (t.0 == "ord.v1.insc").then(|| t.2.as_deref().and_then(crate::arcsvc::ord_claim)).flatten();
        let Some(claim) = claim else {
            eprintln!("arc: {} ord record of block {} differs at a row no proof reaches yet: left to the weight behind it", p.name, d.height);
            return None;
        };
        let Some((raw, made_at, header, index, path)) = made_by(node, p, &Hash256(claim.txid)) else {
            eprintln!("arc: {} ord record of block {} names transaction {} that this arcade doesn't have", p.name, d.height, Hash256(claim.txid).to_hex());
            return None;
        };
        let record = bytes_from(peer, seg, &e.root, e.bytes, d.start, at)?;
        return Some(block::BadOrd { segment: *seg, height: d.height, row: d.row_run, record, entry: k, at: made_at as u64, header, tx: raw, index: index as u64, path });
    }
    None
}

fn investigate_coins(node: &Node, p: &arcade_chain::params::ChainParams, e: &block::CatalogEntry, seg: &Hash, peer: &str) -> Option<block::BadCoins> {
    let d = differing_record(node, p, e, seg, peer, segment::FAMILY_COINS)?;
    let (height, row_run, start, theirs_rec) = (d.height, d.row_run, d.start, d.theirs);
    let ours = coins_entries(&d.ours)?;
    // Their entries as far as the run reaches, each against ours.
    let mut k = 0u32;
    let mut entry_end = None;
    let their_n = u32::from_le_bytes(theirs_rec.get(..4)?.try_into().ok()?);
    let mut pos = 4u64;
    while k < their_n {
        let Some(t) = arcade_arc::registry::coins_entry(&Run::of_bytes(&theirs_rec), 0, k) else { break };
        pos += 53 + t.script.len() as u64;
        if ours.get(k as usize).map(|o| &o.0) != Some(&t) {
            entry_end = Some((t, pos));
            break;
        }
        k += 1;
    }
    let Some((t, len)) = entry_end else {
        eprintln!("arc: {} coins record of block {height} differs only past what a proof can carry, or in its count: left to reveal demands", p.name);
        return None;
    };
    // The coin's making transaction, from our own copy of the chain.
    let txid = Hash256(t.txid);
    let Some((raw, made_at, header, index, path)) = made_by(node, p, &txid) else {
        eprintln!("arc: {} coins record of block {height} names transaction {} that this arcade doesn't have: left to reveal demands", p.name, txid.to_hex());
        return None;
    };
    let record = bytes_from(peer, seg, &e.root, e.bytes, start, len)?;
    Some(block::BadCoins {
        segment: *seg,
        height,
        row: row_run,
        record,
        entry: k,
        at: made_at as u64,
        header,
        tx: raw,
        index: index as u64,
        path,
    })
}

/// From one holder: the proof that the segment holds a header that isn't the source chain's, if it does.
fn investigate(node: &Node, p: &arcade_chain::params::ChainParams, e: &arcade_arc::block::CatalogEntry, seg: &Hash, peer: &str) -> Option<BadHeader> {
    // Their leaf hashes, page by page, and ours: where they first differ.
    let n = e.bytes.div_ceil(DATA_LEAF as u64);
    let mut theirs: Vec<Hash> = Vec::with_capacity(n as usize);
    while (theirs.len() as u64) < n {
        let (_, blob) = ask(peer, json!({"seg": hex::encode(seg), "kind": "h", "first": theirs.len(), "count": HASHES_PER_PAGE}))?;
        if blob.is_empty() || blob.len() % 32 != 0 {
            return None;
        }
        theirs.extend(blob.chunks_exact(32).map(|c| <Hash>::try_from(c).expect("32 bytes")));
    }
    if merkle::root(&theirs).ok()? != e.root {
        return None; // not this segment's leaves
    }
    let mut data = Vec::new();
    let mut sizes = Vec::new();
    for h in e.first..=e.last {
        let u = crate::arcseg::unit_bytes(node, p, segment::FAMILY_BLOCKS, h as u32)?;
        sizes.push(u.len() as u64);
        data.extend(u);
    }
    data.extend(segment::table(&sizes));
    let at = first_difference(&leaf_hashes(&data), &theirs)? * DATA_LEAF as u64;
    // Their table: the block holding that byte (or, in the table, the height of that row).
    let block_bytes = e.bytes.checked_sub(segment::table_bytes(e.last - e.first + 1))?;
    let row = |h: u64| bytes_from(peer, seg, &e.root, e.bytes, segment::row_at(block_bytes, e.first, h), segment::ROW).and_then(|r| r.row(segment::row_at(block_bytes, e.first, h)));
    let height = if at >= block_bytes { (e.first + (at - block_bytes) / segment::ROW).min(e.last) } else { height_holding(e.first, e.last, at, row)? };
    let row_run = bytes_from(peer, seg, &e.root, e.bytes, segment::row_at(block_bytes, e.first, height), segment::ROW)?;
    let start = row_run.row(segment::row_at(block_bytes, e.first, height))?;
    let header = bytes_from(peer, seg, &e.root, e.bytes, start, 80)?;
    let h80 = header.bytes(start, 80)?;
    let ours: Hash256 = node.chain.hash_at(p.chain_no, height as u32).ok()??;
    if arcade_chain::hash::sha256d(&h80) == ours {
        // The right header, other transactions: challenge the first that isn't in our block (second kind).
        challenge(node, p, e, seg, peer, height, block_bytes);
        return None;
    }
    Some(BadHeader { segment: *seg, height, row: row_run, header })
}

/// The second kind (§2.4): block `height` of the listed segment has the right header but holds a transaction our block
/// doesn't. Its body is fetched from `peer`, read transaction by transaction, and the first one not in our block is
/// challenged (posted as Item::TxChallenge, signed by this arcade).
fn challenge(node: &Node, p: &arcade_chain::params::ChainParams, e: &block::CatalogEntry, seg: &Hash, peer: &str, height: u64, block_bytes: u64) -> Option<()> {
    let rows_at = segment::row_at(block_bytes, e.first, height);
    let row = bytes_from(peer, seg, &e.root, e.bytes, rows_at, if height == e.last { segment::ROW } else { 2 * segment::ROW })?;
    let start = row.row(rows_at)?;
    let end = if height == e.last { block_bytes } else { row.row(rows_at + segment::ROW)? };
    // Their block, fetched a run at a time.
    let mut body = Vec::with_capacity((end - start) as usize);
    let mut at = start;
    while at < end {
        let len = (end - at).min(LEAVES_PER_RUN * DATA_LEAF as u64 - DATA_LEAF as u64);
        let run = bytes_from(peer, seg, &e.root, e.bytes, at, len)?;
        body.extend(run.bytes(at, len)?);
        at += len;
    }
    let theirs = arcade_chain::Block::parse_with(&body, &p.format).ok()?;
    let raw_ours = crate::arcseg::unit_bytes(node, p, segment::FAMILY_BLOCKS, height as u32)?;
    let ours = arcade_chain::Block::parse_with(&raw_ours, &p.format).ok()?;
    let known: BTreeSet<Hash256> = ours.txs.iter().map(|t| t.txid).collect();
    // Where each of their transactions starts: after the header (and AuxPoW) and the count, back to back.
    let txs_bytes: u64 = theirs.txs.iter().map(|t| t.raw.len() as u64).sum();
    let mut off = start + body.len() as u64 - txs_bytes;
    for t in &theirs.txs {
        let len = t.raw.len() as u64;
        if !known.contains(&t.txid) {
            if len > (LEAVES_PER_RUN - 1) * DATA_LEAF as u64 {
                eprintln!("arc: {} block {height}'s transaction {} isn't ours but is too big to challenge", p.name, t.txid.to_hex());
                return None;
            }
            let header = bytes_from(peer, seg, &e.root, e.bytes, start, 80)?;
            let tx = bytes_from(peer, seg, &e.root, e.bytes, off, len)?;
            let mut c = TxChallenge { segment: *seg, height, row, header, at: off, len, tx, challenger: node.identity.verifying_key().to_bytes(), sig: [0; 64] };
            let net = crate::arcsvc::svc()?.net();
            c.sig = block::sign(&node.identity, &c.message(&net));
            eprintln!("arc: {} block {height} in the listed segment holds transaction {} that isn't in it: challenged", p.name, t.txid.to_hex());
            crate::arcseg::add(Item::TxChallenge(Box::new(c)));
            return Some(());
        }
        off += len;
    }
    None
}

/// Demand `count` leaves of a listed segment from `first` on the chain (§2.4), signed by this arcade.
pub fn demand(node: &Node, net: &Net, segment: &Hash, first: u64, count: u64) -> Result<Hash, String> {
    let mut d = RevealDemand { segment: *segment, first, count, demander: node.identity.verifying_key().to_bytes(), sig: [0; 64] };
    d.sig = block::sign(&node.identity, &d.message(net));
    let id = d.id(net);
    crate::arcseg::add(Item::RevealDemand(d));
    Ok(id)
}

/// Answer the open challenges and reveal demands against segments this arcade holds (§2.4): for a challenge, the
/// transaction of its own block that covers the challenged byte, with its Merkle path; for a demand, the leaves from
/// its sealed copy with their path.
pub fn answer(node: &Node, net: &Net, reg: &arcade_arc::registry::Registry) {
    let waiting = crate::arcseg::waiting();
    let me = node.identity.verifying_key().to_bytes();
    for r in crate::arcseal::replicas(node, net) {
        for (id, first, count) in reg.demands_on(&r.segment) {
            if waiting.iter().any(|i| matches!(i, Item::RevealAnswer(a) if a.demand == id)) {
                continue;
            }
            let leaves: Option<Vec<[u8; DATA_LEAF]>> = (first..first + count).map(|i| r.data_leaf(&me, i).ok()).collect();
            let all: Option<Vec<Hash>> = std::fs::read(r.dir.join("data.hashes")).ok().map(|b| b.chunks_exact(32).map(|c| c.try_into().expect("32 bytes")).collect());
            match (leaves, all.and_then(|a| merkle::run_path(&a, first, count).ok())) {
                (Some(leaves), Some(path)) => {
                    eprintln!("arc: answered a reveal demand for {count} leaves of a segment this arcade holds");
                    crate::arcseg::add(Item::RevealAnswer(RevealAnswer { demand: id, run: Run { first, leaves, path } }));
                }
                _ => {
                    if UNANSWERED.lock().unwrap().insert(id) {
                        eprintln!("arc: a reveal demand this arcade can't answer from its sealed copy");
                    }
                }
            }
        }
        let open = reg.challenges_on(&r.segment);
        if open.is_empty() {
            continue;
        }
        let Some(p) = arcade_chain::params::by_key(&r.chain) else { continue };
        for (id, height, at) in open {
            if waiting.iter().any(|i| matches!(i, Item::TxAnswer(a) if a.challenge == id)) {
                continue;
            }
            match answer_one(node, p, &r, id, height, at) {
                Some(a) => {
                    eprintln!("arc: answered a challenge to {} block {height}", p.name);
                    crate::arcseg::add(Item::TxAnswer(a));
                }
                None => {
                    if UNANSWERED.lock().unwrap().insert(id) {
                        eprintln!("arc: a challenge to {} block {height} that this arcade can't answer", p.name);
                    }
                }
            }
        }
    }
}

fn answer_one(node: &Node, p: &arcade_chain::params::ChainParams, r: &crate::arcseal::Replica, id: Hash, height: u64, at: u64) -> Option<TxAnswer> {
    // Our segment, as it is listed: the blocks, then the table.
    let mut data = Vec::new();
    let mut sizes = Vec::new();
    for h in r.first..=r.last {
        let u = crate::arcseg::unit_bytes(node, p, r.family, h as u32)?;
        sizes.push(u.len() as u64);
        data.extend(u);
    }
    let start: u64 = sizes[..(height - r.first) as usize].iter().sum();
    data.extend(segment::table(&sizes));
    let raw = &data[start as usize..(start + sizes[(height - r.first) as usize]) as usize];
    let b = arcade_chain::Block::parse_with(raw, &p.format).ok()?;
    let txs_bytes: u64 = b.txs.iter().map(|t| t.raw.len() as u64).sum();
    let mut off = start + raw.len() as u64 - txs_bytes;
    let ids: Vec<Hash256> = b.txs.iter().map(|t| t.txid).collect();
    for (i, t) in b.txs.iter().enumerate() {
        let len = t.raw.len() as u64;
        if off <= at && at < off + len {
            let (first, n) = segment::leaves_of(off, len);
            let run = Run::of(&data, first, n).ok()?;
            let path = merkle_path(&ids, i);
            return Some(TxAnswer { challenge: id, at: off, len, tx: run, index: i as u64, path });
        }
        off += len;
    }
    None
}

/// A transaction's Merkle path in its block (Bitcoin's tree: the last node doubled on odd levels).
pub fn merkle_path(ids: &[Hash256], mut i: usize) -> Vec<Hash> {
    let (mut level, mut path) = (ids.to_vec(), Vec::new());
    while level.len() > 1 {
        if level.len() % 2 == 1 {
            level.push(*level.last().expect("not empty"));
        }
        path.push(level[i ^ 1].0);
        level = level.chunks(2).map(|x| arcade_chain::hash::sha256d(&[x[0].0, x[1].0].concat())).collect();
        i /= 2;
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_block_holding_a_byte_is_found_by_the_table() {
        // Blocks of 100, 50, 200 and 10 bytes from height 7: they start at 0, 100, 150, 350.
        let starts = [0u64, 100, 150, 350];
        let row = |h: u64| starts.get((h - 7) as usize).copied();
        assert_eq!(height_holding(7, 10, 0, row), Some(7));
        assert_eq!(height_holding(7, 10, 99, row), Some(7));
        assert_eq!(height_holding(7, 10, 100, row), Some(8));
        assert_eq!(height_holding(7, 10, 349, row), Some(9));
        assert_eq!(height_holding(7, 10, 355, row), Some(10));
        // A path made here checks with the chain's own branch walk, at every position of odd and even trees.
        for n in [1usize, 2, 3, 5, 8] {
            let ids: Vec<Hash256> = (0..n).map(|i| Hash256([i as u8 + 1; 32])).collect();
            let root = arcade_chain::hash::merkle_root(&ids);
            for i in 0..n {
                let path: Vec<Hash256> = merkle_path(&ids, i).into_iter().map(Hash256).collect();
                assert_eq!(arcade_chain::hash::merkle_branch_root(ids[i], &path, i as u32), root, "{n} {i}");
            }
        }
        let h = |b: u8| [b; 32];
        assert_eq!(first_difference(&[h(1), h(2), h(3)], &[h(1), h(2), h(3)]), None);
        assert_eq!(first_difference(&[h(1), h(2), h(3)], &[h(1), h(9), h(3)]), Some(1));
        assert_eq!(first_difference(&[h(1), h(2)], &[h(1), h(2), h(3)]), Some(2));
    }
}
