//! Apps: app cards (the original's game cards, docs/compat/APPS.md) read from every installed
//! chain with the arcade ledger, the App Store over them, and the apps this arcade installed.
//!
//! - One card per app: (chain, creator, name casefolded), newest by (height, position).
//! - Removed when its creator deleted the newest card (a feed act DELETE); a newer card restores it.
//! - Opens `play` only when that page has the same creator, else the card's own page.
//! - Likes, dislikes, comments and tips on any version count for the app (home chain feed acts).
//! - Popular = hot(likes − dislikes + 0.5·comments, block time); New = newest first. Starred first.
//! - Copycats: a card that looks like an earlier card of a listed app by another creator
//!   (cards::looks_like) is hidden until the operator clears it; trusted publishers are never flagged.
//! - Installing keeps (chain, creator, name): it always opens the newest version.
//! - Cards of a chain whose ledger isn't kept here are the mesh's word: up to three independent holders are
//!   compared, and each card's creator and JSON checked against its first transaction where it can be read.
//!   One holder's card is listed with a caution (trust yellow, said again when it opens and on payment
//!   requests); a card holders disagree on, or its transaction contradicts, is not listed (red).

use crate::node::Node;
use arcade_chain::params::ChainParams;
use arcade_ledger::cards::{self, Card};
use arcade_store::KeyBuf;
use std::sync::{Arc, Mutex};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};

struct Ver {
    txid: String,
    number: u64,
    creator: String,
    height: u64,
    position: u64,
    card: Card,
    /// How far the row is vouched for: green = this arcade's own ledger, or 2+ arcades sent it alike;
    /// yellow = one arcade's word (a red row is never listed).
    trust: crate::vouch::Level,
    /// Arcades that sent it (0: this arcade's own ledger).
    holders: usize,
    /// Its creator and card are known to be the chain's: this arcade's own ledger, or checked against
    /// the card's first transaction. Only such a creator counts as a trusted publisher.
    checked: bool,
}

struct App {
    chain: &'static ChainParams,
    /// Who first published it (its id goes by this, so a transfer keeps installs, permissions and allowances) and who
    /// holds it now (the last transfer's address, else the root): only the owner withdraws or transfers it.
    root: String,
    owner: String,
    versions: Vec<Ver>, // newest first; never a withdrawal card, nor a version one withdrew
    removed: bool,
    /// Withdrawn by its publisher (2026-11-21): the newest card of the creator's app is a withdrawal of the whole app.
    /// {reason, replaced_by, txid (the withdrawal card)}.
    withdrawn: Option<Value>,
}

/// One creator's cards of one app (newest first) as the store offers them (2026-11-21, Martin): withdrawal cards are
/// never versions; the versions any of them lists are hidden; and when the newest card withdraws the whole app, the app
/// is withdrawn (kept findable, not listed). None when no version is left to offer. Cards here are all the same
/// creator's (gather groups by creator), so another creator's withdrawal never touches this app.
fn resolve(vs: Vec<Ver>) -> Option<(Vec<Ver>, Option<Value>)> {
    let whole = vs.first().and_then(|n| {
        n.card.withdraw.as_ref().filter(|w| w.versions.is_empty()).map(|w| json!({"reason": w.reason, "replaced_by": w.replaced_by, "txid": n.txid}))
    });
    let hidden: BTreeSet<String> = vs.iter().filter_map(|v| v.card.withdraw.as_ref()).flat_map(|w| w.versions.iter().cloned()).collect();
    let offered: Vec<Ver> = vs.into_iter().filter(|v| v.card.withdraw.is_none() && v.card.transfer_to.is_empty() && !hidden.contains(&v.txid)).collect();
    if offered.is_empty() {
        return None;
    }
    Some((offered, whole))
}

/// One app per first publisher and name, following its transfers (2026-11-21): a transfer card in the owner's cards
/// hands the app to its `transfer_to` address, whose cards of the same name AFTER it are the app's next versions; the old
/// owner's cards after it are no longer the app's, and the new owner's from before it were never this app's. Chains of
/// transfers are followed. (root, owner, versions in any order, transfer cards included for `resolve` to leave out).
fn follow_transfers(mut by: BTreeMap<(String, String), Vec<Ver>>) -> Vec<(String, String, Vec<Ver>)> {
    let at = |v: &Ver| (v.height, v.position);
    let targets: BTreeSet<(String, String)> = by.iter()
        .flat_map(|((_, k), vs)| vs.iter().filter(|v| !v.card.transfer_to.is_empty()).map(move |v| (v.card.transfer_to.clone(), k.clone())))
        .collect();
    let mut starts: Vec<(String, String)> = by.keys().filter(|k| !targets.contains(*k)).cloned().collect();
    // A group only ever handed over is followed from its giver; one left over (its giver's chain went elsewhere) stands alone.
    let mut out = Vec::new();
    let mut walk = |by: &mut BTreeMap<(String, String), Vec<Ver>>, start: (String, String)| -> Option<(String, String, Vec<Ver>)> {
        let mut vs_all = Vec::new();
        let (mut cur, mut after) = (start.clone(), (0u64, 0u64));
        let mut owner = start.0.clone();
        let mut first = true;
        while let Some(mut vs) = by.remove(&cur) {
            vs.sort_by_key(|v| at(v));
            let t = vs.iter().find(|v| !v.card.transfer_to.is_empty() && (first || at(v) > after)).map(|v| (at(v), v.card.transfer_to.clone()));
            let until = t.as_ref().map(|x| x.0).unwrap_or((u64::MAX, u64::MAX));
            vs_all.extend(vs.into_iter().filter(|v| (first || at(v) > after) && at(v) <= until));
            owner = cur.0.clone();
            first = false;
            match t {
                Some((tat, to)) if to != cur.0 => {
                    after = tat;
                    owner = to.clone();
                    cur = (to, cur.1.clone());
                }
                _ => break,
            }
        }
        (!vs_all.is_empty()).then(|| (start.0, owner, vs_all))
    };
    starts.sort();
    for k in starts {
        if let Some(x) = walk(&mut by, k) {
            out.push(x);
        }
    }
    let rest: Vec<(String, String)> = by.keys().cloned().collect();
    for k in rest {
        if let Some(x) = walk(&mut by, k) {
            out.push(x);
        }
    }
    out
}

fn meta_get(node: &Node, k: &str) -> Value {
    node.app.store.read(|r| r.get(arcade_app::META, k.as_bytes())).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or(Value::Null)
}

fn meta_put(node: &Node, k: &str, v: &Value) -> Result<(), String> {
    node.app.store.write(|w| w.put(arcade_app::META, k.as_bytes(), v.to_string().as_bytes())).map_err(|e| e.to_string())
}

/// Chains whose arcade ledger is indexed here.
fn ledger_chains(node: &Node) -> Vec<&'static ChainParams> {
    let mut out = Vec::new();
    if crate::chains::home_indexes(node).iter().any(|i| i == "ledger") {
        out.push(node.params);
    }
    for r in node.extra.lock().unwrap().values() {
        if r.indexes.iter().any(|i| i == "ledger") {
            out.push(r.params);
        }
    }
    // The ARCADE chain keeps the arcade ledger whenever it runs here (arcsvc): its apps are in the store too.
    if let Some(p) = crate::arcsvc::defined().filter(|p| crate::arcsvc::runs(p)) {
        out.push(p);
    }
    out
}

/// The @tag of an app's creator, from the ledger of the chain the app is on. Tags live where the arcade
/// ledger does: the home chain, and the ARCADE chain (Martin 2026-11-15: an ARCADE app shows its publisher).
/// On ARCADE a tag is under the ARCADE address when claimed there, or under the same key's home-chain
/// address when the testnet's ledger snapshot carried it over; failing both, the home chain's own tag.
fn creator_tag(node: &Node, p: &ChainParams, creator: &str) -> Option<String> {
    // (A chain whose tag tables are borrowed here: lenders' rows, by key, crate::hometag.)
    creator_tag_here(node, p, creator).or_else(|| crate::hometag::of_address(node, creator))
}

fn creator_tag_here(node: &Node, p: &ChainParams, creator: &str) -> Option<String> {
    use arcade_ledger::index::tag_of;
    let home = node.ledger_home();
    // An app on the old home (Pepecoin testnet) once ARCADE is home: its creator's tag by key hash (crate::hometag).
    if p.chain_no == node.params.chain_no && home.chain_no != p.chain_no {
        return tag_of(&node.chain, p.chain_no, creator).ok().flatten().or_else(|| crate::hometag::of_address(node, creator));
    }
    if p.chain_no == home.chain_no {
        return tag_of(&node.chain, p.chain_no, creator).ok().flatten();
    }
    if !crate::arcsvc::runs(p) {
        return None;
    }
    if let Some(t) = tag_of(&node.chain, p.chain_no, creator).ok().flatten() {
        return Some(t);
    }
    let sc = arcade_ledger::address::script_for_address(p, creator).filter(|sc| sc.len() == 25 && sc[..3] == [0x76, 0xa9, 0x14])?;
    let at_home = arcade_ledger::address::base58check(home.p2pkh_version, &sc[3..23]);
    tag_of(&node.chain, p.chain_no, &at_home).ok().flatten().or_else(|| tag_of(&node.chain, home.chain_no, &at_home).ok().flatten())
}

fn app_id(chain: &str, creator: &str, key: &str) -> String {
    format!("{chain}:{creator}:{key}")
}

/// The inscription rows of chain `p` that may be app cards (HTML whose record names a game or
/// an app), from this arcade's ledger.
pub fn card_rows(node: &Node, p: &ChainParams) -> Vec<Value> {
    let c = p.chain_no;
    let rows = node.chain.store(c).read(|r| r.scan_prefix(arcade_ledger::engine::INSCRIPTION, &KeyBuf::new().u16(c).done())).unwrap_or_default();
    rows.into_iter()
        .filter_map(|(_, v)| serde_json::from_slice::<Value>(&v).ok())
        .filter(|v| v["content_type"].as_str().unwrap_or("").to_ascii_lowercase().starts_with("text/html"))
        .filter(|v| v["json"].as_str().is_some_and(|j| j.contains("\"game\"") || j.contains("\"app\"")))
        .map(|v| json!({"txid": v["txid"], "number": v["number"], "creator": v["creator"], "json": v["json"], "block_height": v["block_height"], "position": v["position"]}))
        .collect()
}

/// Card rows of chains this arcade only borrows, from the arcades on the mesh that keep their
/// ledger: kept here, asked again in the background every 10 minutes (the store never waits).
static BORROWED_CARDS: std::sync::Mutex<BTreeMap<String, (std::time::Instant, Vec<Value>)>> = std::sync::Mutex::new(BTreeMap::new());

/// Holders asked for a chain's app cards, to compare what they say.
const CARD_HOLDERS: usize = 3;

