//! The genesis bulletin (docs/ARCADECHAIN.md §10): the list block 0 is made from, kept over the mesh before there is
//! a chain. The rules of the list are arcade-arc `genesis.rs`; this is the arcade's side of it.
//!
//! - **One bulletin per arcade**, of records: items (catalog entries, registration commits, proofs, audit commits,
//!   openings, in arcade-arc's item bytes), the assembler's **receipts** for them, and its signed **closed** hash. New
//!   records are passed on once over every link ("gb" frames) and kept in `<home>/arc/genesis-<net>/board.bin`.
//! - **The assembler** (named in the settings) gives a receipt only for an item that reached it in its window. Everyone
//!   builds the closed list and the final list from **receipted items only**, so every arcade gets the same lists, and
//!   a receipt for an item missing from someone's list proves it was left out.
//! - **Taking part follows Earn ARCADE:** an arcade with the switch on for a listed chain it keeps seals that chain's
//!   segments, posts their catalog entries and one commit of its replicas while the list is open, then answers both
//!   beacons from its sealed copies by itself.
//! - **The final list** is written to `final.json` beside the board once the last window closes: the network's block 0
//!   is made from it (its hash is `Genesis::final_list`).
//!
//! Settings (`--arc-genesis <file>`):
//!
//! ```text
//! {"net": "arctest", "assembler": "<node id>", "segment_bytes": 16777216,
//!  "opens": t, "closes": t, "beacon_1": t, "answers_1_by": t, "beacon_2": t, "answers_2_by": t,   (UTC seconds)
//!  "beacon_chain": "btc", "beacon_depth": 6,
//!  "fixed_beacons": ["<hex>", "<hex>"]}            (closed test networks only: no chain to take them from)
//! ```

use crate::node::Node;
use arcade_arc::block::{self, AuditCommit, AuditOpening, CatalogEntry, Item, Key, RegCommit, RegProof, Sig};
use arcade_chain::Reader;
use arcade_arc::catalog::Catalog;
use arcade_arc::genesis::{self, Answers, ClosedList, FinalList, Struck};
use arcade_arc::merkle::{self, Hash};
use arcade_arc::params::{self, Net};
use arcade_arc::{draw, registry, tagged};
use arcade_mesh::link::{Frame, Link, LinkError};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub struct Settings {
    pub net: Net,
    pub assembler: Key,
    pub segment_bytes: u64,
    pub opens: u64,
    pub closes: u64,
    pub beacon_1: u64,
    pub answers_1_by: u64,
    pub beacon_2: u64,
    pub answers_2_by: u64,
    pub beacon_chain: String,
    pub beacon_depth: u32,
    pub fixed_beacons: Option<(Hash, Hash)>,
    /// Block 0's time and first difficulty, its `clock` ("vdf"), and the rest of the chain settings (`"block_0"`): with them the final step
    /// also writes `block0.json`, chain settings to start the network from (`--arc-local`, or built into a release).
    pub block_0: Option<Value>,
}

static SET: OnceLock<Settings> = OnceLock::new();

fn hex32(v: &Value) -> Option<[u8; 32]> {
    v.as_str().and_then(|s| hex::decode(s).ok()).and_then(|b| b.try_into().ok())
}

