//! Posts, feed acts, message candidates and profiles of a ledger chain this arcade can't read for itself yet: its copy
//! is borrowed, or kept but still catching up (a fresh arcade syncing Pepecoin testnet). They are read from arcades that
//! keep the chain's messages and profiles indexes, through the mesh Explorer (`explorer.msg_rows`), so a new operator's
//! feed, follows and messages aren't empty meanwhile (HOMECHAIN.md option C, 2c; Martin 2026-11-17).
//!
//! Rows are kept here by their key and fetched in pages after the last one held (posts, acts and candidates are keyed by
//! height and position, so new ones come last); profiles are keyed by address and change in place, so they are read
//! again whole now and then. What comes back is one arcade's word, as other mesh Explorer answers are.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How far (blocks) this arcade's own messages index may be behind the chain's headers before lenders are read instead.
pub const BEHIND: u32 = 30;
/// Rows held per chain and table, at most (the oldest go first).
const MOST: usize = 200_000;
/// How long held rows stand before new ones are asked for.
const FRESH: Duration = Duration::from_secs(30);
/// Profiles are read again whole this often (they change in place).
const PROFILES_AGAIN: Duration = Duration::from_secs(600);
/// @tags are read again whole this often: keyed by name, a new claim can land anywhere in key order, and a released
/// tag goes away (crate::hometag asks the mesh about a tag it doesn't find meanwhile).
const TAGS_AGAIN: Duration = Duration::from_secs(120);
/// How long one read may spend fetching pages; the rest comes on the next read.
const BUDGET: Duration = Duration::from_secs(8);

#[derive(Default)]
struct Held {
    rows: BTreeMap<Vec<u8>, (Value, u64)>,
    at: Option<Instant>,
    whole_at: Option<Instant>,
    busy: bool,
}