/// The app card rows of a chain from the mesh, kept 10 minutes; asked again in the background when older. `kept`: the
/// chain's ledger is kept here too (catching up), so the question goes to the mesh rather than being answered here.
///
/// A row is one arcade's word: up to three independent holders are asked and their rows compared (`holders`: how
/// many sent the same row; `trust` "red" when they disagree), and each card's creator and JSON are checked against
/// its first transaction where this arcade can read it (`checked`: true, false, or null when it can't).
fn mesh_card_rows(node: &Node, p: &'static ChainParams, kept: bool) -> Vec<Value> {
    let (stale, rows) = match BORROWED_CARDS.lock().unwrap().get(p.key) {
        Some((at, rows)) => (at.elapsed().as_secs() > 600, rows.clone()),
        None => (true, Vec::new()),
    };
    if stale {
        // Marked fresh now, so one ask at a time; the answer replaces it when it comes.
        BORROWED_CARDS.lock().unwrap().insert(p.key.to_string(), (std::time::Instant::now(), rows.clone()));
        let n = node.self_arc();
        std::thread::spawn(move || {
            let q = json!({"chain": p.key});
            let mut answers: Vec<(Vec<Value>, String)> = crate::vouch::ask_each(&n, p, "ledger", "explorer.app_cards", &q, CARD_HOLDERS, 20_000)
                .into_iter()
                .filter_map(|(v, id)| Some((v["rows"].as_array()?.clone(), id)))
                .collect();
            if answers.is_empty() {
                // Nobody answered this round: one holder, as before (it also asks to be introduced to holders).
                let got = if kept {
                    crate::explorer::mesh_ask(&n, p, "ledger", "explorer.app_cards", &q)
                } else {
                    crate::explorer::handle(&n, "explorer.app_cards", &q).map(|v| {
                        let id = v["_mesh"]["node"].as_str().unwrap_or("").to_string();
                        (v, id)
                    })
                };
                if let Ok((v, id)) = got {
                    answers.push((v["rows"].as_array().cloned().unwrap_or_default(), id));
                }
            }
            if answers.is_empty() {
                return;
            }
            let mut rows = merge_card_rows(&answers);
            for r in rows.iter_mut().filter(|r| r["trust"] != "red") {
                r["checked"] = match card_checked(&n, p, r) {
                    Some(true) => json!(true),
                    Some(false) => {
                        r["trust"] = json!(crate::vouch::Level::Red.as_str());
                        r["why"] = json!("its creator or card is not what its transaction says");
                        json!(false)
                    }
                    None => Value::Null,
                };
            }
            BORROWED_CARDS.lock().unwrap().insert(p.key.to_string(), (std::time::Instant::now(), rows));
            // The kept apps see the answer at their next read (2026-11-21: the fingerprint counts these).
            MESH_CARDS_SEEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
    }
    rows
}

/// Rows from several holders, one per txid: `holders` = how many sent it as it is, and "trust": "red" with `why`
/// when two holders sent the same card differently (creator, JSON, height, place or number).
fn merge_card_rows(answers: &[(Vec<Value>, String)]) -> Vec<Value> {
    let said = |r: &Value| json!([r["creator"], r["json"], r["block_height"], r["position"], r["number"]]);
    let mut by: BTreeMap<String, Vec<(Option<Value>, String)>> = BTreeMap::new();
    let mut first: BTreeMap<String, Value> = BTreeMap::new();
    for (rows, id) in answers {
        for r in rows {
            let Some(t) = r["txid"].as_str().map(str::to_lowercase) else { continue };
            // Only the card's own fields: what this arcade says of it (holders, checked, trust) is never taken from a holder.
            first.entry(t.clone()).or_insert_with(|| json!({"txid": t, "number": r["number"], "creator": r["creator"], "json": r["json"],
                                                            "block_height": r["block_height"], "position": r["position"]}));
            by.entry(t).or_default().push((Some(said(r)), id.clone()));
        }
    }
    let mut out = Vec::new();
    for (t, said) in by {
        let groups = crate::vouch::tally(&said);
        let mut r = first.remove(&t).unwrap_or(Value::Null);
        if groups.len() > 1 {
            r["trust"] = json!(crate::vouch::Level::Red.as_str());
            r["why"] = json!(format!("arcades on the mesh sent this card differently ({})", groups.iter().map(|g| g.1.iter().map(|w| crate::vouch::who(w)).collect::<Vec<_>>().join(", ")).collect::<Vec<_>>().join(" / ")));
            r["holders"] = json!(0);
        } else {
            // The row as most of them sent it (all of them, here), and how many did.
            r = groups.first().map(|g| {
                let mut x = r.clone();
                x["holders"] = json!(g.1.len());
                x
            }).unwrap_or(r);
        }
        out.push(r);
    }
    out
}

/// Each card checked once: (chain, txid, creator, JSON) -> whether its first transaction says so.
static CARD_CHECKS: std::sync::Mutex<BTreeMap<String, bool>> = std::sync::Mutex::new(BTreeMap::new());

/// Whether a mesh card row is what its first transaction says (creator and JSON); None when that transaction
/// can't be read here.
fn card_checked(node: &Node, p: &'static ChainParams, r: &Value) -> Option<bool> {
    let (txid, creator, j) = (r["txid"].as_str()?, r["creator"].as_str()?, r["json"].as_str()?);
    let key = format!("{}|{txid}|{creator}|{j}", p.key);
    if let Some(b) = CARD_CHECKS.lock().unwrap().get(&key) {
        return Some(*b);
    }
    let ok = crate::pagechain::card_matches(node, p, txid, creator, j).ok()?;
    let mut g = CARD_CHECKS.lock().unwrap();
    if g.len() > 20_000 {
        g.clear();
    }
    g.insert(key, ok);
    Some(ok)
}

fn borrowed_card_rows(node: &Node, kept: &[&'static ChainParams]) -> Vec<(&'static ChainParams, Vec<Value>)> {
    let mut out = Vec::new();
    for p in arcade_chain::params::ALL_CHAINS.iter().copied() {
        if kept.iter().any(|k| k.chain_no == p.chain_no) || !crate::borrow::is_borrowed(p.key) {
            continue;
        }
        let rows = mesh_card_rows(node, p, false);
        out.push((p, rows));
    }
    out
}

// ---------------- the cached apps (2026-11-21) ----------------
//
// Reading every chain's card rows and resolving them (gather_now) took about two minutes on a busy arcade, and the store,
// one app's card, launching an app, a page's app and the installed list each did it, often twice in one request (card()
// read the apps once to find the app and again for the copycat flags). Now the apps are kept: one copy, shared (an Arc,
// never deep-copied), made again at most every FRESH, or sooner when this arcade itself publishes (invalidate). Only one
// copy is made at a time: a caller waits for the one being made, or, when there is an older copy and the new one has
// taken more than WAIT_AT_MOST already, takes the older one (it is at most a little behind). The copycat flags are kept
// beside the copy they were made from, and made again when the operator rules on a card or trusts a publisher.

/// How long a copy of the apps is used as it is.
const FRESH: std::time::Duration = std::time::Duration::from_secs(15);
/// A copy older than FRESH is still used while nothing it reads moved (no kept chain's tip, no answer from the mesh about
/// a borrowed chain's cards), up to this age: rows lent by other arcades (feed acts) can change without a tip moving.
const FRESH_IF_UNCHANGED: std::time::Duration = std::time::Duration::from_secs(60);
/// How long a caller waits for a new copy when there is an older one to take instead.
const WAIT_AT_MOST: std::time::Duration = std::time::Duration::from_secs(2);

/// Bumped each time the mesh answers about a borrowed chain's app cards (BORROWED_CARDS), so a copy made before knows.
static MESH_CARDS_SEEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
/// Numbers each copy of the apps, so the flags know which copy they were made from.
static SNAP_IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// What a copy of the apps was made from, cheaply read: each kept chain's tip, the chains borrowed now, and how many
/// mesh answers about borrowed cards had come.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct Fingerprint {
    tips: Vec<(u16, Option<(u32, [u8; 32])>)>,
    borrowed: Vec<u16>,
    mesh_cards: u64,
}

fn fingerprint(node: &Node) -> Fingerprint {
    Fingerprint {
        tips: ledger_chains(node).iter().map(|p| (p.chain_no, node.chain.tip(p.chain_no).ok().flatten().map(|t| (t.height, t.hash.0)))).collect(),
        borrowed: arcade_chain::params::ALL_CHAINS.iter().filter(|p| crate::borrow::is_borrowed(p.key)).map(|p| p.chain_no).collect(),
        mesh_cards: MESH_CARDS_SEEN.load(std::sync::atomic::Ordering::SeqCst),
    }
}

/// One copy of the apps.
struct Snap {
    apps: Vec<App>,
    id: u64,
    at: std::time::Instant,
    fp: Fingerprint,
    /// The cache's epoch when it was begun: a write here since (invalidate) makes it old.
    epoch: u64,
}

#[derive(Default)]
struct CacheState {
    snap: Option<Arc<Snap>>,
    /// Since when a copy is being made (None: none is).
    building: Option<std::time::Instant>,
    epoch: u64,
}

/// The apps of this arcade, kept (one per Node: a test's arcades never share them).
#[derive(Default)]
pub struct Cache {
    st: Mutex<CacheState>,
    cv: std::sync::Condvar,
    /// (copy id, flags epoch, flags): one made at a time.
    flags: Mutex<Option<(u64, u64, Arc<BTreeMap<String, String>>)>>,
    flags_epoch: std::sync::atomic::AtomicU64,
    /// The page rows (/r/apps, [`page_rows`]) of one copy and its flags: made once per copy.
    rows: Mutex<Option<(u64, Arc<BTreeMap<String, String>>, Arc<Vec<Value>>)>>,
}

/// Marks the copy being made as no longer being made if making it panicked, so the callers waiting for it don't wait forever.
struct Building<'a>(&'a Cache, bool);

impl Drop for Building<'_> {
    fn drop(&mut self) {
        if !self.1 {
            let mut s = self.0.st.lock().unwrap_or_else(|e| e.into_inner());
            s.building = None;
            self.0.cv.notify_all();
        }
    }
}

impl Cache {
    fn lock(&self) -> std::sync::MutexGuard<'_, CacheState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The copy to use for `fp`, made with `build` when there is none fresh (see the notes above).
    fn snapshot(&self, fp: Fingerprint, build: impl FnOnce() -> Vec<App>) -> Arc<Snap> {
        let mut s = self.lock();
        loop {
            if let Some(sn) = &s.snap {
                let age = sn.at.elapsed();
                if sn.epoch == s.epoch && (age < FRESH || (age < FRESH_IF_UNCHANGED && sn.fp == fp)) {
                    return sn.clone();
                }
            }
            match s.building {
                None => break,
                Some(since) => match s.snap.clone() {
                    None => s = self.cv.wait(s).unwrap_or_else(|e| e.into_inner()),
                    Some(old) => {
                        let left = WAIT_AT_MOST.saturating_sub(since.elapsed());
                        if left.is_zero() {
                            return old;
                        }
                        s = self.cv.wait_timeout(s, left).unwrap_or_else(|e| e.into_inner()).0;
                    }
                },
            }
        }
        // This caller makes it.
        s.building = Some(std::time::Instant::now());
        let epoch = s.epoch;
        drop(s);
        let mut guard = Building(self, false);
        let apps = build();
        let snap = Arc::new(Snap { apps, id: SNAP_IDS.fetch_add(1, std::sync::atomic::Ordering::SeqCst), at: std::time::Instant::now(), fp, epoch });
        let mut s = self.lock();
        s.snap = Some(snap.clone());
        s.building = None;
        guard.1 = true;
        self.cv.notify_all();
        snap
    }

    /// The flags of `snap`, made with `make` when the kept ones are of another copy or older than a verdict.
    fn flags(&self, snap: &Snap, make: impl FnOnce() -> BTreeMap<String, String>) -> Arc<BTreeMap<String, String>> {
        let mut f = self.flags.lock().unwrap_or_else(|e| e.into_inner());
        let fe = self.flags_epoch.load(std::sync::atomic::Ordering::SeqCst);
        if let Some((id, e, m)) = &*f {
            if *id == snap.id && *e == fe {
                return m.clone();
            }
        }
        let m = Arc::new(make());
        *f = Some((snap.id, fe, m.clone()));
        m
    }

    fn invalidate(&self) {
        self.lock().epoch += 1;
        self.invalidate_flags();
    }