impl Settings {
    pub fn parse(text: &str) -> Result<Settings, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| format!("genesis settings: {e}"))?;
        let net = match v["net"].as_str() {
            Some("arctest") => params::TESTNET,
            Some("arc") => params::MAINNET,
            _ => return Err("genesis settings: net must be \"arc\" or \"arctest\"".into()),
        };
        let t = |k: &str| v[k].as_u64().ok_or(format!("genesis settings: {k} (UTC seconds) missing"));
        let s = Settings {
            block_0: v.get("block_0").filter(|b| b.is_object()).cloned(),
            net,
            assembler: hex32(&v["assembler"]).ok_or("genesis settings: assembler must be an arcade id")?,
            segment_bytes: v["segment_bytes"].as_u64().unwrap_or(params::SEGMENT_BYTES),
            opens: t("opens")?,
            closes: t("closes")?,
            beacon_1: t("beacon_1")?,
            answers_1_by: t("answers_1_by")?,
            beacon_2: t("beacon_2")?,
            answers_2_by: t("answers_2_by")?,
            beacon_chain: v["beacon_chain"].as_str().unwrap_or("btc").to_string(),
            beacon_depth: v["beacon_depth"].as_u64().unwrap_or(6) as u32,
            fixed_beacons: match &v["fixed_beacons"] {
                Value::Null => None,
                b => Some((hex32(&b[0]).ok_or("genesis settings: fixed_beacons are two hashes")?, hex32(&b[1]).ok_or("genesis settings: fixed_beacons are two hashes")?)),
            },
        };
        let order = [s.opens, s.closes, s.beacon_1, s.answers_1_by, s.beacon_2, s.answers_2_by];
        if order.windows(2).any(|w| w[1] <= w[0]) {
            return Err("genesis settings: the times must rise: opens, closes, beacon_1, answers_1_by, beacon_2, answers_2_by".into());
        }
        Ok(s)
    }
}

/// Where the list stands at `now`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Before,
    Open,
    /// Closed, waiting for beacon 1.
    Closed,
    /// Beacon 1 known: proofs and audit commits.
    Answers1,
    /// Waiting for beacon 2.
    Between,
    /// Beacon 2 known: openings.
    Answers2,
    Final,
}

fn phase(s: &Settings, now: u64, b1: bool, b2: bool) -> Phase {
    if now < s.opens {
        Phase::Before
    } else if now < s.closes {
        Phase::Open
    } else if now < s.answers_1_by {
        if now >= s.beacon_1 && b1 { Phase::Answers1 } else { Phase::Closed }
    } else if now < s.answers_2_by {
        if now >= s.beacon_2 && b2 { Phase::Answers2 } else { Phase::Between }
    } else {
        Phase::Final
    }
}

/// One record of the bulletin.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Record {
    Item(Item),
    /// The assembler's receipt for an item (by its id).
    Receipt(Hash, Sig),
    /// The assembler's closed hash.
    Closed(Hash, Sig),
}

const R_ITEM: u8 = 0;
const R_RECEIPT: u8 = 1;
const R_CLOSED: u8 = 2;

impl Record {
    fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        let kind = match self {
            Record::Item(i) => {
                i.encode(&mut body);
                R_ITEM
            }
            Record::Receipt(id, sig) => {
                body.extend_from_slice(id);
                body.extend_from_slice(sig);
                R_RECEIPT
            }
            Record::Closed(h, sig) => {
                body.extend_from_slice(h);
                body.extend_from_slice(sig);
                R_CLOSED
            }
        };
        let mut out = vec![kind];
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// The records in `blob`, as far as they read.
    fn decode_all(blob: &[u8]) -> Vec<Record> {
        let mut out = Vec::new();
        let mut at = 0usize;
        while at + 5 <= blob.len() {
            let kind = blob[at];
            let len = u32::from_le_bytes(blob[at + 1..at + 5].try_into().expect("4 bytes")) as usize;
            let Some(body) = blob.get(at + 5..at + 5 + len) else { break };
            at += 5 + len;
            let r = match kind {
                R_ITEM => Item::decode(&mut Reader::new(body)).ok().map(Record::Item),
                R_RECEIPT | R_CLOSED if body.len() == 96 => {
                    let (h, s): ([u8; 32], [u8; 64]) = (body[..32].try_into().expect("32"), body[32..].try_into().expect("64"));
                    Some(if kind == R_RECEIPT { Record::Receipt(h, s) } else { Record::Closed(h, s) })
                }
                _ => None,
            };
            match r {
                Some(r) => out.push(r),
                None => break,
            }
        }
        out
    }
}

/// An item's id on the bulletin (what a receipt names).
pub fn item_id(net: &Net, item: &Item) -> Hash {
    let mut b = Vec::new();
    item.encode(&mut b);
    tagged::hash(net, "genesis item", &[&b])
}

fn receipt_message(net: &Net, id: &Hash) -> Vec<u8> {
    tagged::message(net, "genesis receipt", &[id])
}