fn st() -> &'static Mutex<HashMap<(u16, &'static str), Held>> {
    static S: OnceLock<Mutex<HashMap<(u16, &'static str), Held>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The index lenders of chain `p` must keep for `table`. ARCADE keeps its posts, messages and profiles with every
/// block, whatever its offer lists: any arcade that keeps it will do.
pub(crate) fn index_of(p: &ChainParams, table: &str) -> &'static str {
    if crate::arcborrow::is(p) {
        ""
    } else if table == "tag" {
        "ledger"
    } else if table == "profile" {
        "profiles"
    } else {
        "messages"
    }
}

/// Whether chain `p`'s posts, messages and profiles are read from lenders here: its copy is borrowed, or its own
/// messages index is more than [`BEHIND`] blocks short of its headers, and some arcade on the mesh keeps them.
pub fn lent(node: &Node, p: &ChainParams) -> bool {
    let c = p.chain_no;
    let borrowed = crate::borrow::is_borrowed(p.key) && !(node.extra.lock().unwrap().contains_key(p.key) && node.chain.indexers_for(c).contains(&"messages.v1"));
    let behind = || {
        let at = node.chain.indexed_to(c, "messages.v1").ok().flatten();
        let top = node.chain.best_header(c).ok().flatten().and_then(|h| node.chain.header(c, &h).ok().flatten()).map(|r| r.height).unwrap_or(0);
        node.chain.indexers_for(c).contains(&"messages.v1") && at.unwrap_or(0) + BEHIND < top
    };
    (borrowed || forced(p) || behind()) && !crate::shares::index_holders(node, p.net, index_of(p, "post")).is_empty()
}

/// Every row of `table` ("post", "act", "cand" or "profile") held for chain `p`, in key order, each with its block's
/// time as `_t` (the lender's word, as this arcade may not have the header yet). Fetches what's new first, within
/// [`BUDGET`], unless another read is fetching already (then what is held).
pub fn rows(node: &Node, p: &'static ChainParams, table: &'static str) -> Vec<Value> {
    refresh(node, p, table);
    let s = st().lock().unwrap();
    s.get(&(p.chain_no, table)).map(|h| h.rows.values().map(|(v, t)| with_time(v, *t)).collect()).unwrap_or_default()
}

/// Candidates after (height, position), oldest first, at most `limit`: as `arcade_ledger::messages::candidates`.
pub fn candidates(node: &Node, p: &'static ChainParams, after: Option<(u32, u32)>, limit: usize) -> Vec<(u32, u32, Value)> {
    refresh(node, p, "cand");
    let s = st().lock().unwrap();
    let Some(h) = s.get(&(p.chain_no, "cand")) else { return Vec::new() };
    let start = after.map(|(h, p)| [h.to_be_bytes(), p.to_be_bytes()].concat());
    let range = match &start {
        Some(k) => h.rows.range::<Vec<u8>, _>((std::ops::Bound::Excluded(k.clone()), std::ops::Bound::Unbounded)),
        None => h.rows.range::<Vec<u8>, _>(..),
    };
    range
        .filter(|(k, _)| k.len() == 8)
        .take(limit)
        .map(|(k, (v, t))| (u32::from_be_bytes(k[..4].try_into().unwrap()), u32::from_be_bytes(k[4..].try_into().unwrap()), with_time(v, *t)))
        .collect()
}

/// The profile published for `address` on chain `p`, as lenders keep it.
pub fn profile(node: &Node, p: &'static ChainParams, address: &str) -> Option<Value> {
    refresh(node, p, "profile");
    st().lock().unwrap().get(&(p.chain_no, "profile")).and_then(|h| h.rows.get(address.as_bytes()).map(|(v, _)| v.clone()))
}

/// Whether chain `p`'s @tags are read from lenders here: its copy is borrowed and its own ledger index isn't kept
/// (ARCADE borrowed; Pepecoin testnet not installed), and some arcade on the mesh keeps them.
pub fn tags_lent(node: &Node, p: &ChainParams) -> bool {
    let borrowed = if crate::arcborrow::is(p) {
        crate::arcsvc::borrowed(p)
    } else {
        crate::borrow::is_borrowed(p.key) && !node.chain.indexers_for(p.chain_no).contains(&"ledger.v1")
    };
    (borrowed || forced(p)) && !crate::shares::index_holders(node, p.net, index_of(p, "tag")).is_empty()
}

/// Who holds `tag` on chain `p` as lenders keep its tag table: (address, claim height). None: not among the rows held.
/// Rows are held per (chain, tag).
pub fn tag_owner(node: &Node, p: &'static ChainParams, tag: &str) -> Option<(String, u32)> {
    refresh(node, p, "tag");
    held_tag_owner(p.chain_no, tag)
}

fn held_tag_owner(c: u16, tag: &str) -> Option<(String, u32)> {
    st().lock().unwrap().get(&(c, "tag")).and_then(|h| {
        h.rows.get(tag.as_bytes()).and_then(|(v, _)| Some((v["a"].as_str()?.to_string(), v["h"].as_u64().unwrap_or(0) as u32)))
    })
}

/// The @tag held by the key hash `h` (20 bytes) on chain `p`, as lenders keep the tag table.
pub fn tag_of_key(node: &Node, p: &'static ChainParams, h: &[u8]) -> Option<String> {
    refresh(node, p, "tag");
    held_tag_of_key(p.chain_no, h)
}

fn held_tag_of_key(c: u16, h: &[u8]) -> Option<String> {
    let s = st().lock().unwrap();
    let held = s.get(&(c, "tag"))?;
    held.rows.iter().find_map(|(k, (v, _))| {
        let a = v["a"].as_str()?;
        let kh = arcade_ledger::address::decode_base58check(a).filter(|(_, x)| x.len() == 20).map(|(_, x)| x)?;
        (kh == h).then(|| String::from_utf8_lossy(k).into_owned())
    })
}

/// This arcade's own rows and lenders' together, one per txid (its own first, then lenders' it doesn't have).
pub fn merged(own: Vec<Value>, lent: Vec<Value>) -> Vec<Value> {
    let have: std::collections::HashSet<String> = own.iter().filter_map(|v| v["txid"].as_str().map(String::from)).collect();
    let mut out = own;
    out.extend(lent.into_iter().filter(|v| v["txid"].as_str().is_none_or(|t| !have.contains(t))));
    out
}

/// What is held per chain and table, for `ledger.lent`: {chain: {lent, tables: {table: {rows, fresh_s}}}}.
pub fn status(node: &Node) -> Value {
    let chains: Vec<(&'static ChainParams, bool)> = node.ledger_chains().into_iter().map(|p| (p, lent(node, p))).collect();
    let s = st().lock().unwrap();
    let mut out = serde_json::Map::new();
    for (p, is_lent) in chains {
        let tables: serde_json::Map<String, Value> = ["post", "act", "cand", "profile", "tag"]
            .iter()
            .filter_map(|t| s.get(&(p.chain_no, *t)).map(|h| (t.to_string(), json!({"rows": h.rows.len(), "fresh_s": h.at.map(|a| a.elapsed().as_secs())}))))
            .collect();
        let holders = crate::shares::index_holders(node, p.net, index_of(p, "post")).len();
        out.insert(p.key.to_string(), json!({"lent": is_lent, "holders": holders, "forced": forced(p), "tables": tables}));
    }
    Value::Object(out)
}

/// Tests only: ARCADE_LENT=<chain keys> reads those chains from lenders whatever their own copy.
fn forced(p: &ChainParams) -> bool {
    std::env::var("ARCADE_LENT").is_ok_and(|v| v.split(',').any(|k| k.trim() == p.key))
}

fn with_time(v: &Value, t: u64) -> Value {
    let mut v = v.clone();
    if t > 0 {
        v["_t"] = json!(t);
    }
    v
}

/// Ask lenders for what's new in `table` of `p`, unless held rows are fresh or another read is at it.
fn refresh(node: &Node, p: &'static ChainParams, table: &'static str) {
    let whole;
    let mut after;
    {
        let mut s = st().lock().unwrap();
        let h = s.entry((p.chain_no, table)).or_default();
        if h.busy || h.at.is_some_and(|a| a.elapsed() < FRESH) {
            return;
        }
        h.busy = true;
        whole = match table {
            "profile" => h.whole_at.is_none_or(|a| a.elapsed() > PROFILES_AGAIN),
            "tag" => h.whole_at.is_none_or(|a| a.elapsed() > TAGS_AGAIN),
            _ => false,
        };
        after = if whole { None } else { h.rows.keys().next_back().cloned() };
    }
    let t0 = Instant::now();
    let mut got: Vec<(Vec<u8>, Value, u64)> = Vec::new();
    let (mut finished, mut failed) = (false, false);
    while t0.elapsed() < BUDGET {
        let mut req = json!({"chain": p.key, "table": table, "limit": 1000});
        if let Some(a) = &after {
            req["after"] = json!(hex::encode(a));
        }
        let Ok((v, _)) = crate::shares::ask_explorer(node, p, index_of(p, table), "explorer.msg_rows", &req) else {
            // No lender answers (none updated yet, or none reachable): not asked again before FRESH is up, so a feed
            // isn't held up by the mesh on every read.
            failed = true;
            break;
        };
        let rows = v["rows"].as_array().cloned().unwrap_or_default();
        for r in &rows {
            let Some(k) = r["k"].as_str().and_then(|k| hex::decode(k).ok()) else { continue };
            got.push((k, r["v"].clone(), r["t"].as_u64().unwrap_or(0)));
        }
        after = got.last().map(|g| g.0.clone()).or(after);
        if v["more"] != true || rows.is_empty() {
            finished = true;
            break;
        }
    }
    let mut s = st().lock().unwrap();
    let h = s.entry((p.chain_no, table)).or_default();
    h.busy = false;
    if whole && finished {
        h.rows.clear();
        h.whole_at = Some(Instant::now());
    }
    for (k, v, t) in got {
        h.rows.insert(k, (v, t));
    }
    while h.rows.len() > MOST {
        h.rows.pop_first();
    }
    // Fresh only once everything new is in: a read cut short by the budget goes on at the next one.
    if finished || failed {
        h.at = Some(Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lent_tags_by_name_and_by_key_on_their_own_chain() {
        let (c, other) = (60_002, 60_003);
        let p = &arcade_chain::params::PEPECOIN_TEST;
        let alice = arcade_ledger::address::base58check(p.p2pkh_version, &[5; 20]);
        let bob = arcade_ledger::address::base58check(p.p2pkh_version, &[6; 20]);
        {
            let mut s = st().lock().unwrap();
            let h = s.entry((c, "tag")).or_default();
            h.rows.insert(b"alice".to_vec(), (json!({"a": alice, "t": "00", "h": 42, "p": 1}), 0));
            h.rows.insert(b"bob".to_vec(), (json!({"a": bob, "t": "01", "h": 43, "p": 0}), 0));
            h.at = Some(Instant::now());
        }
        assert_eq!(held_tag_owner(c, "alice"), Some((alice.clone(), 42)));
        assert_eq!(held_tag_owner(c, "carol"), None);
        // Another chain's namespace holds nothing of these.
        assert_eq!(held_tag_owner(other, "alice"), None);
        assert_eq!(held_tag_of_key(c, &[6; 20]).as_deref(), Some("bob"));
        assert_eq!(held_tag_of_key(c, &[7; 20]), None);
        assert_eq!(held_tag_of_key(other, &[6; 20]), None);
    }

    #[test]
    fn candidates_after_a_cursor_in_key_order() {
        let c = 60_001;
        let k = |h: u32, p: u32| [h.to_be_bytes(), p.to_be_bytes()].concat();
        {
            let mut s = st().lock().unwrap();
            let h = s.entry((c, "cand")).or_default();
            for (hh, pp) in [(5, 1), (5, 0), (7, 2), (6, 9)] {
                h.rows.insert(k(hh, pp), (json!({"h": hh, "p": pp}), 1000 + hh as u64));
            }
            h.at = Some(Instant::now());
        }
        let held = st().lock().unwrap();
        let h = &held[&(c, "cand")];
        let after: Vec<(u32, u32)> = h.rows.range::<Vec<u8>, _>((std::ops::Bound::Excluded(k(5, 1)), std::ops::Bound::Unbounded)).map(|(k, _)| (u32::from_be_bytes(k[..4].try_into().unwrap()), u32::from_be_bytes(k[4..].try_into().unwrap()))).collect();
        assert_eq!(after, [(6, 9), (7, 2)]);
        assert_eq!(with_time(&h.rows[&k(7, 2)].0, h.rows[&k(7, 2)].1)["_t"], 1007);
    }
}