    fn invalidate_flags(&self) {
        self.flags_epoch.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Make the apps again on the next read (this arcade published or withdrew a card, say).
pub fn invalidate(node: &Node) {
    node.apps_cache.invalidate();
}

/// Make the copycat flags again on the next read (a verdict on a card, a publisher trusted or not).
pub fn invalidate_flags(node: &Node) {
    node.apps_cache.invalidate_flags();
}

/// Every app on every ledger chain (this arcade's, and the chains it borrows), grouped and resolved: the kept copy
/// ([`Cache`]), made again at most every [`FRESH`].
fn gather(node: &Node) -> Arc<Snap> {
    node.apps_cache.snapshot(fingerprint(node), || gather_now(node))
}

/// The copycat flags of a copy of the apps ([`flags`]), kept beside it.
fn flags_of(node: &Node, snap: &Snap) -> Arc<BTreeMap<String, String>> {
    node.apps_cache.flags(snap, || flags(node, &snap.apps))
}

/// [`gather`] made afresh: every chain's card rows read and resolved.
fn gather_now(node: &Node) -> Vec<App> {
    let mut apps = Vec::new();
    let acts = crate::feedsvc::ledger_rows(node, arcade_ledger::messages::ACT);
    let kept = ledger_chains(node);
    let mut chains: Vec<(&'static ChainParams, Vec<Value>)> = kept.iter().map(|p| (*p, card_rows(node, p))).collect();
    // A chain kept here that is still catching up (borrowed meanwhile) hasn't reached the blocks most apps were
    // published in: the cards an arcade on the mesh keeps are added, until this one has them itself.
    for (p, rows) in chains.iter_mut() {
        if crate::borrow::is_borrowed(p.key) {
            let have: BTreeSet<String> = rows.iter().filter_map(|r| r["txid"].as_str().map(String::from)).collect();
            rows.extend(mesh_card_rows(node, p, true).into_iter().filter(|r| r["txid"].as_str().is_some_and(|t| !have.contains(t))));
        }
    }
    chains.extend(borrowed_card_rows(node, &kept));
    for (p, rows) in chains {
        let mut by: BTreeMap<(String, String), Vec<Ver>> = BTreeMap::new();
        for v in rows {
            let j = v["json"].as_str().unwrap_or("");
            let Some(card) = cards::parse(j) else { continue };
            let Some((trust, holders, checked)) = vouched(&v) else {
                // Arcades disagree on it, or it isn't what its transaction says: not offered.
                let txid = v["txid"].as_str().unwrap_or("");
                crate::vouch::alert_once(node, &format!("card:{}:{txid}", p.key), "apps", &format!(
                    "An app card on {} ({}, {txid}) is not listed: {}", p.name, card.name, v["why"].as_str().unwrap_or("arcades disagree on it")));
                continue;
            };
            let creator = v["creator"].as_str().unwrap_or("").to_string();
            by.entry((creator.clone(), card.key())).or_default().push(Ver {
                txid: v["txid"].as_str().unwrap_or("").to_string(),
                number: v["number"].as_u64().unwrap_or(0),
                creator,
                height: v["block_height"].as_u64().unwrap_or(0),
                position: v["position"].as_u64().unwrap_or(0),
                card,
                trust,
                holders,
                checked,
            });
        }
        for (root, owner, mut vs) in follow_transfers(by) {
            vs.sort_by(|a, b| (b.height, b.position).cmp(&(a.height, a.position)));
            let Some((vs, withdrawn)) = resolve(vs) else { continue };
            let newest = &vs[0];
            // Removed: the creator's DELETE on the newest card (feed acts on any ledger chain, by the creator's key).
            let creator = node.home_address(&newest.creator);
            let removed = acts.iter().any(|a| a["kind"] == 6 && a["target"] == newest.txid.as_str() && a["from"] == creator.as_str());
            apps.push(App { chain: p, root, owner, versions: vs, removed, withdrawn });
        }
    }
    apps
}

/// A card row's (trust, holders, checked): this arcade's own rows carry no `holders` (green, checked); a
/// mesh row's trust is how many arcades sent it alike. None for a red row (not offered).
fn vouched(v: &Value) -> Option<(crate::vouch::Level, usize, bool)> {
    if v["trust"] == "red" || v["checked"] == false {
        return None;
    }
    match v["holders"].as_u64() {
        None => Some((crate::vouch::Level::Green, 0, true)),
        Some(n) => Some((crate::vouch::Level::of_sources(n as usize), n as usize, v["checked"] == true)),
    }
}

/// What the store says of a card's trust: {level, holders, checked, note}.
fn trust_json(v: &Ver) -> Value {
    let note = match (v.holders, v.checked) {
        (0, _) => "From this arcade's own ledger.".to_string(),
        (1, true) => "One arcade on the mesh listed this version; its publisher and card were checked against its transaction here, its place among the versions was not.".to_string(),
        (1, false) => "One arcade on the mesh listed this version, and its transaction couldn't be read here to check it. Double-check the publisher before trusting it.".to_string(),
        (n, true) => format!("{n} arcades on the mesh listed this version alike; its publisher and card were checked against its transaction here."),
        (n, false) => format!("{n} arcades on the mesh listed this version alike; its transaction couldn't be read here to check it."),
    };
    json!({"level": v.trust.as_str(), "holders": v.holders, "checked": v.checked, "note": note})
}

fn block_time(node: &Node, c: u16, h: u64) -> u64 {
    node.chain.hash_at(c, h as u32).ok().flatten().and_then(|hash| node.chain.header(c, &hash).ok().flatten()).map(|r| r.header.time as u64).unwrap_or(0)
}

/// Copycat flags: the newest card of each app compared with earlier cards by other creators.
fn flags(node: &Node, apps: &[App]) -> BTreeMap<String, String> {
    let trusted: BTreeSet<String> = meta_get(node, "apps_trusted").as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    // What a card can be mistaken for is an app the store still lists: not one its creator removed,
    // and not one the operator confirmed as a copy.
    let all = comparable(apps, |a| meta_get(node, &format!("cardwatch:{}:{}", a.chain.key, a.versions[0].txid)) == "confirmed");
    let mut out = BTreeMap::new();
    for a in apps {
        let n = &a.versions[0];
        let verdict = meta_get(node, &format!("cardwatch:{}:{}", a.chain.key, n.txid));
        if let Some(v) = verdict.as_str() {
            out.insert(n.txid.clone(), v.to_string());
            continue;
        }
        // A trusted publisher is never held back, but only when the card is known to be theirs: a card another arcade
        // names them on, unchecked, is compared like any other.
        if trusted.contains(&n.creator) && n.checked {
            continue;
        }
        if let Some(f) = lookalike(n, &a.root, &all) {
            out.insert(n.txid.clone(), f);
        }
    }
    out
}

/// The cards a new card may be mistaken for: every version of each app the store still lists. Not one its creator
/// removed, nor one its creator withdrew whole (Martin 2026-11-21: once the original owner took it away, a new app of
/// that name is no copy of anything), nor one the operator confirmed as a copy (`confirmed`).
fn comparable<'a>(apps: &'a [App], confirmed: impl Fn(&App) -> bool) -> Vec<(&'a Ver, &'static str, &'a str)> {
    let mut all = Vec::new();
    for a in apps {
        if a.removed || a.withdrawn.is_some() || confirmed(a) {
            continue;
        }
        for v in &a.versions {
            all.push((v, a.chain.key, a.root.as_str()));
        }
    }
    all
}

/// "flagged: looks like ..." when an earlier card by another creator in `all` looks like `n`.
/// `root`: the app `n` is a version of; its own earlier versions (by an owner it was transferred from) are no copy.
fn lookalike(n: &Ver, root: &str, all: &[(&Ver, &str, &str)]) -> Option<String> {
    let earlier = all.iter().find(|(o, _, r)| *r != root && o.creator != n.creator && (o.height, o.position) < (n.height, n.position) && cards::looks_like(&n.card, &o.card));
    earlier.map(|(o, ch, _)| format!("flagged: looks like {} by {} on {ch}", o.card.name, o.creator))
}

/// Opinions, comments and tips on any version of an app (home chain feed acts).
/// A review's stars: a comment that begins with five marks, ★ for each star given and ☆ for the
/// rest, given ones first ("★★★★☆ works well" = 4). The original arcade shows it as a comment.
pub fn stars_of(text: &str) -> Option<(u8, &str)> {
    let head: Vec<char> = text.chars().take(5).collect();
    if head.len() < 5 || !head.iter().all(|c| *c == '★' || *c == '☆') {
        return None;
    }
    let n = head.iter().take_while(|c| **c == '★').count();
    if n == 0 || head[n..].iter().any(|c| *c == '★') {
        return None;
    }
    let rest = &text[head.iter().map(|c| c.len_utf8()).sum::<usize>()..];
    Some((n as u8, rest.trim()))
}

/// Where an act sits in time: its block height (unconfirmed last), then its txid.
fn act_order(a: &Value) -> (u64, String) {
    (a["o"].as_u64().or_else(|| a["h"].as_u64().filter(|h| *h > 0)).unwrap_or(u64::MAX), a["txid"].as_str().unwrap_or("").to_string())
}

/// Every edit (kind 5) and delete (kind 6) keyed by (target txid, writer), built once per request
/// so each comment finds its own in one lookup instead of a scan of every act.
struct Fixes {
    /// The latest edit's order and text.
    edits: BTreeMap<(String, String), ((u64, String), Value)>,
    deletes: BTreeSet<(String, String)>,
}

fn fixes(acts: &[Value]) -> Fixes {
    let mut f = Fixes { edits: BTreeMap::new(), deletes: BTreeSet::new() };
    for a in acts {
        let key = || (a["target"].as_str().unwrap_or("").to_string(), a["from"].as_str().unwrap_or("").to_string());
        match a["kind"].as_u64().unwrap_or(0) {
            5 => {
                let o = act_order(a);
                // Ties go to the later row, as max_by_key would.
                if f.edits.get(&key()).is_none_or(|(x, _)| o >= *x) {
                    f.edits.insert(key(), (o, a["text"].clone()));
                }
            }
            6 => {
                f.deletes.insert(key());
            }
            _ => {}
        }
    }
    f
}

/// Comments on an app (any version) as they stand: an edit by its writer replaces its text, a
/// delete by its writer takes it out. Oldest first.
fn comments_on(acts: &[Value], fx: &Fixes, txids: &BTreeSet<String>) -> Vec<Value> {
    let k = |a: &Value| a["kind"].as_u64().unwrap_or(0);
    let key = |c: &Value| (c["txid"].as_str().unwrap_or("").to_string(), c["from"].as_str().unwrap_or("").to_string());
    let mut out: Vec<Value> = acts
        .iter()
        .filter(|a| k(a) == 3 && txids.contains(a["target"].as_str().unwrap_or("")))
        .filter(|c| !fx.deletes.contains(&key(c)))
        .cloned()
        .collect();
    for c in out.iter_mut() {
        if let Some((_, text)) = fx.edits.get(&key(c)) {
            c["text"] = text.clone();
        }
        if let Some((n, _)) = stars_of(c["text"].as_str().unwrap_or("")) {
            c["stars"] = json!(n);
        }
    }
    out.sort_by_key(act_order);
    out
}

/// Star ratings from the reviews: each person's latest surviving review with stars counts (edits
/// applied, deleted ones gone). `my_review` is the one that counts for me, the one "change" edits;
/// `my_reviews` is every starred review of mine still standing, oldest first, so "remove" can
/// delete them all and an older one does not count again.
fn ratings(comments: &[Value], me: &str) -> Value {
    let mut by: BTreeMap<String, (u8, String)> = BTreeMap::new();
    let mut mine_all: Vec<String> = Vec::new();
    for c in comments {
        if let Some(n) = c["stars"].as_u64() {
            let from = c["from"].as_str().unwrap_or("");
            let txid = c["txid"].as_str().unwrap_or("").to_string();
            if from == me {
                mine_all.push(txid.clone());
            }
            by.insert(from.to_string(), (n as u8, txid));
        }
    }
    let mut dist = [0u64; 5];
    for (n, _) in by.values() {
        dist[*n as usize - 1] += 1;
    }
    let count: u64 = dist.iter().sum();
    let sum: u64 = dist.iter().enumerate().map(|(i, x)| (i as u64 + 1) * x).sum();
    let mine = by.get(me);
    json!({"count": count, "average": if count == 0 { Value::Null } else { json!((sum as f64 / count as f64 * 10.0).round() / 10.0) },
           "dist": dist, "mine": mine.map(|m| m.0), "my_review": mine.map(|m| m.1.clone()), "my_reviews": mine_all})
}

fn social(acts: &[Value], fx: &Fixes, txids: &BTreeSet<String>, me: &str) -> Value {
    let on: Vec<&Value> = acts.iter().filter(|a| txids.contains(a["target"].as_str().unwrap_or(""))).collect();
    let k = |a: &Value| a["kind"].as_u64().unwrap_or(0);
    let order = |a: &Value| (a["o"].as_u64().or_else(|| a["h"].as_u64().filter(|h| *h > 0)).unwrap_or(u64::MAX), a["txid"].as_str().unwrap_or("").to_string());
    let mut likes: BTreeMap<String, (u64, (u64, String))> = BTreeMap::new();
    let mut dislikes: BTreeMap<String, (u64, (u64, String))> = BTreeMap::new();
    for a in &on {
        let who = a["from"].as_str().unwrap_or("").to_string();
        let o = order(a);
        let map = match k(a) {
            1 | 2 => &mut likes,
            8 | 9 => &mut dislikes,
            _ => continue,
        };
        if map.get(&who).is_none_or(|x| o > x.1) {
            map.insert(who, (k(a), o));
        }
    }
    let nl = likes.values().filter(|x| x.0 == 1).count();
    let nd = dislikes.values().filter(|x| x.0 == 8).count();
    let comments = comments_on(acts, fx, txids);
    let rating = ratings(&comments, me);
    // Each rating moves the score like a like or a dislike would: +1 for 5 stars, -1 for 1.
    // (stars - 3) / 2 per rater: 5 stars +1, 4 +0.5, 3 nothing, 2 -0.5, 1 -1.
    let stars_score: f64 = rating["dist"].as_array().map(|d| d.iter().enumerate().map(|(i, x)| (i as f64 - 2.0) / 2.0 * x.as_f64().unwrap_or(0.0)).sum()).unwrap_or(0.0);
    let tips = on.iter().filter(|a| matches!(k(a), 7 | 10)).count();
    let mine = if likes.get(me).is_some_and(|x| x.0 == 1) { "like" } else if dislikes.get(me).is_some_and(|x| x.0 == 8) { "dislike" } else { "none" };
    json!({"likes": nl, "dislikes": nd, "comments": comments.len(), "tips": tips, "my_opinion": mine,
           "last_comments": comments.iter().rev().take(3).rev().map(|a| json!({"from": a["from"], "text": a["text"], "txid": a["txid"], "stars": a["stars"]})).collect::<Vec<_>>(),
           "rating": rating,
           "score": nl as f64 - nd as f64 + 0.5 * comments.len() as f64 + stars_score})
}

/// Apps every arcade has installed from the start (Martin 2026-11-17): the Browser, and the Feed, which the Feed tab
/// became. Each: its app id, and whether its Feed switch starts on (the Feed app posts in the operator's name, as the
/// tab did). Added once each, as soon as its card is known here: an operator who removes one keeps it removed.
pub const DEFAULT_APPS: &[(&str, bool)] = &[
    ("peptest:nWKJgSeAZSmfRCxZnwVgtYyPNowQT5X1Gk:browser", false),
    ("arctest:aSBD9BhXG2sNtCmy4MW8LnonVgD78LSSqD:feed", true),
];

/// Install the [`DEFAULT_APPS`] not added before (at most one look through the cards a minute: the first may come
/// before the chains holding them are read).
pub fn add_defaults(node: &Node) {
    static LAST: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let done: Vec<String> = meta_get(node, "apps_defaults").as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    let todo: Vec<&(&str, bool)> = DEFAULT_APPS.iter().filter(|d| !done.iter().any(|x| x == d.0)).collect();
    if todo.is_empty() {
        return;
    }
    {
        let mut l = LAST.lock().unwrap();
        if l.is_some_and(|t| t.elapsed() < std::time::Duration::from_secs(60)) {
            return;
        }
        *l = Some(std::time::Instant::now());
    }
    let snap = gather(node);
    let apps = &snap.apps;
    let mut done = done;
    let mut ids = installed_ids(node);
    for (id, feed) in todo {
        let Some(a) = apps.iter().find(|a| !a.removed && a.withdrawn.is_none() && app_id(a.chain.key, &a.root, &a.versions[0].card.key()) == *id) else { continue };
        if !ids.iter().any(|x| x["id"] == *id) {
            ids.push(json!({"id": id, "at": arcade_app::now(), "card": a.versions[0].card.to_json(), "default": true}));
            let _ = node.app.event("apps", &format!("installed {} (one of the apps every arcade starts with)", a.versions[0].card.name));
        }
        if *feed {
            let _ = set_perms(node, id, &json!({"feed": true}));
        }
        done.push(id.to_string());
    }
    let _ = meta_put(node, "apps_installed", &json!(ids));
    let _ = meta_put(node, "apps_defaults", &json!(done));
}

fn installed_ids(node: &Node) -> Vec<Value> {
    meta_get(node, "apps_installed").as_array().cloned().unwrap_or_default()
}

fn starred_ids(node: &Node) -> BTreeSet<String> {
    meta_get(node, "apps_starred").as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

fn entry(node: &Node, a: &App, acts: &[Value], fx: &Fixes, me: &str, flag: Option<&String>, installed: &BTreeSet<String>, starred: &BTreeSet<String>) -> Value {
    let n = &a.versions[0];
    let c = a.chain.chain_no;
    let id = app_id(a.chain.key, &a.root, &n.card.key());
    // Open `play` only when that page has the same creator.
    let opens = if !n.card.play.is_empty()
        && arcade_ledger::content::inscription(&node.chain, c, &n.card.play).is_some_and(|p| p["creator"] == n.creator.as_str())
    {
        n.card.play.clone()
    } else {
        n.txid.clone()
    };
    let txids: BTreeSet<String> = a.versions.iter().map(|v| v.txid.clone()).collect();
    let mut s = social(acts, fx, &txids, me);
    let time = block_time(node, c, n.height);
    let age_h = arcade_app::now().saturating_sub(time) as f64 / 3600.0;
    let hot = (1.0 + s["score"].as_f64().unwrap_or(0.0)) / (age_h + 2.0).powf(1.5);
    s["hot"] = json!(hot);
    let tag = creator_tag(node, a.chain, &n.creator);
    json!({
        "id": id, "chain": a.chain.key, "chain_name": a.chain.name, "txid": n.txid, "number": n.number,
        "creator": n.creator, "creator_tag": tag, "owner": a.owner, "first_publisher": a.root, "height": n.height, "time": time, "card": n.card.to_json(),
        "opens": opens, "versions": a.versions.iter().map(|v| json!({"txid": v.txid, "version": v.card.version, "height": v.height, "trust": v.trust.as_str()})).collect::<Vec<_>>(),
        "trust": trust_json(n),
        "removed": a.removed, "withdrawn": a.withdrawn, "flag": flag, "mine": n.creator == me, "installed": installed.contains(&id), "starred": starred.contains(&id),
        "social": s,
    })
}

fn me(node: &Node) -> String {
    crate::msgsvc::sender_key(node).map(|(a, _, _)| a).unwrap_or_default()
}

/// App Store categories: these first, in this order. Any other category a card names shows after
/// them, so a new kind of app needs no new arcade. (key, label, what it holds)
pub const CATEGORIES: &[(&str, &str, &str)] = &[
    ("games", "Games", "Things to play"),
    ("apps", "Apps", "Tools and everything else"),
    ("social", "Social", "Chats, groups and feeds"),
    ("node-modules", "Node modules", "Features for this arcade itself, published and installed like any app"),
    ("themes", "Themes", "How this window looks: an application window or a desktop, its colours and frame (docs/THEMES.md)"),
];

/// A category for the store's list: a known one's label, else its name in words.
fn category_json(key: &str, count: usize) -> Value {
    match CATEGORIES.iter().find(|c| c.0 == key) {
        Some((k, l, a)) => json!({"key": k, "label": l, "about": a, "count": count, "known": true}),
        None => {
            let label: String = key.split('-').map(|w| { let mut c = w.chars(); c.next().map(|f| f.to_uppercase().chain(c).collect::<String>()).unwrap_or_default() }).collect::<Vec<_>>().join(" ");
            json!({"key": key, "label": label, "about": "", "count": count, "known": false})
        }
    }
}

/// The categories, for publishing (the known ones).
pub fn categories() -> Value {
    Value::Array(CATEGORIES.iter().map(|c| category_json(c.0, 0)).collect())
}

/// The App Store: every app (not removed; flagged only when asked), starred first. `category`
/// "" is all of them; the reply counts each category among the apps the search finds.
pub fn store(node: &Node, sort: &str, q: &str, show_flagged: bool, category: &str) -> Value {
    store_with(node, sort, q, show_flagged, category, false)
}

/// [`store`]; `withdrawn`: only the apps their publishers withdrew (never listed otherwise; 2026-11-21).
pub fn store_with(node: &Node, sort: &str, q: &str, show_flagged: bool, category: &str, withdrawn: bool) -> Value {
    let snap = gather(node);
    let apps = &snap.apps;
    let fl = flags_of(node, &snap);
    let acts = crate::feedsvc::ledger_rows(node, arcade_ledger::messages::ACT);
    let fx = fixes(&acts);
    let me = me(node);
    let installed: BTreeSet<String> = installed_ids(node).iter().filter_map(|x| x["id"].as_str().map(String::from)).collect();
    let starred = starred_ids(node);
    let q = q.trim().to_lowercase();
    let mut out: Vec<Value> = apps
        .iter()
        .filter(|a| !a.removed && a.withdrawn.is_some() == withdrawn)
        .filter(|a| {
            let f = fl.get(&a.versions[0].txid);
            let hidden = f.is_some_and(|f| f.starts_with("flagged") || f == "confirmed");
            show_flagged == hidden
        })
        .filter(|a| q.is_empty() || a.versions[0].card.name.to_lowercase().contains(&q) || a.versions[0].card.genre.to_lowercase().contains(&q) || a.versions[0].card.description.to_lowercase().contains(&q) || a.versions[0].card.category.contains(&q))
        .map(|a| entry(node, a, &acts, &fx, &me, fl.get(&a.versions[0].txid), &installed, &starred))
        .collect();
    let mut counts: BTreeMap<String, usize> = CATEGORIES.iter().map(|c| (c.0.to_string(), 0)).collect();
    for a in &out {
        *counts.entry(a["card"]["category"].as_str().unwrap_or("apps").to_string()).or_default() += 1;
    }
    let mut cats: Vec<Value> = CATEGORIES.iter().map(|c| category_json(c.0, counts[c.0])).collect();
    cats.extend(counts.iter().filter(|(k, _)| !CATEGORIES.iter().any(|c| c.0 == k.as_str())).map(|(k, n)| category_json(k, *n)));
    if !category.is_empty() {
        out.retain(|a| a["card"]["category"] == category);
    }
    out.sort_by(|a, b| {
        let st = b["starred"].as_bool().cmp(&a["starred"].as_bool());
        st.then_with(|| if sort == "new" {
            b["height"].as_u64().cmp(&a["height"].as_u64())
        } else {
            b["social"]["hot"].as_f64().partial_cmp(&a["social"]["hot"].as_f64()).unwrap_or(std::cmp::Ordering::Equal)
        })
    });
    json!({"apps": out, "categories": cats, "chains": ledger_chains(node).iter().map(|p| p.key).collect::<Vec<_>>()})
}

/// The apps installed here, each at its newest version.
pub fn installed(node: &Node) -> Value {
    add_defaults(node);
    let ids = installed_ids(node);
    let snap = gather(node);
    let apps = &snap.apps;
    let fl = flags_of(node, &snap);
    let acts = crate::feedsvc::ledger_rows(node, arcade_ledger::messages::ACT);
    let fx = fixes(&acts);
    let me = me(node);
    let set: BTreeSet<String> = ids.iter().filter_map(|x| x["id"].as_str().map(String::from)).collect();
    let starred = starred_ids(node);
    let mut out = Vec::new();
    for i in &ids {
        let id = i["id"].as_str().unwrap_or("");
        match apps.iter().find(|a| app_id(a.chain.key, &a.root, &a.versions[0].card.key()) == id) {
            Some(a) => {
                let mut e = entry(node, a, &acts, &fx, &me, fl.get(&a.versions[0].txid), &set, &starred);
                e["installed_at"] = i["at"].clone();
                out.push(e);
            }
            None => out.push(json!({"id": id, "missing": true, "card": i["card"], "installed_at": i["at"]})),
        }
    }
    json!(out)
}

/// The app with a version `txid` on `chain` in `snap` (one copy of the apps: gather), and its id.
fn find<'a>(snap: &'a Snap, chain: &str, txid: &str) -> Result<(&'a App, String), String> {
    let p = crate::chains::find(chain).ok_or_else(|| format!("unknown chain {chain}"))?;
    let a = snap.apps
        .iter()
        .find(|a| a.chain.chain_no == p.chain_no && a.versions.iter().any(|v| v.txid == txid))
        .ok_or("no app card with that txid here")?;
    let id = app_id(a.chain.key, &a.root, &a.versions[0].card.key());
    Ok((a, id))
}

pub fn install(node: &Node, chain: &str, txid: &str) -> Result<Value, String> {
    let snap = gather(node);
    let (a, id) = find(&snap, chain, txid)?;
    let mut ids = installed_ids(node);
    if !ids.iter().any(|x| x["id"] == id.as_str()) {
        ids.push(json!({"id": id, "at": arcade_app::now(), "card": a.versions[0].card.to_json()}));
        meta_put(node, "apps_installed", &json!(ids))?;
        let _ = node.app.event("apps", &format!("installed {}", a.versions[0].card.name));
    }
    Ok(json!({"installed": id}))
}

pub fn uninstall(node: &Node, id: &str) -> Result<Value, String> {
    let ids: Vec<Value> = installed_ids(node).into_iter().filter(|x| x["id"] != id).collect();
    meta_put(node, "apps_installed", &json!(ids))?;
    Ok(json!({"uninstalled": id}))
}

pub fn star(node: &Node, chain: &str, txid: &str, on: bool) -> Result<Value, String> {
    let (_, id) = find(&gather(node), chain, txid)?;
    let mut s = starred_ids(node);
    if on { s.insert(id.clone()); } else { s.remove(&id); }
    meta_put(node, "apps_starred", &json!(s.into_iter().collect::<Vec<_>>()))?;
    Ok(json!({"id": id, "starred": on}))
}

/// One app: card, versions, every comment, permission switches. One copy of the apps for both the app and the copycat
/// flags (2026-11-21: it read every chain twice before).
pub fn card(node: &Node, chain: &str, txid: &str) -> Result<Value, String> {
    let snap = gather(node);
    let (a, id) = find(&snap, chain, txid)?;
    let fl = flags_of(node, &snap);
    let acts = crate::feedsvc::ledger_rows(node, arcade_ledger::messages::ACT);
    let fx = fixes(&acts);
    let installed: BTreeSet<String> = installed_ids(node).iter().filter_map(|x| x["id"].as_str().map(String::from)).collect();
    let mut e = entry(node, a, &acts, &fx, &me(node), fl.get(&a.versions[0].txid), &installed, &starred_ids(node));
    let txids: BTreeSet<String> = a.versions.iter().map(|v| v.txid.clone()).collect();
    let comments = comments_on(&acts, &fx, &txids);
    e["comments"] = json!(comments.iter().map(|a| json!({"from": a["from"], "text": a["text"], "txid": a["txid"], "height": a["h"], "stars": a["stars"]})).collect::<Vec<_>>());
    e["permissions"] = perms(node, &id);
    Ok(e)
}

/// Per-app permission switches (only the ones its card claims matter).
pub fn perms(node: &Node, id: &str) -> Value {
    let v = meta_get(node, &format!("appperm:{id}"));
    let mut v = if v.is_object() { v } else { json!({"wallet": false, "contacts": false, "groups": false, "feed": false, "media": false, "pictures": false, "notify": false}) };
    // The folder the person gave the app (crate::foldersvc, 2026-11-21): {granted, name}; its path is never shown, and
    // it is kept on its own (appfolder:<id>), not with the switches.
    v["folder"] = crate::foldersvc::status(node, id);
    v
}

pub fn set_perms(node: &Node, id: &str, set: &Value) -> Result<Value, String> {
    let mut v = perms(node, id);
    for k in ["wallet", "contacts", "groups", "feed", "media", "pictures", "notify"] {
        if let Some(b) = set[k].as_bool() {
            v[k] = json!(b);
        }
    }
    // Spending allowances (2026-11-21, Martin): {service: ARCADE a day, or null/0 to take it away}.
    if let Some(m) = set["allowances"].as_object() {
        let mut al = v["allowances"].as_object().cloned().unwrap_or_default();
        for (svc, n) in m {
            if !svc_name_ok(svc) {
                return Err(format!("allowances: {svc} is no service name"));
            }
            match n.as_f64().filter(|x| *x > 0.0) {
                Some(x) if x <= ALLOWANCE_MOST => {
                    al.insert(svc.clone(), json!((x * 1e8).round() as u64));
                }
                Some(_) => return Err(format!("allowances: at most {ALLOWANCE_MOST} ARCADE a day")),
                None => {
                    al.remove(svc);
                }
            }
        }
        v["allowances"] = json!(al);
    }
    // The folder (2026-11-21): only taken away here (folder: false); it is given by apps.folder_set, after the person
    // picks one on the app window's card.
    if set["folder"] == false {
        crate::foldersvc::revoke(node, id)?;
    }
    if let Some(o) = v.as_object_mut() {
        o.remove("folder");
    }
    meta_put(node, &format!("appperm:{id}"), &v)?;
    Ok(perms(node, id))
}

// ---------------- spending allowances (2026-11-21) ----------------
//
// Martin: an app "should be given an allowance to spend on her own inference". Any app, any service: the person gives
// an app up to N ARCADE a day for one service (the AI models', say); a payment the app asks for within what is left today
// goes without a card, one beyond it asks as before. The allowance is kept with the app's permissions (`allowances`,
// satoshis a day per service); what was spent, per app, service and day (UTC), in `appspend:`. The window reserves a
// payment here before it pays (`allowance_use`), so two payments at once can't both fit what only one does, and gives it
// back when the payment fails (`undo`).

/// The most a day an allowance may be, in ARCADE.
const ALLOWANCE_MOST: f64 = 10_000.0;
static SPENDING: Mutex<()> = Mutex::new(());

fn svc_name_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

fn today() -> u64 {
    arcade_app::now() / 86_400
}

fn spent_key(id: &str, service: &str, day: u64) -> String {
    format!("appspend:{id}:{service}:{day}")
}

/// An app's allowance for a service: {service, per_day, spent_today, left, payments} (ARCADE; payments today, newest
/// first, at most 50).
pub fn allowance(node: &Node, id: &str, service: &str) -> Result<Value, String> {
    if !svc_name_ok(service) {
        return Err("service: 1-32 of a-z, 0-9 and -".into());
    }
    let per_day = perms(node, id)["allowances"][service].as_u64().unwrap_or(0);
    let spent = meta_get(node, &spent_key(id, service, today()));
    let sats = spent["sats"].as_u64().unwrap_or(0);
    Ok(json!({"id": id, "service": service, "per_day": per_day as f64 / 1e8, "spent_today": sats as f64 / 1e8,
              "left": per_day.saturating_sub(sats) as f64 / 1e8, "payments": spent["payments"].clone()}))
}

/// Reserve `amount` ARCADE of today's allowance for a payment the app asks for: {within: true, ...} when it fits (it
/// counts at once), {within: false, ...} when not (nothing counted: the window asks on a card). `undo` gives a
/// reservation back (the payment failed).
pub fn allowance_use(node: &Node, id: &str, service: &str, amount: f64, note: &str, undo: bool) -> Result<Value, String> {
    if !(amount > 0.0) || !amount.is_finite() {
        return Err("amount: ARCADE above zero".into());
    }
    let want = (amount * 1e8).round() as u64;
    let _one = SPENDING.lock().unwrap();
    let per_day = perms(node, id)["allowances"][service].as_u64().unwrap_or(0);
    let key = spent_key(id, service, today());
    let mut spent = meta_get(node, &key);
    let sats = spent["sats"].as_u64().unwrap_or(0);
    if undo {
        spent["sats"] = json!(sats.saturating_sub(want));
        let mut p: Vec<Value> = spent["payments"].as_array().cloned().unwrap_or_default();
        p.insert(0, json!({"at": arcade_app::now(), "amount": -amount, "note": "given back: the payment failed"}));
        p.truncate(50);
        spent["payments"] = json!(p);
        meta_put(node, &key, &spent)?;
        drop(_one);
        return allowance(node, id, service);
    }
    let within = per_day > 0 && sats + want <= per_day;
    if within {
        spent["sats"] = json!(sats + want);
        let mut p: Vec<Value> = spent["payments"].as_array().cloned().unwrap_or_default();
        p.insert(0, json!({"at": arcade_app::now(), "amount": amount, "note": note.chars().take(140).collect::<String>()}));
        p.truncate(50);
        spent["payments"] = json!(p);
        meta_put(node, &key, &spent)?;
    }
    drop(_one);
    let mut a = allowance(node, id, service)?;
    a["within"] = json!(within);
    Ok(a)
}

/// Ids of the apps whose Notifications switch is on.
pub fn notify_ids(node: &Node) -> Vec<String> {
    let rows = node.app.store.read(|r| r.scan_prefix(arcade_app::META, b"appperm:")).unwrap_or_default();
    rows.into_iter()
        .filter(|(_, v)| serde_json::from_slice::<Value>(v).is_ok_and(|v| v["notify"] == true))
        .filter_map(|(k, _)| String::from_utf8(k).ok().map(|k| k["appperm:".len()..].to_string()))
        .collect()
}

/// The app a page belongs to (the txid of any of its versions, on any chain kept here):
/// its id, name and chain, and the txid its card goes by.
pub fn of_page(node: &Node, txid: &str) -> Option<Value> {
    let snap = gather(node);
    let a = snap.apps.iter().find(|a| a.versions.iter().any(|v| v.txid == txid))?;
    let first = &a.versions[0];
    // The page's own version: one arcade's word for it is said where the app asks for money (walletsvc::page_card).
    let this = a.versions.iter().find(|v| v.txid == txid).unwrap_or(first);
    Some(json!({"id": app_id(a.chain.key, &a.root, &first.card.key()), "name": first.card.name, "chain": a.chain.key, "txid": first.txid,
                "creator": this.creator, "trust": trust_json(this)}))
}

/// Operator's verdict on a flagged card: "clear" (show it) or "confirm" (keep it hidden).
pub fn review(node: &Node, chain: &str, txid: &str, verdict: &str) -> Result<Value, String> {
    let v = match verdict {
        "clear" => "cleared",
        "confirm" => "confirmed",
        _ => return Err("verdict is clear or confirm".into()),
    };
    meta_put(node, &format!("cardwatch:{chain}:{txid}"), &json!(v))?;
    // The kept copycat flags are made again with this verdict (2026-11-21).
    invalidate_flags(node);
    Ok(json!({"txid": txid, "status": v}))
}

pub fn trust(node: &Node, address: &str, on: bool) -> Result<Value, String> {
    let mut t: BTreeSet<String> = meta_get(node, "apps_trusted").as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
    if on { t.insert(address.to_string()); } else { t.remove(address); }
    meta_put(node, "apps_trusted", &json!(t.iter().collect::<Vec<_>>()))?;
    invalidate_flags(node);
    Ok(json!({"trusted": t}))
}

/// The apps on `p` with a version `txid` or the family (or name) `family`, withdrawn ones too: (creator, newest card,
/// every offered version's txid). For apps.withdraw (crate::apppub), which checks the creator.
pub fn lookup(node: &Node, p: &ChainParams, txid: Option<&str>, family: Option<&str>) -> Vec<(String, Card, Vec<String>)> {
    gather(node)
        .apps
        .iter()
        .filter(|a| a.chain.chain_no == p.chain_no)
        .filter(|a| match (txid, family) {
            (Some(t), _) => a.versions.iter().any(|v| v.txid == t),
            (None, Some(f)) => a.versions[0].card.family == f || a.versions[0].card.key() == f.to_lowercase(),
            _ => false,
        })
        .map(|a| (a.owner.clone(), a.versions[0].card.clone(), a.versions.iter().map(|v| v.txid.clone()).collect()))
        .collect()
}

/// What the app window needs: the page to run (served sandboxed by this arcade, with its pass for the page API: the
/// address carries the page's key, crate::http::page_key) and the card.
pub fn launch(node: &Node, chain: &str, txid: &str) -> Result<Value, String> {
    let e = card(node, chain, txid)?;
    if e["removed"] == true {
        return Err("its publisher removed this app".into());
    }
    let port = node.http_port.get().copied().unwrap_or(8420);
    let opens = e["opens"].as_str().unwrap_or("").to_ascii_lowercase();
    let key = crate::chains::find(chain).map(|cp| crate::http::page_key(node, cp.key, &opens)).unwrap_or_default();
    // One arcade's word for the version that opens: it still opens, with a caution the app window shows.
    // The page and its app, for the page API's folder routes (crate::foldersvc, 2026-11-21): a page's pass names only the
    // page, and looking its app up through the whole App Store takes minutes on a busy arcade.
    crate::foldersvc::remember_page(&opens, e["id"].as_str().unwrap_or(""));
    let caution = (e["trust"]["level"] == "yellow").then(|| e["trust"]["note"].clone());
    // Withdrawn by its publisher: it still opens (an installed copy is never taken away), and the window says so.
    let withdrawn = e["withdrawn"].as_object().map(|w| json!({"reason": w.get("reason"), "replaced_by": w.get("replaced_by")}));
    Ok(json!({
        "caution": caution, "withdrawn": withdrawn,
        "id": e["id"], "title": format!("{}{}", e["card"]["name"].as_str().unwrap_or("App"), e["card"]["version"].as_str().filter(|v| !v.is_empty()).map(|v| format!(" {v}")).unwrap_or_default()),
        "frame": format!("http://127.0.0.1:{port}/app/{chain}/{opens}?pass={key}"),
        "card": e["card"], "permissions": e["permissions"], "creator": e["creator"], "chain": chain,
    }))
}

// ---------------- arcade.apps: the apps for app pages (2026-11-22) ----------------
//
// An app page lists the arcade's apps (GET /r/apps, arcade.apps.list) and asks its window to open one (arcade.apps.open:
// the window asks the person, then apps.open). Public card data only, made once per kept copy of the apps (never a walk
// of the store per request), and bounded. Cards carry an icon (`cover`) and no screenshots.

/// Rows a page gets at most in one answer, and by default.
pub const PAGE_ROWS_MAX: usize = 500;
pub const PAGE_ROWS_DEFAULT: usize = 100;

/// The rows of every app the store lists (not removed, not withdrawn, not held back as a lookalike), by name: {id, chain,
/// chain_name, txid (its newest card), name, version, category, about, by (the publisher's @tag or null), creator, owner,
/// icon (the cover's address on this arcade, or null), trust}. `tag`: an app's publisher's @tag.
fn page_rows_of(apps: &[App], fl: &BTreeMap<String, String>, tag: impl Fn(&App) -> Option<String>) -> Vec<Value> {
    let mut out: Vec<Value> = apps
        .iter()
        .filter(|a| !a.removed && a.withdrawn.is_none())
        .filter(|a| !fl.get(&a.versions[0].txid).is_some_and(|f| f.starts_with("flagged") || f == "confirmed"))
        .map(|a| {
            let n = &a.versions[0];
            let c = &n.card;
            json!({
                "id": app_id(a.chain.key, &a.root, &c.key()), "chain": a.chain.key, "chain_name": a.chain.name, "txid": n.txid,
                "name": c.name, "version": c.version, "category": c.category, "about": c.description,
                "by": tag(a), "creator": n.creator, "owner": a.owner,
                "icon": (!c.cover.is_empty()).then(|| format!("/app/{}/{}", a.chain.key, c.cover)),
                "trust": n.trust.as_str(),
            })
        })
        .collect();
    out.sort_by(|a, b| {
        let k = |v: &Value| (v["name"].as_str().unwrap_or("").to_lowercase(), v["id"].as_str().unwrap_or("").to_string());
        k(a).cmp(&k(b))
    });
    out
}

/// [`page_rows_of`] for this arcade's kept copy of the apps, made once per copy and flags.
fn page_rows(node: &Node) -> Arc<Vec<Value>> {
    let snap = gather(node);
    let fl = flags_of(node, &snap);
    let mut kept = node.apps_cache.rows.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((id, f, rows)) = &*kept {
        if *id == snap.id && Arc::ptr_eq(f, &fl) {
            return rows.clone();
        }
    }
    let rows = Arc::new(page_rows_of(&snap.apps, &fl, |a| creator_tag(node, a.chain, &a.versions[0].creator)));
    *kept = Some((snap.id, fl, rows.clone()));
    rows
}

/// The rows a page asked for: those matching `q` (name, about, category, publisher's tag; "" all), from `offset`, at most
/// `limit` (capped at [`PAGE_ROWS_MAX`]), each with `installed` (installed here). {apps, total (matching), more}.
fn page_list_of(rows: &[Value], installed: &BTreeSet<String>, q: &str, offset: usize, limit: usize) -> Value {
    let q = q.trim().to_lowercase();
    let hit = |r: &&Value| {
        q.is_empty() || ["name", "about", "category", "by"].iter().any(|k| r[*k].as_str().is_some_and(|s| s.to_lowercase().contains(&q)))
    };
    let found: Vec<&Value> = rows.iter().filter(hit).collect();
    let limit = limit.clamp(1, PAGE_ROWS_MAX);
    let page: Vec<Value> = found.iter().skip(offset).take(limit).map(|r| {
        let mut r = (*r).clone();
        r["installed"] = json!(r["id"].as_str().is_some_and(|i| installed.contains(i)));
        r
    }).collect();
    json!({"apps": page, "total": found.len(), "more": found.len() > offset.saturating_add(page.len())})
}

/// The apps for app pages (GET /r/apps, apps.rows): see [`page_list_of`].
pub fn page_list(node: &Node, q: &str, offset: usize, limit: usize) -> Value {
    let installed: BTreeSet<String> = installed_ids(node).iter().filter_map(|x| x["id"].as_str().map(String::from)).collect();
    page_list_of(&page_rows(node), &installed, q, offset, limit)
}

/// Which app an app page may open (arcade.apps.open): one the store lists, never itself (`from`: the asking app's id).
/// (chain, txid) of its newest card, or why not.
fn open_target(rows: &[Value], id: &str, from: &str) -> Result<(String, String), String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("which app? give its id (from arcade.apps.list)".into());
    }
    if !from.is_empty() && id == from {
        return Err("an app can't open itself: it is already open".into());
    }
    let r = rows.iter().find(|r| r["id"] == id).ok_or("no app with that id on this arcade")?;
    Ok((r["chain"].as_str().unwrap_or("").to_string(), r["txid"].as_str().unwrap_or("").to_string()))
}