fn closed_message(net: &Net, h: &Hash) -> Vec<u8> {
    tagged::message(net, "genesis closed", &[h])
}

#[derive(Default)]
struct Board {
    /// Every record, in the order it was taken in (each link is sent the ones after the last it got).
    records: Vec<Vec<u8>>,
    seen: HashSet<Vec<u8>>,
    items: BTreeMap<Hash, Item>,
    receipts: HashMap<Hash, Sig>,
    closed: Option<(Hash, Sig)>,
    told: HashMap<String, usize>,
    file: Option<PathBuf>,
}

static BOARD: LazyBoard = LazyBoard(OnceLock::new());
struct LazyBoard(OnceLock<Mutex<Board>>);
impl LazyBoard {
    fn get(&self) -> std::sync::MutexGuard<'_, Board> {
        self.0.get_or_init(|| Mutex::new(Board::default())).lock().unwrap()
    }
}

/// Most records a bulletin keeps (a commit, proof and audits per owner: room for many thousands of arcades).
const MAX_RECORDS: usize = 1_000_000;

/// Take a record in: checked as far as it can be on its own, kept, saved, passed on. Whether it was new.
fn take(rec: Record) -> bool {
    let Some(s) = SET.get() else { return false };
    let net = &s.net;
    let ok = match &rec {
        Record::Item(i) => match i {
            Item::CatalogEntry(e) => check_entry(s, e),
            Item::RegCommit(c) => genesis::check_commit(net, c).is_ok(),
            // Checked in full against the closed list and the beacons when the final list is made.
            Item::RegProof(_) | Item::AuditCommit(_) | Item::AuditOpening(_) => true,
            _ => false,
        },
        Record::Receipt(id, sig) => block::verify(&s.assembler, &receipt_message(net, id), sig),
        Record::Closed(h, sig) => block::verify(&s.assembler, &closed_message(net, h), sig),
    };
    if !ok {
        return false;
    }
    let bytes = rec.encode();
    let mut b = BOARD.get();
    if b.records.len() >= MAX_RECORDS || !b.seen.insert(bytes.clone()) {
        return false;
    }
    match &rec {
        Record::Item(i) => {
            b.items.insert(item_id(net, i), i.clone());
        }
        Record::Receipt(id, sig) => {
            b.receipts.insert(*id, *sig);
        }
        Record::Closed(h, sig) => b.closed = Some((*h, *sig)),
    }
    if let Some(f) = &b.file {
        if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(f) {
            let _ = out.write_all(&bytes);
        }
    }
    b.records.push(bytes);
    true
}

fn check_entry(s: &Settings, e: &CatalogEntry) -> bool {
    let least = (e.last - e.first + 1).checked_mul(arcade_arc::segment::ROW).and_then(|t| t.checked_add(s.segment_bytes));
    s.net.is_listed_at(&e.chain, 0)
        && arcade_arc::segment::family_at(&s.net, e.family, 0)
        && arcade_arc::segment::whole_ranges(e.first, e.last)
        && least.is_some_and(|l| e.bytes >= l)
        && block::verify(&e.poster, &e.message(&s.net), &e.sig)
}

/// The window an item kind is received in, by the assembler's clock.
fn in_window(s: &Settings, item: &Item, ph: Phase) -> bool {
    match item {
        Item::CatalogEntry(_) | Item::RegCommit(_) => ph == Phase::Open,
        Item::RegProof(_) | Item::AuditCommit(_) => ph == Phase::Answers1,
        Item::AuditOpening(_) => ph == Phase::Answers2,
        _ => {
            let _ = s;
            false
        }
    }
}

/// Post an item of our own: on our bulletin (and so to every link), receipted at once if we are the assembler.
fn post(node: &Node, item: Item) {
    let s = SET.get().expect("set");
    let id = item_id(&s.net, &item);
    if take(Record::Item(item)) {
        eprintln!("arc genesis: posted item {}", hex::encode(&id[..8]));
    }
    assemble(node);
}

/// As the assembler: a receipt for every item on the board that came in its window and has none yet; the closed hash
/// once the list closes.
fn assemble(node: &Node) {
    let s = SET.get().expect("set");
    if node.identity.verifying_key().to_bytes() != s.assembler {
        return;
    }
    let ph = current_phase(node);
    let todo: Vec<Hash> = {
        let b = BOARD.get();
        b.items.iter().filter(|(id, i)| !b.receipts.contains_key(*id) && in_window(s, i, ph)).map(|(id, _)| *id).collect()
    };
    for id in todo {
        let sig = block::sign(&node.identity, &receipt_message(&s.net, &id));
        take(Record::Receipt(id, sig));
    }
    let closing = arcade_app::now() >= s.closes && BOARD.get().closed.is_none();
    if closing {
        let mut l = closed_list();
        // Rehearsals of the correction (§15 step 16): an assembler that leaves a receipted commit out.
        if std::env::var_os("ARCADE_GENESIS_TEST_LEAVE_OUT").is_some() {
            let me = node.identity.verifying_key().to_bytes();
            if let Some(at) = l.commits.iter().rposition(|c| c.owner != me) {
                let c = l.commits.remove(at);
                eprintln!("arc genesis: (test) leaving out the receipted commit of {}", hex::encode(&c.owner[..8]));
            }
        }
        let h = l.hash(&s.net);
        let sig = block::sign(&node.identity, &closed_message(&s.net, &h));
        take(Record::Closed(h, sig));
        eprintln!("arc genesis: the list closed: {}", hex::encode(h));
    }
}

/// The closed list's hash, as every arcade makes it from the receipted items on its bulletin (§10, §15 step 16). The
/// assembler's signed closed hash is only a check: a receipt proves its item belongs in the list, so a list the
/// assembler closed without a receipted item is corrected here, by everyone alike, and the seeds, the final list and
/// block 0 all follow the corrected one.
fn closed_hash(net: &Net) -> Hash {
    closed_list().hash(net)
}

/// The closed list: receipted catalog entries and commits.
fn closed_list() -> ClosedList {
    let b = BOARD.get();
    let mut l = ClosedList::default();
    for (id, i) in &b.items {
        if !b.receipts.contains_key(id) {
            continue;
        }
        match i {
            Item::CatalogEntry(e) => l.entries.push(e.clone()),
            Item::RegCommit(c) => l.commits.push(c.clone()),
            _ => {}
        }
    }
    l
}

/// The beacons as far as they are known: fixed (test networks), or the first block of the beacon chain with a header
/// time at or after the beacon's moment, once `beacon_depth` blocks deep.
fn beacons(node: &Node) -> (Option<Hash>, Option<Hash>) {
    let s = SET.get().expect("set");
    if let Some((a, b)) = s.fixed_beacons {
        let now = arcade_app::now();
        return ((now >= s.beacon_1).then_some(a), (now >= s.beacon_2).then_some(b));
    }
    let Some(p) = arcade_chain::params::by_key(&s.beacon_chain) else { return (None, None) };
    let Ok(Some(tip)) = node.chain.tip(p.chain_no) else { return (None, None) };
    let time_at = |h: u32| -> Option<u64> {
        let id = node.chain.hash_at(p.chain_no, h).ok().flatten()?;
        Some(node.chain.header(p.chain_no, &id).ok().flatten()?.header.time as u64)
    };
    let find = |t: u64| -> Option<Hash> {
        // The first height whose time is at or after t (times are near enough to rising: a binary search, then a walk back).
        let (mut lo, mut hi) = (0u32, tip.height);
        if time_at(hi)? < t {
            return None;
        }
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if time_at(mid)? >= t { hi = mid } else { lo = mid + 1 }
        }
        while lo > 0 && time_at(lo - 1)? >= t {
            lo -= 1;
        }
        if tip.height < lo + s.beacon_depth {
            return None;
        }
        Some(node.chain.hash_at(p.chain_no, lo).ok().flatten()?.0)
    };
    let now = arcade_app::now();
    ((now >= s.beacon_1).then(|| find(s.beacon_1)).flatten(), (now >= s.beacon_2).then(|| find(s.beacon_2)).flatten())
}