/// apps.open: what the window of app `from` (its id; or `page`, its page's txid) needs to open app `id` in its own window
/// (apps.launch's answer), after the person said yes there. Refused for the asking app itself and for an id the store
/// doesn't list.
pub fn open(node: &Node, id: &str, from: &str, page: &str) -> Result<Value, String> {
    let from = if from.is_empty() && !page.is_empty() {
        of_page(node, page).and_then(|a| a["id"].as_str().map(String::from)).unwrap_or_default()
    } else {
        from.to_string()
    };
    let (chain, txid) = open_target(&page_rows(node), id, &from)?;
    launch(node, &chain, &txid)
}

// ---------------- arcade.storage: saves per app page (the original's pagestore limits) ----------------

const MAX_KEY: usize = 256;
const MAX_VALUE: usize = 65_536;
const MAX_TOTAL: usize = 1_048_576;
const MAX_KEYS: usize = 1000;
const HISTORY: usize = 10;

fn shelf(node: &Node, page: &str) -> (serde_json::Map<String, Value>, serde_json::Map<String, Value>) {
    let items = meta_get(node, &format!("pagestore:{page}")).as_object().cloned().unwrap_or_default();
    let hist = meta_get(node, &format!("pagehist:{page}")).as_object().cloned().unwrap_or_default();
    (items, hist)
}