fn current_phase(node: &Node) -> Phase {
    let s = SET.get().expect("set");
    let (b1, b2) = beacons(node);
    phase(s, arcade_app::now(), b1.is_some(), b2.is_some())
}

/// Every owner's receipted answers, gathered.
fn answers() -> BTreeMap<Key, Answers> {
    let s = SET.get().expect("set");
    let b = BOARD.get();
    let mut proofs: BTreeMap<Key, RegProof> = BTreeMap::new();
    let mut commits: BTreeMap<Hash, AuditCommit> = BTreeMap::new();
    let mut opens: BTreeMap<Hash, AuditOpening> = BTreeMap::new();
    for (id, i) in &b.items {
        if !b.receipts.contains_key(id) {
            continue;
        }
        match i {
            Item::RegProof(p) => {
                proofs.entry(p.owner).or_insert_with(|| p.clone());
            }
            Item::AuditCommit(c) => {
                commits.entry(c.replica).or_insert_with(|| c.clone());
            }
            Item::AuditOpening(o) => {
                opens.entry(o.replica).or_insert_with(|| o.clone());
            }
            _ => {}
        }
    }
    let closed = {
        let mut l = ClosedList::default();
        for (id, i) in &b.items {
            if let (true, Item::RegCommit(c)) = (b.receipts.contains_key(id), i) {
                l.commits.push(c.clone());
            }
        }
        l
    };
    drop(b);
    let mut out = BTreeMap::new();
    for c in &closed.commits {
        let Some(proof) = proofs.get(&c.owner) else { continue };
        let audits = c
            .replicas
            .iter()
            .filter_map(|(seg, _)| {
                let id = block::replica_id(&s.net, seg, &c.owner);
                Some((commits.get(&id)?.clone(), opens.get(&id)?.clone()))
            })
            .collect();
        out.insert(c.owner, Answers { proof: proof.clone(), audits });
    }
    out
}

/// The final list as this arcade makes it from its bulletin (once both beacons are known).
pub fn final_list(node: &Node) -> Option<(FinalList, Vec<Struck>)> {
    let s = SET.get()?;
    let (Some(b1), Some(b2)) = beacons(node) else { return None };
    Some(genesis::finalize(&s.net, s.segment_bytes, &closed_list(), &b1, &b2, &answers()))
}

// ---------------- this arcade taking part ----------------

/// What this arcade has posted, so it posts each thing once.
#[derive(Default)]
struct Mine {
    entries: HashSet<Hash>,
    committed: bool,
    answered_1: bool,
    answered_2: bool,
    /// Audit leaves made after beacon 1, kept for the openings.
    leaves: HashMap<Hash, Vec<Hash>>,
}

static MINE: Mutex<Option<Mine>> = Mutex::new(None);
/// Why this arcade isn't taking part as it should, for arc.genesis (empty when all is well).
static NOTE: Mutex<String> = Mutex::new(String::new());

fn take_part(node: &Arc<Node>) {
    let s = SET.get().expect("set");
    let net = s.net;
    let me = node.identity.verifying_key().to_bytes();
    let ph = current_phase(node);
    let mut guard = MINE.lock().unwrap();
    let mine = guard.get_or_insert_with(Mine::default);
    // Seal what Earn ARCADE is on for, before the list closes (the same sealing as on the chain; block 0's listed chains).
    if matches!(ph, Phase::Before | Phase::Open) {
        crate::arcseg::auto_seal(node, &net, &Catalog::new(net, s.segment_bytes), 0);
    }
    let replicas: Vec<crate::arcseal::Replica> = crate::arcseal::replicas(node, &net).into_iter().filter(|r| crate::arcseg::earn_on(node, &r.chain)).collect();
    match ph {
        Phase::Open => {
            for r in &replicas {
                if mine.entries.contains(&r.segment) {
                    continue;
                }
                let mut e = CatalogEntry { chain: r.chain.clone(), family: r.family, first: r.first, last: r.last, bytes: r.bytes, manifest: [0; 32], root: r.root, poster: me, sig: [0; 64] };
                e.sig = block::sign(&node.identity, &e.message(&net));
                if e.segment(&net) == r.segment {
                    post(node, Item::CatalogEntry(e));
                    mine.entries.insert(r.segment);
                }
            }
            // The commit once sealing has caught up (nothing sealing now), or in the last tenth of the window regardless.
            let late = arcade_app::now() + (s.closes - s.opens) / 10 >= s.closes;
            if !mine.committed && !replicas.is_empty() && (crate::arcseal::sealing().is_none() || late) {
                let Some(payout) = payout(node, &net) else {
                    *NOTE.lock().unwrap() = "no wallet here: make one (or restore it) so this arcade has an address to be paid at".into();
                    return;
                };
                NOTE.lock().unwrap().clear();
                let mut c = RegCommit { owner: me, payout, replicas: replicas.iter().map(|r| (r.segment, r.replica_root)).take(block::MAX_BATCH).collect(), anchor: 0, nonce: 0, sig: [0; 64] };
                c.finish(&net, &genesis::list_anchor(&net), registry::STAMP_BITS, &node.identity);
                eprintln!("arc genesis: committing {} replicas", c.replicas.len());
                post(node, Item::RegCommit(c));
                mine.committed = true;
            }
        }
        Phase::Answers1 if !mine.answered_1 => {
            let (Some(b1), _) = beacons(node) else { return };
            let Some(c) = my_commit(&me) else { return };
            let closed = closed_hash(&net);
            let pseed = genesis::proof_seed(&net, &closed, &b1);
            let held: Option<Vec<&crate::arcseal::Replica>> = c.replicas.iter().map(|(seg, _)| replicas.iter().find(|r| r.segment == *seg)).collect();
            let Some(held) = held else { return eprintln!("arc genesis: a replica of our commit isn't here any more") };
            let counts: Vec<u64> = held.iter().map(|r| r.leaves).collect();
            let mut opened = Vec::new();
            for (at, i) in registry::picks(&net, &pseed, &me, &counts) {
                match held[at].open(&me, &[i]) {
                    Ok(mut o) => opened.append(&mut o),
                    Err(e) => return eprintln!("arc genesis: opening a replica: {e}"),
                }
            }
            post(node, Item::RegProof(RegProof { owner: me, leaves: opened }));
            for r in &held {
                let id = block::replica_id(&net, &r.segment, &me);
                let Ok(leaves) = crate::arcaudit::audit_leaves(&net, r, &pseed) else { continue };
                let Ok(root) = merkle::root(&leaves) else { continue };
                let mut ac = AuditCommit { replica: id, pick: 0, root, sig: [0; 64] };
                ac.sig = block::sign(&node.identity, &ac.message(&net, &genesis::audit_pick_id(&pseed)));
                post(node, Item::AuditCommit(ac));
                mine.leaves.insert(id, leaves);
            }
            mine.answered_1 = true;
        }
        Phase::Answers2 if !mine.answered_2 => {
            let (_, Some(b2)) = beacons(node) else { return };
            let Some(c) = my_commit(&me) else { return };
            let closed = closed_hash(&net);
            let oseed = genesis::open_seed(&net, &closed, &b2);
            for (seg, _) in &c.replicas {
                let Some(r) = replicas.iter().find(|r| r.segment == *seg) else { continue };
                let id = block::replica_id(&net, seg, &me);
                let Some(leaves) = mine.leaves.get(&id) else { continue };
                let at: Vec<u64> = (0..params::AUDIT_OPENINGS as u32).map(|k| draw::leaf_pick(&net, &oseed, &id, k, r.leaves)).collect();
                let Ok(opened) = r.open(&me, &at) else { continue };
                let paths: Result<Vec<Vec<Hash>>, _> = at.iter().map(|&i| merkle::path(leaves, i)).collect();
                let Ok(paths) = paths else { continue };
                post(node, Item::AuditOpening(AuditOpening { replica: id, leaves: opened.into_iter().zip(paths).collect() }));
            }
            mine.answered_2 = true;
        }
        Phase::Final => {
            let dir = board_dir(node, &net);
            let out = dir.join("final.json");
            if !out.exists() {
                if let Some((list, struck)) = final_list(node) {
                    let fj = final_json(&net, &list, &struck);
                    let _ = std::fs::write(&out, fj.to_string());
                    // Mainnet starts from a named code release only (§15 step 17).
                    let code_missing = net == params::MAINNET && s.block_0.as_ref().is_some_and(|b| b["code"].as_str().is_none_or(|c| c.len() != 64));
                    if code_missing {
                        *NOTE.lock().unwrap() = "block 0 isn't written: the settings' block_0.code doesn't name the first code release".into();
                    }
                    if let (Some(b0), (Some(b1), Some(b2)), false) = (&s.block_0, beacons(node), code_missing) {
                        let closed = closed_hash(&net);
                        let mut chain = b0.clone();
                        chain["net"] = json!(net.key);
                        chain["genesis"] = json!({"time": b0["time"], "difficulty": b0["difficulty"], "closed_list": hex::encode(closed),
                            "final_list": hex::encode(list.hash(&net)), "beacon_1": hex::encode(b1), "beacon_2": hex::encode(b2)});
                        // Mainnet: block 0 names the first code release (§15 step 17), from the settings' block_0.
                        if let Some(code) = b0["code"].as_str() {
                            chain["genesis"]["code"] = json!(code);
                        }
                        chain["segment_bytes"] = json!(s.segment_bytes);
                        chain["list"] = json!({"entries": fj["entries"], "owners": fj["owners"]});
                        // In block_0 "time" is block 0's moment; in chain settings it names the clock ("clock" here).
                        if let Some(o) = chain.as_object_mut() {
                            o.remove("difficulty");
                            match o.remove("clock") {
                                Some(c) => o.insert("time".into(), c),
                                None => o.remove("time"),
                            };
                        }
                        let _ = std::fs::write(dir.join("block0.json"), chain.to_string());
                    }
                    eprintln!("arc genesis: final list {} ({} owners, {} entries, {} struck) written to {}", hex::encode(list.hash(&net)), list.owners.len(), list.entries.len(), struck.len(), out.display());
                }
            }
        }
        _ => {}
    }
}