/// One storage op from an app page (load, set, remove, clear, history).
pub fn storage(node: &Node, page: &str, op: &str, key: Option<&str>, value: Option<&str>) -> Result<Value, String> {
    if !crate::udpsvc::is_hex(page, 64) {
        return Err("no such inscription".into());
    }
    let (mut items, mut hist) = shelf(node, page);
    let now = arcade_app::now() as f64;
    let file = |hist: &mut serde_json::Map<String, Value>, k: &str, old: &Value| {
        let mut l = hist.get(k).and_then(|x| x.as_array().cloned()).unwrap_or_default();
        l.insert(0, json!({"value": old, "updated": Value::Null, "replaced": now}));
        l.truncate(HISTORY);
        hist.insert(k.to_string(), json!(l));
    };
    match op {
        "load" => return Ok(json!({"items": items})),
        "history" => {
            return Ok(json!({"history": match key.filter(|k| !k.is_empty()) {
                Some(k) => hist.get(k).map(|h| json!({k: h})).unwrap_or(json!({})),
                None => Value::Object(hist),
            }}))
        }
        "set" => {
            let k = key.unwrap_or("");
            let v = value.unwrap_or("");
            if k.is_empty() || k.chars().count() > MAX_KEY {
                return Err("a key is 1 to 256 characters".into());
            }
            if v.chars().count() > MAX_VALUE {
                return Err("a value is at most 65,536 characters".into());
            }
            if !items.contains_key(k) && items.len() >= MAX_KEYS {
                return Err("a page may keep at most 1000 keys".into());
            }
            let others: usize = items.iter().filter(|(x, _)| x.as_str() != k).map(|(x, y)| x.chars().count() + y.as_str().unwrap_or("").chars().count()).sum();
            if others + k.chars().count() + v.chars().count() > MAX_TOTAL {
                return Err("a page may keep at most 1,048,576 characters in all".into());
            }
            if let Some(old) = items.get(k).cloned() {
                if old != v {
                    file(&mut hist, k, &old);
                }
            }
            items.insert(k.to_string(), json!(v));
        }
        "remove" => {
            let k = key.unwrap_or("");
            if let Some(old) = items.remove(k) {
                file(&mut hist, k, &old);
            }
        }
        "clear" => {
            for (k, old) in std::mem::take(&mut items) {
                file(&mut hist, &k, &old);
            }
        }
        _ => return Err("op must be set, remove, clear or history".into()),
    }
    meta_put(node, &format!("pagestore:{page}"), &Value::Object(items))?;
    meta_put(node, &format!("pagehist:{page}"), &Value::Object(hist))?;
    Ok(json!({}))
}

#[cfg(test)]
mod rating_tests {
    use super::*;

    #[test]
    fn reviews_carry_stars_and_the_latest_counts() {
        assert_eq!(stars_of("★★★★☆ works well"), Some((4, "works well")));
        assert_eq!(stars_of("★★★★★"), Some((5, "")));
        assert_eq!(stars_of("★☆☆☆☆meh"), Some((1, "meh")));
        for no in ["☆☆☆☆☆ zero", "★★☆★☆ mixed", "★★★★ four marks", "great ★★★★★", ""] {
            assert_eq!(stars_of(no), None, "{no}");
        }
        let app: BTreeSet<String> = ["a1".to_string()].into();
        let act = |kind: u64, txid: &str, from: &str, target: &str, text: &str, h: u64| json!({"kind": kind, "txid": txid, "from": from, "target": target, "text": text, "h": h});
        let acts = vec![
            act(3, "r1", "bob", "a1", "★★☆☆☆ crashes", 10),
            act(3, "r2", "bob", "a1", "★★★★★ fixed now", 12),      // bob's latest counts: 5
            act(3, "r3", "amy", "a1", "★★★☆☆ ok", 11),
            act(5, "e1", "amy", "r3", "★★★★☆ better", 13),         // amy edits hers: 4
            act(3, "r4", "cat", "a1", "★☆☆☆☆ no", 11),
            act(6, "d1", "cat", "r4", "", 14),                     // cat takes hers back
            act(6, "d2", "bob", "r3", "", 14),                     // not bob's to delete
            act(3, "r5", "dan", "a1", "nice app", 11),             // a comment without stars
        ];
        let c = comments_on(&acts, &fixes(&acts), &app);
        assert_eq!(c.iter().map(|x| x["txid"].as_str().unwrap()).collect::<Vec<_>>(), ["r1", "r3", "r5", "r2"]);
        let r = ratings(&c, "amy");
        assert_eq!((r["count"].as_u64(), r["average"].as_f64(), r["mine"].as_u64()), (Some(2), Some(4.5), Some(4)));
        assert_eq!(r["dist"], json!([0, 0, 0, 1, 1]));
        assert_eq!(r["my_review"], "r3");
        let s = social(&acts, &fixes(&acts), &app, "bob");
        assert_eq!(s["rating"]["mine"], 5);
        assert_eq!(s["score"].as_f64(), Some(0.5 * 4.0 + 1.0 + 0.5));
        assert_eq!(s["rating"]["my_reviews"], json!(["r1", "r2"]));
    }

    fn act(kind: u64, txid: &str, from: &str, target: &str, text: &str, h: u64) -> Value {
        json!({"kind": kind, "txid": txid, "from": from, "target": target, "text": text, "h": h})
    }

    fn rated(acts: &[Value], who: &str) -> Value {
        let app: BTreeSet<String> = ["a1".to_string()].into();
        ratings(&comments_on(acts, &fixes(acts), &app), who)
    }

    /// What the store's buttons send: "Change my rating" edits `my_review` (a new review only when
    /// there is none), "Remove my rating" deletes every one in `my_reviews`.
    #[test]
    fn change_then_remove_leaves_no_rating() {
        let mut acts = vec![act(3, "r1", "bob", "a1", "★★☆☆☆ crashes", 10)];
        let r = rated(&acts, "bob");
        assert_eq!((r["mine"].as_u64(), r["my_review"].as_str()), (Some(2), Some("r1")));
        // Change to 5: an edit of the review that counts.
        acts.push(act(5, "e1", "bob", "r1", "★★★★★ fixed now", 11));
        let r = rated(&acts, "bob");
        assert_eq!((r["mine"].as_u64(), r["count"].as_u64(), r["my_review"].as_str()), (Some(5), Some(1), Some("r1")));
        assert_eq!(r["dist"], json!([0, 0, 0, 0, 1]));
        // A later edit wins over an earlier one, and an unconfirmed one over both.
        acts.push(act(5, "e2", "bob", "r1", "★★★☆☆ hmm", 0));
        assert_eq!(rated(&acts, "bob")["mine"], 3);
        // Remove: every starred review of bob's goes.
        for t in rated(&acts, "bob")["my_reviews"].as_array().unwrap().clone() {
            acts.push(act(6, &format!("d-{}", t.as_str().unwrap()), "bob", t.as_str().unwrap(), "", 12));
        }
        let r = rated(&acts, "bob");
        assert_eq!((r["count"].as_u64(), r["mine"].as_u64(), r["my_review"].as_str()), (Some(0), None, None));
        assert_eq!(r["my_reviews"], json!([]));

        // Reviews from before "change" was an edit: rate 2, rate 5 as a new review, remove.
        let mut acts = vec![act(3, "r1", "bob", "a1", "★★☆☆☆", 10), act(3, "r2", "bob", "a1", "★★★★★", 11)];
        let r = rated(&acts, "bob");
        assert_eq!((r["mine"].as_u64(), r["count"].as_u64(), r["my_reviews"].clone()), (Some(5), Some(1), json!(["r1", "r2"])));
        // Deleting only the latest would bring the 2 back...
        let mut only_latest = acts.clone();
        only_latest.push(act(6, "d2", "bob", "r2", "", 12));
        assert_eq!(rated(&only_latest, "bob")["mine"], 2);
        // ...so remove deletes them all.
        acts.push(act(6, "d1", "bob", "r1", "", 12));
        acts.push(act(6, "d2", "bob", "r2", "", 12));
        assert_eq!(rated(&acts, "bob")["count"], 0);
    }

    /// The per-comment scan the edit/delete index replaced, kept to check they agree.
    fn comments_on_scan(acts: &[Value], txids: &BTreeSet<String>) -> Vec<Value> {
        let k = |a: &Value| a["kind"].as_u64().unwrap_or(0);
        let mut out: Vec<Value> = acts.iter().filter(|a| k(a) == 3 && txids.contains(a["target"].as_str().unwrap_or(""))).cloned().collect();
        out.retain(|c| !acts.iter().any(|a| k(a) == 6 && a["target"] == c["txid"] && a["from"] == c["from"]));
        for c in out.iter_mut() {
            if let Some(e) = acts.iter().filter(|a| k(a) == 5 && a["target"] == c["txid"] && a["from"] == c["from"]).max_by_key(|a| act_order(a)) {
                c["text"] = e["text"].clone();
            }
            if let Some((n, _)) = stars_of(c["text"].as_str().unwrap_or("")) {
                c["stars"] = json!(n);
            }
        }
        out.sort_by_key(act_order);
        out
    }

    #[test]
    fn indexed_comments_match_the_scan() {
        // A made-up feed: comments on three apps by five people, with edits (some unconfirmed, some
        // by someone else), deletes (some not the writer's), likes and replies to replies.
        let people = ["amy", "bob", "cat", "dan", "eve"];
        let texts = ["★★☆☆☆ meh", "★★★★★", "nice", "★☆☆☆☆ broken", "★★★★☆ good"];
        let mut acts = Vec::new();
        let mut seed: u64 = 7;
        let mut next = |m: u64| { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) % m };
        for i in 0..400u64 {
            let from = people[next(5) as usize];
            let h = if next(10) == 0 { 0 } else { 1 + next(50) };
            let txid = format!("t{i:03}");
            let comments: Vec<String> = acts.iter().filter(|a: &&Value| a["kind"] == 3).map(|a| a["txid"].as_str().unwrap().to_string()).collect();
            let pick = |n: u64| comments.get(n as usize).cloned().unwrap_or_else(|| "a1".into());
            let a = match next(6) {
                0 | 1 => act(3, &txid, from, ["a1", "a2", "a3"][next(3) as usize], texts[next(5) as usize], h),
                2 | 3 => { let n = next(comments.len().max(1) as u64); act(5, &txid, from, &pick(n), texts[next(5) as usize], h) }
                4 => { let n = next(comments.len().max(1) as u64); act(6, &txid, from, &pick(n), "", h) }
                _ => act(1, &txid, from, "a1", "", h),
            };
            acts.push(a);
        }
        let fx = fixes(&acts);
        for app in [vec!["a1"], vec!["a2", "a3"], vec!["a1", "a2", "a3"], vec!["none"]] {
            let txids: BTreeSet<String> = app.iter().map(|s| s.to_string()).collect();
            let fast = comments_on(&acts, &fx, &txids);
            assert_eq!(fast, comments_on_scan(&acts, &txids), "{app:?}");
            for who in people {
                assert_eq!(ratings(&fast, who), ratings(&comments_on_scan(&acts, &txids), who));
            }
        }
        // The feed is busy enough to mean something.
        let all: BTreeSet<String> = ["a1", "a2", "a3"].iter().map(|s| s.to_string()).collect();
        let c = comments_on(&acts, &fx, &all);
        assert!(c.len() > 20 && c.iter().any(|x| x["stars"].is_u64()), "{}", c.len());
        assert!(fx.edits.len() > 10 && fx.deletes.len() > 10);
    }
}

#[cfg(test)]
mod trust_tests {
    use super::*;
    use crate::vouch::Level;