fn my_commit(me: &Key) -> Option<RegCommit> {
    let b = BOARD.get();
    b.items.iter().find_map(|(id, i)| match i {
        Item::RegCommit(c) if c.owner == *me && b.receipts.contains_key(id) => Some(c.clone()),
        _ => None,
    })
}

/// This arcade's address on the network (its wallet's first key on the home chain), as a payout.
fn payout(node: &Node, net: &Net) -> Option<block::Payout> {
    crate::arcseg::payout_of(node, net, node.params)
}

fn final_json(net: &Net, list: &FinalList, struck: &[Struck]) -> Value {
    json!({
        "final_list": hex::encode(list.hash(net)),
        "entries": list.entries.iter().map(|e| {
            let mut b = Vec::new();
            Item::CatalogEntry(e.clone()).encode(&mut b);
            json!({"segment": hex::encode(e.segment(net)), "chain": e.chain, "family": e.family, "first": e.first, "last": e.last, "item": hex::encode(b)})
        }).collect::<Vec<_>>(),
        "owners": list.owners.iter().map(|o| json!({"owner": hex::encode(o.owner), "payout": hex::encode(o.payout),
            "replicas": o.replicas.iter().map(|(s, r)| [hex::encode(s), hex::encode(r)]).collect::<Vec<_>>()})).collect::<Vec<_>>(),
        "struck": struck.iter().map(|s| json!({"owner": hex::encode(s.owner), "segment": s.segment.map(hex::encode), "why": s.why})).collect::<Vec<_>>(),
    })
}

fn board_dir(node: &Node, net: &Net) -> PathBuf {
    node.home.join("arc").join(format!("genesis-{}", net.key))
}

// ---------------- start, mesh, API ----------------

/// Whether a genesis list is kept here.
pub fn active() -> bool {
    SET.get().is_some()
}