    fn row(txid: &str, creator: &str, h: u64) -> Value {
        json!({"txid": txid, "number": 1, "creator": creator, "json": r#"{"app":{"name":"Frogger"}}"#, "block_height": h, "position": 0})
    }

    #[test]
    fn mesh_cards_carry_how_many_holders_vouch() {
        // Two holders send the same card, one adds a card only it knows, and one sends a card another sent with a
        // different creator and a higher height (a "newest version" under someone else's name).
        let a = vec![row("aa", "Dreal", 10), row("bb", "Dreal", 20)];
        let b = vec![row("aa", "Dreal", 10), row("cc", "Dreal", 30)];
        let c = vec![row("bb", "Dfake", 99)];
        let m = merge_card_rows(&[(a, "n1".into()), (b, "n2".into()), (c, "n3".into())]);
        let by = |t: &str| m.iter().find(|r| r["txid"] == t).unwrap().clone();
        assert_eq!(m.len(), 3);
        assert_eq!(vouched(&by("aa")), Some((Level::Green, 2, false)));
        assert_eq!(vouched(&by("cc")), Some((Level::Yellow, 1, false)));
        // Disagreement: red, not offered.
        assert_eq!(by("bb")["trust"], "red");
        assert!(by("bb")["why"].as_str().unwrap().contains("differently"));
        assert_eq!(vouched(&by("bb")), None);
        // Checked against its transaction: still one holder's word for its place, but its creator is the chain's.
        let mut cc = by("cc");
        cc["checked"] = json!(true);
        assert_eq!(vouched(&cc), Some((Level::Yellow, 1, true)));
        // A card its transaction contradicts is not offered either.
        cc["checked"] = json!(false);
        assert_eq!(vouched(&cc), None);
        // This arcade's own ledger rows carry no holders: green, checked.
        assert_eq!(vouched(&row("dd", "Dreal", 1)), Some((Level::Green, 0, true)));
    }

    #[test]
    fn one_holders_version_says_so() {
        let v = |holders: usize, checked: bool| Ver {
            txid: "aa".into(),
            number: 1,
            creator: "Dreal".into(),
            height: 1,
            position: 0,
            card: cards::parse(r#"{"app":{"name":"Frogger"}}"#).unwrap(),
            trust: if holders == 0 { Level::Green } else { Level::of_sources(holders) },
            holders,
            checked,
        };
        let t = trust_json(&v(1, false));
        assert_eq!((t["level"].as_str(), t["checked"].as_bool()), (Some("yellow"), Some(false)));
        assert!(t["note"].as_str().unwrap().contains("Double-check"));
        assert_eq!(trust_json(&v(2, true))["level"], "green");
        assert_eq!(trust_json(&v(0, true))["note"], "From this arcade's own ledger.");
    }
}

#[cfg(test)]
mod withdraw_tests {
    use super::*;

    fn ver(txid: &str, creator: &str, h: u64, card: &str) -> Ver {
        Ver { txid: txid.into(), number: h, creator: creator.into(), height: h, position: 0, card: cards::parse(card).unwrap(),
              trust: crate::vouch::Level::Green, holders: 0, checked: true }
    }
    fn chain() -> &'static ChainParams {
        arcade_arc::store::chain_params(&arcade_arc::params::TESTNET, &arcade_chain::Hash256::ZERO)
    }
    /// One creator's cards (any order) as gather files them: newest first, resolved.
    fn app(mut vs: Vec<Ver>) -> Option<App> {
        vs.sort_by(|a, b| (b.height, b.position).cmp(&(a.height, a.position)));
        let root = vs.last().map(|v| v.creator.clone()).unwrap_or_default();
        resolve(vs).map(|(versions, withdrawn)| App { chain: chain(), root: root.clone(), owner: root, versions, removed: false, withdrawn })
    }
    const F1: &str = r#"{"app":{"name":"Frog Pond","family":"frog-pond","version":"1"}}"#;
    const F2: &str = r#"{"app":{"name":"Frog Pond","family":"frog-pond","version":"2"}}"#;
    const WHOLE: &str = r#"{"app":{"name":"Frog Pond","family":"frog-pond","withdrawn":true,"reason":"retired","replaced_by":"frog-lake"}}"#;
    fn only(txid: &str) -> String {
        format!(r#"{{"app":{{"name":"Frog Pond","family":"frog-pond","withdrawn":true,"versions":["{txid}"]}}}}"#)
    }
    fn t(c: char) -> String {
        c.to_string().repeat(64)
    }

    #[test]
    fn a_withdrawal_card_hides_the_family() {
        let a = app(vec![ver(&t('1'), "X", 10, F1), ver(&t('2'), "X", 20, F2), ver(&t('3'), "X", 30, WHOLE)]).unwrap();
        let w = a.withdrawn.as_ref().expect("withdrawn whole");
        assert_eq!((w["reason"].as_str(), w["replaced_by"].as_str(), w["txid"].as_str()), (Some("retired"), Some("frog-lake"), Some(t('3').as_str())));
        assert_eq!(a.versions.iter().map(|v| v.txid.clone()).collect::<Vec<_>>(), [t('2'), t('1')], "the withdrawal card is no version");
        // A newer real version after the withdrawal lists it again.
        let back = app(vec![ver(&t('1'), "X", 10, F1), ver(&t('3'), "X", 30, WHOLE), ver(&t('4'), "X", 40, F2)]).unwrap();
        assert!(back.withdrawn.is_none());
        assert_eq!(back.versions[0].txid, t('4'));
    }

    #[test]
    fn withdrawing_some_versions_hides_only_those() {
        let a = app(vec![ver(&t('1'), "X", 10, F1), ver(&t('2'), "X", 20, F2), ver(&t('3'), "X", 30, &only(&t('2')))]).unwrap();
        assert!(a.withdrawn.is_none(), "the app stays listed");
        assert_eq!(a.versions.iter().map(|v| v.txid.clone()).collect::<Vec<_>>(), [t('1')]);
        // Every version withdrawn one by one: nothing is left to offer.
        assert!(app(vec![ver(&t('1'), "X", 10, F1), ver(&t('3'), "X", 30, &only(&t('1')))]).is_none());
    }

    #[test]
    fn another_creators_withdrawal_does_not_count() {
        // gather groups by creator: Y's withdrawal card is its own group, with no version to offer, and X's app is as
        // it was.
        let x = app(vec![ver(&t('1'), "X", 10, F1)]).unwrap();
        assert!(x.withdrawn.is_none());
        assert!(app(vec![ver(&t('9'), "Y", 50, WHOLE)]).is_none(), "a withdrawal alone offers nothing");
        let y = app(vec![ver(&t('8'), "Y", 40, F2), ver(&t('9'), "Y", 50, &only(&t('1')))]).unwrap();
        assert_eq!(y.versions.len(), 1, "Y's list of X's version is no withdrawal of it");
        assert_eq!(comparable(std::slice::from_ref(&x), |_| false).len(), 1);
    }

    #[test]
    fn a_family_its_owner_withdrew_blocks_no_new_app() {
        let y = || app(vec![ver(&t('7'), "Y", 60, F1)]).unwrap();
        let flagged = |x: App| {
            let apps = [x, y()];
            lookalike(&apps[1].versions[0], &apps[1].root, &comparable(&apps, |_| false))
        };
        // Without X's withdrawal, Y's later card looks like X's app: flagged.
        assert!(flagged(app(vec![ver(&t('1'), "X", 10, F1)]).unwrap()).is_some_and(|f| f.starts_with("flagged")));
        // X withdrew the whole family: nothing to be mistaken for any more.
        assert_eq!(flagged(app(vec![ver(&t('1'), "X", 10, F1), ver(&t('2'), "X", 20, WHOLE)]).unwrap()), None);
        // Withdrawing only a version leaves the family comparable.
        assert!(flagged(app(vec![ver(&t('1'), "X", 10, F1), ver(&t('5'), "X", 15, F2), ver(&t('2'), "X", 20, &only(&t('5')))]).unwrap()).is_some());
        // A "withdrawal" of X's family by Y is Y's own group (and offers nothing): X's app still blocks.
        assert!(app(vec![ver(&t('6'), "Y", 30, WHOLE)]).is_none());
        assert!(flagged(app(vec![ver(&t('1'), "X", 10, F1)]).unwrap()).is_some());
    }
}

#[cfg(test)]
mod allowance_tests {
    use super::*;

    #[test]
    fn payments_within_the_allowance_count_and_beyond_it_do_not() {
        let (_one, t) = crate::http::tests::node();
        let n = &t.node;
        let id = "test-app-id";
        // No allowance: nothing fits.
        assert_eq!(allowance_use(n, id, "models", 1.0, "", false).unwrap()["within"], false);
        set_perms(n, id, &json!({"allowances": {"models": 5}})).unwrap();
        let a = allowance_use(n, id, "models", 2.0, "credit", false).unwrap();
        assert_eq!(a["within"], true);
        assert_eq!(a["spent_today"], 2.0);
        assert_eq!(a["left"], 3.0);
        // More than is left: refused and not counted.
        let b = allowance_use(n, id, "models", 3.5, "", false).unwrap();
        assert_eq!(b["within"], false);
        assert_eq!(b["spent_today"], 2.0);
        // Given back after a failed payment.
        let c = allowance_use(n, id, "models", 2.0, "", true).unwrap();
        assert_eq!(c["spent_today"], 0.0);
        // Another service has its own (none here), and taking it away stops it.
        assert_eq!(allowance_use(n, id, "speech", 1.0, "", false).unwrap()["within"], false);
        set_perms(n, id, &json!({"allowances": {"models": 0}})).unwrap();
        assert_eq!(allowance_use(n, id, "models", 1.0, "", false).unwrap()["within"], false);
        assert!(set_perms(n, id, &json!({"allowances": {"Bad Name": 1}})).is_err());
        assert!(set_perms(n, id, &json!({"allowances": {"models": 1e9}})).is_err());
        // The other switches are untouched.
        assert_eq!(perms(n, id)["wallet"], false);
    }
}

#[cfg(test)]
mod page_rows_tests {
    use super::*;

    fn chain() -> &'static ChainParams {
        arcade_arc::store::chain_params(&arcade_arc::params::TESTNET, &arcade_chain::Hash256::ZERO)
    }
    fn app(name: &str, creator: &str, txid: char, extra: &str) -> App {
        let card = cards::parse(&format!(r#"{{"app":{{"name":"{name}","description":"About {name}","version":"1"{extra}}}}}"#)).unwrap();
        let v = Ver { txid: txid.to_string().repeat(64), number: 1, creator: creator.into(), height: 1, position: 0, card,
                      trust: crate::vouch::Level::Green, holders: 0, checked: true };
        App { chain: chain(), root: creator.into(), owner: creator.into(), versions: vec![v], removed: false, withdrawn: None }
    }

    #[test]
    fn rows_list_what_the_store_lists_by_name() {
        let cover = "c".repeat(64);
        let mut gone = app("Gone", "x", '3', "");
        gone.removed = true;
        let mut away = app("Away", "x", '4', "");
        away.withdrawn = Some(json!({"reason": "old"}));
        let apps = vec![app("zebra", "x", '1', ""), app("Atlas", "y", '2', &format!(r#","cover":"{cover}""#)), gone, away, app("Copy", "z", '5', "")];
        let fl = BTreeMap::from([("5".repeat(64), "flagged: looks like Atlas".to_string())]);
        let rows = page_rows_of(&apps, &fl, |a| (a.root == "y").then(|| "maker".to_string()));
        let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["Atlas", "zebra"], "removed, withdrawn and held-back apps are not listed");
        let a = &rows[0];
        assert_eq!(a["id"], app_id(chain().key, "y", "atlas"));
        assert_eq!(a["txid"], "2".repeat(64));
        assert_eq!(a["by"], "maker");
        assert_eq!(a["about"], "About Atlas");
        assert_eq!(a["icon"], format!("/app/{}/{cover}", chain().key));
        assert_eq!(rows[1]["icon"], Value::Null);
        assert_eq!(rows[1]["by"], Value::Null);
        assert!(a.get("screenshots").is_none(), "cards carry no screenshots");
        // Search, installed, paging and the cap.
        let installed = BTreeSet::from([a["id"].as_str().unwrap().to_string()]);
        let l = page_list_of(&rows, &installed, "MAKER", 0, 10);
        assert_eq!((l["total"].clone(), l["apps"][0]["installed"].clone(), l["more"].clone()), (json!(1), json!(true), json!(false)));
        let l = page_list_of(&rows, &installed, "", 0, 1);
        assert_eq!((l["apps"].as_array().unwrap().len(), l["more"].clone(), l["total"].clone()), (1, json!(true), json!(2)));
        assert_eq!(page_list_of(&rows, &installed, "", 1, 1)["apps"][0]["installed"], false);
        let many: Vec<Value> = (0..PAGE_ROWS_MAX + 20).map(|i| json!({"id": i.to_string(), "name": "n"})).collect();
        assert_eq!(page_list_of(&many, &BTreeSet::new(), "", 0, usize::MAX)["apps"].as_array().unwrap().len(), PAGE_ROWS_MAX);
    }

    #[test]
    fn opening_refuses_itself_and_unknown_apps() {
        let rows = page_rows_of(&[app("Atlas", "y", '2', ""), app("Notes", "x", '1', "")], &BTreeMap::new(), |_| None);
        let atlas = rows[0]["id"].as_str().unwrap();
        let notes = rows[1]["id"].as_str().unwrap();
        assert_eq!(open_target(&rows, atlas, notes).unwrap(), (chain().key.to_string(), "2".repeat(64)));
        assert!(open_target(&rows, atlas, atlas).unwrap_err().contains("can't open itself"));
        assert!(open_target(&rows, "pep:nobody:nothing", notes).unwrap_err().contains("no app with that id"));
        assert!(open_target(&rows, " ", notes).unwrap_err().contains("which app"));
    }

    /// The node's own rows: made once per copy of the apps.
    #[test]
    fn the_nodes_rows_are_kept() {
        let t = crate::sitesvc::tests::setup();
        let n = &t.node;
        assert!(Arc::ptr_eq(&page_rows(n), &page_rows(n)));
        let l = page_list(n, "", 0, PAGE_ROWS_DEFAULT);
        assert!(l["apps"].is_array() && l["total"].is_u64(), "{l}");
        assert!(open(n, "pep:nobody:nothing", "", "").unwrap_err().contains("no app with that id"));
    }
}

#[cfg(test)]
mod transfer_tests {
    use super::*;

    fn ver(txid: &str, creator: &str, h: u64, card: &str) -> Ver {
        Ver { txid: txid.into(), number: h, creator: creator.into(), height: h, position: 0, card: cards::parse(card).unwrap(),
              trust: crate::vouch::Level::Green, holders: 0, checked: true }
    }
    fn t(c: char) -> String {
        c.to_string().repeat(64)
    }
    const A: &str = r#"{"app":{"name":"Frog Pond","family":"frog-pond","version":"1"}}"#;
    const A2: &str = r#"{"app":{"name":"Frog Pond","family":"frog-pond","version":"2"}}"#;
    const TO_Y: &str = r#"{"app":{"name":"Frog Pond","family":"frog-pond","transfer_to":"Y"}}"#;

    fn group(vs: Vec<Ver>) -> BTreeMap<(String, String), Vec<Ver>> {
        let mut by: BTreeMap<(String, String), Vec<Ver>> = BTreeMap::new();
        for v in vs {
            by.entry((v.creator.clone(), v.card.key())).or_default().push(v);
        }
        by
    }

    #[test]
    fn a_transfer_hands_the_app_on_and_keeps_its_root() {
        // X publishes v1, transfers to Y; Y publishes v2 after. Y's card from before the transfer is no version of it;
        // X's card after the transfer neither.
        let by = group(vec![ver(&t('1'), "X", 10, A), ver(&t('2'), "X", 20, TO_Y), ver(&t('0'), "Y", 5, A2),
                            ver(&t('3'), "Y", 30, A2), ver(&t('4'), "X", 40, A2)]);
        let out = follow_transfers(by);
        let main = out.into_iter().find(|(r, _, _)| r == "X").expect("the app, rooted at X");
        assert_eq!(main.1, "Y", "owned by Y now");
        let mut txids: Vec<String> = main.2.iter().map(|v| v.txid.clone()).collect();
        txids.sort();
        assert_eq!(txids, vec![t('1'), t('2'), t('3')]);
        // Resolved: the transfer card is no version; Y's v2 is the newest.
        let mut vs = main.2;
        vs.sort_by(|a, b| (b.height, b.position).cmp(&(a.height, a.position)));
        let (offered, _) = resolve(vs).unwrap();
        assert_eq!(offered[0].txid, t('3'));
        assert!(offered.iter().all(|v| v.card.transfer_to.is_empty()));
    }

    #[test]
    fn the_new_owners_version_is_no_copy_of_its_own_app() {
        let by = group(vec![ver(&t('1'), "X", 10, A), ver(&t('2'), "X", 20, TO_Y), ver(&t('3'), "Y", 30, A2)]);
        let apps: Vec<App> = follow_transfers(by).into_iter().filter_map(|(root, owner, mut vs)| {
            vs.sort_by(|a, b| (b.height, b.position).cmp(&(a.height, a.position)));
            resolve(vs).map(|(versions, withdrawn)| App { chain: chain(), root, owner, versions, removed: false, withdrawn })
        }).collect();
        assert_eq!(apps.len(), 1);
        let all = comparable(&apps, |_| false);
        assert!(lookalike(&apps[0].versions[0], &apps[0].root, &all).is_none(), "not flagged against its own earlier versions");
        // Somebody else's same-name card later is still a copy.
        let z = ver(&t('5'), "Z", 50, A2);
        assert!(lookalike(&z, "Z", &all).is_some());
    }

    #[test]
    fn transfers_chain_and_a_card_without_one_stays_alone() {
        let to_z = r#"{"app":{"name":"Frog Pond","family":"frog-pond","transfer_to":"Z"}}"#;
        let by = group(vec![ver(&t('1'), "X", 10, A), ver(&t('2'), "X", 20, TO_Y), ver(&t('3'), "Y", 30, to_z), ver(&t('4'), "Z", 40, A2)]);
        let out = follow_transfers(by);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "X");
        assert_eq!(out[0].1, "Z");
        assert_eq!(out[0].2.len(), 4);
    }

    fn chain() -> &'static ChainParams {
        arcade_arc::store::chain_params(&arcade_arc::params::TESTNET, &arcade_chain::Hash256::ZERO)
    }
}

#[cfg(test)]
mod cache_tests {
    //! The kept apps (2026-11-21): one copy within the window, made again after invalidate, one maker at a time.
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    fn chain() -> &'static ChainParams {
        arcade_arc::store::chain_params(&arcade_arc::params::TESTNET, &arcade_chain::Hash256::ZERO)
    }
    /// `n` made-up apps, one version each.
    fn many(n: usize) -> Vec<App> {
        (0..n)
            .map(|i| {
                let card = cards::parse(&format!(r#"{{"app":{{"name":"App {i}","family":"app-{i}","version":"1"}}}}"#)).unwrap();
                let creator = format!("creator{}", i % 97);
                let v = Ver { txid: format!("{i:064x}"), number: i as u64, creator: creator.clone(), height: i as u64, position: 0, card,
                              trust: crate::vouch::Level::Green, holders: 0, checked: true };
                App { chain: chain(), root: creator.clone(), owner: creator, versions: vec![v], removed: false, withdrawn: None }
            })
            .collect()
    }
    fn fp(h: u32) -> Fingerprint {
        Fingerprint { tips: vec![(1, Some((h, [0; 32])))], borrowed: vec![], mesh_cards: 0 }
    }
    /// Puts in a copy made `ago` back, as if read then.
    fn aged(c: &Cache, ago: Duration, f: Fingerprint) -> Arc<Snap> {
        let mut s = c.lock();
        let snap = Arc::new(Snap { apps: many(1), id: SNAP_IDS.fetch_add(1, Ordering::SeqCst), at: Instant::now().checked_sub(ago).unwrap(), fp: f, epoch: s.epoch });
        s.snap = Some(snap.clone());
        snap
    }

    #[test]
    fn two_reads_in_the_window_share_one_copy_and_invalidate_makes_it_again() {
        let c = Cache::default();
        let made = AtomicUsize::new(0);
        let build = || { made.fetch_add(1, Ordering::SeqCst); many(3) };
        let a = c.snapshot(fp(1), build);
        let b = c.snapshot(fp(1), build);
        assert!(Arc::ptr_eq(&a, &b));
        // Within the window a moved tip waits for it to end (at most every 15 s).
        let b2 = c.snapshot(fp(2), build);
        assert!(Arc::ptr_eq(&a, &b2));
        assert_eq!(made.load(Ordering::SeqCst), 1);
        c.invalidate();
        let d = c.snapshot(fp(1), build);
        assert!(!Arc::ptr_eq(&a, &d));
        assert_eq!(made.load(Ordering::SeqCst), 2);
        assert_eq!(d.apps.len(), 3);
        // The flags: made once per copy, again after a verdict, and again for a new copy.
        let fm = AtomicUsize::new(0);
        let mk = || { fm.fetch_add(1, Ordering::SeqCst); BTreeMap::from([("t".to_string(), "flagged".to_string())]) };
        let f1 = c.flags(&d, mk);
        let f2 = c.flags(&d, mk);
        assert!(Arc::ptr_eq(&f1, &f2));
        assert_eq!(fm.load(Ordering::SeqCst), 1);
        c.invalidate_flags();
        let f3 = c.flags(&d, mk);
        assert!(!Arc::ptr_eq(&f1, &f3) && *f1 == *f3);
        assert_eq!(fm.load(Ordering::SeqCst), 2);
        // A write here (invalidate) makes both again.
        c.invalidate();
        let e = c.snapshot(fp(1), build);
        let _ = c.flags(&e, mk);
        assert_eq!((made.load(Ordering::SeqCst), fm.load(Ordering::SeqCst)), (3, 3));
    }

    #[test]
    fn an_older_copy_is_kept_only_while_nothing_it_read_moved() {
        let c = Cache::default();
        let made = AtomicUsize::new(0);
        let build = || { made.fetch_add(1, Ordering::SeqCst); many(1) };
        // Past the window, nothing moved: still used.
        let old = aged(&c, FRESH + Duration::from_secs(5), fp(7));
        assert!(Arc::ptr_eq(&old, &c.snapshot(fp(7), build)));
        assert_eq!(made.load(Ordering::SeqCst), 0);
        // Past the window, a tip moved: made again.
        assert!(!Arc::ptr_eq(&old, &c.snapshot(fp(8), build)));
        assert_eq!(made.load(Ordering::SeqCst), 1);
        // Too old even though nothing moved: made again.
        let old = aged(&c, FRESH_IF_UNCHANGED + Duration::from_secs(1), fp(7));
        assert!(!Arc::ptr_eq(&old, &c.snapshot(fp(7), build)));
        assert_eq!(made.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn callers_at_once_wait_for_one_maker() {
        let c = Arc::new(Cache::default());
        let made = Arc::new(AtomicUsize::new(0));
        let hs: Vec<_> = (0..8)
            .map(|_| {
                let (c, made) = (c.clone(), made.clone());
                std::thread::spawn(move || c.snapshot(fp(1), || { made.fetch_add(1, Ordering::SeqCst); std::thread::sleep(Duration::from_millis(300)); many(2) }))
            })
            .collect();
        let got: Vec<Arc<Snap>> = hs.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(made.load(Ordering::SeqCst), 1);
        assert!(got.iter().all(|s| Arc::ptr_eq(s, &got[0])));
    }

    #[test]
    fn a_slow_remake_hands_out_the_older_copy_after_two_seconds() {
        let c = Arc::new(Cache::default());
        let old = c.snapshot(fp(1), || many(1));
        c.invalidate();
        let c2 = c.clone();
        let maker = std::thread::spawn(move || c2.snapshot(fp(1), || { std::thread::sleep(Duration::from_secs(4)); many(2) }));
        while c.lock().building.is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let t = Instant::now();
        let got = c.snapshot(fp(1), || panic!("only one maker at a time"));
        assert!(Arc::ptr_eq(&got, &old), "the older copy");
        assert!(t.elapsed() < Duration::from_millis(3000), "{:?}", t.elapsed());
        let new = maker.join().unwrap();
        assert_eq!(new.apps.len(), 2);
        assert!(Arc::ptr_eq(&new, &c.snapshot(fp(1), || panic!("the new copy is fresh"))));
    }

    #[test]
    fn a_maker_that_panics_leaves_no_one_waiting() {
        let c = Arc::new(Cache::default());
        let c2 = c.clone();
        let _ = std::thread::spawn(move || c2.snapshot(fp(1), || panic!("reading the chains failed"))).join();
        assert!(c.lock().building.is_none());
        assert_eq!(c.snapshot(fp(1), || many(1)).apps.len(), 1);
    }

    /// A timing note: a kept copy of many apps is handed out in microseconds (the copy is shared, never cloned).
    #[test]
    fn a_kept_copy_of_many_apps_is_a_pointer_copy() {
        let c = Cache::default();
        let t = Instant::now();
        let first = c.snapshot(fp(1), || many(20_000));
        let made = t.elapsed();
        let t = Instant::now();
        for _ in 0..1000 {
            assert!(Arc::ptr_eq(&first, &c.snapshot(fp(1), || panic!("kept"))));
        }
        let kept = t.elapsed() / 1000;
        eprintln!("20000 apps: made in {made:?}, kept copy in {kept:?} a read");
        assert!(kept < Duration::from_millis(5), "{kept:?}");
    }

    /// The node's own reads: one copy for two reads, the same answers from it, and invalidate makes it again.
    #[test]
    fn the_nodes_store_reads_one_copy() {
        let t = crate::sitesvc::tests::setup();
        let n = &t.node;
        let a = gather(n);
        let b = gather(n);
        assert!(Arc::ptr_eq(&a, &b));
        assert!(Arc::ptr_eq(&flags_of(n, &a), &flags_of(n, &b)));
        let before = store(n, "popular", "", false, "");
        invalidate(n);
        let c = gather(n);
        assert!(!Arc::ptr_eq(&a, &c));
        assert_eq!(store(n, "popular", "", false, ""), before);
        // A verdict makes the flags again, not the apps.
        let f = flags_of(n, &c);
        review(n, "x", &"00".repeat(32), "clear").unwrap();
        assert!(Arc::ptr_eq(&c, &gather(n)));
        assert!(!Arc::ptr_eq(&f, &flags_of(n, &c)));
    }
}