/// `--arc-genesis <settings>`: keep the bulletin of that list, and take part in it.
pub fn start(node: &Arc<Node>, settings: &std::path::Path) -> Result<(), String> {
    let text = std::fs::read_to_string(settings).map_err(|e| format!("{}: {e}", settings.display()))?;
    let s = Settings::parse(&text)?;
    let dir = board_dir(node, &s.net);
    let _ = SET.set(s);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join("board.bin");
    let saved = std::fs::read(&file).unwrap_or_default();
    BOARD.get().file = None; // replaying: not written again
    let mut n = 0;
    for r in Record::decode_all(&saved) {
        n += take(r) as usize;
    }
    BOARD.get().file = Some(file);
    eprintln!("arc genesis: bulletin of {} with {n} records", SET.get().expect("set").net.key);
    let node = node.clone();
    std::thread::Builder::new()
        .name("arc genesis".into())
        .spawn(move || loop {
            assemble(&node);
            take_part(&node);
            std::thread::sleep(Duration::from_secs(2));
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Records this link hasn't been sent.
pub fn tick(peer: &str, link: &mut Link) -> Result<(), LinkError> {
    let Some(s) = SET.get() else { return Ok(()) };
    let due: Vec<u8> = {
        let mut b = BOARD.get();
        let from = b.told.get(peer).copied().unwrap_or(0);
        if from >= b.records.len() {
            return Ok(());
        }
        // At most ~1 MB a frame; the rest next time.
        let mut out = Vec::new();
        let mut to = from;
        while to < b.records.len() && out.len() < (1 << 20) {
            out.extend_from_slice(&b.records[to]);
            to += 1;
        }
        b.told.insert(peer.to_string(), to);
        out
    };
    let mut f = Frame::new(json!({"t": "gb", "op": "r", "net": s.net.key}));
    f.blob = due;
    link.send(&f)
}

/// A "gb" frame: records from a peer.
pub fn on_frame(node: &Node, f: &Frame) {
    let Some(s) = SET.get() else { return };
    if f.head["net"] != s.net.key {
        return;
    }
    let mut new = 0;
    for r in Record::decode_all(&f.blob) {
        new += take(r) as usize;
    }
    if new > 0 {
        assemble(node);
    }
}

/// `arc.genesis`: where the list stands here.
pub fn status(node: &Node) -> Result<Value, String> {
    let s = SET.get().ok_or("no genesis list here (start with --arc-genesis)")?;
    let (b1, b2) = beacons(node);
    let ph = current_phase(node);
    let me = node.identity.verifying_key().to_bytes();
    let closed = closed_list();
    let b = BOARD.get();
    let mine_receipted = b.items.iter().filter(|(id, i)| b.receipts.contains_key(*id) && matches!(i, Item::RegCommit(c) if c.owner == me)).count();
    let assembler_closed = b.closed.map(|x| hex::encode(x.0));
    let (records, items, receipts) = (b.records.len(), b.items.len(), b.receipts.len());
    drop(b);
    let fin = (ph == Phase::Final).then(|| final_list(node)).flatten();
    Ok(json!({
        "net": s.net.key, "phase": format!("{ph:?}"), "assembler": hex::encode(s.assembler), "i_am_assembler": me == s.assembler,
        "records": records, "items": items, "receipts": receipts,
        "closed": {"entries": closed.entries.len(), "commits": closed.commits.len(), "hash_here": hex::encode(closed.hash(&s.net)), "hash_assembler": assembler_closed.clone(),
                   // The assembler's list left a receipted item out (or took an unreceipted one): corrected (§15 step 16).
                   "corrected": assembler_closed.is_some_and(|a| a != hex::encode(closed.hash(&s.net)))},
        "beacons": [b1.map(hex::encode), b2.map(hex::encode)],
        "my_commit_receipted": mine_receipted > 0,
        "note": NOTE.lock().unwrap().clone(),
        "final": fin.map(|(l, st)| json!({"hash": hex::encode(l.hash(&s.net)), "owners": l.owners.len(), "entries": l.entries.len(), "struck": st.len()})),
    }))
}
