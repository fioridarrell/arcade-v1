//! Each coin's rich list for the Explorer (explorer.richlist): the addresses holding the most of a chain this arcade
//! keeps with its Addresses index, made in the background from the address index's unspent coins
//! (arcade_chaindb::ChainDb::rich_list), one chain at a time within the CPU share, and kept in the store so a restart
//! shows the last one at once. A chain only borrowed here, or kept without addresses, is asked of an arcade that keeps
//! them (crate::explorer's mesh questions, as for an address's balance).

use crate::node::Node;
use arcade_chain::params::ChainParams;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Addresses kept per chain (a page shows at most PAGE_MAX of them).
const KEEP: usize = 500;
const PAGE_MAX: usize = 100;
/// How often a list is made again: the ARCADE chain (small, busy) often, other coins hourly.
const REFRESH_ARCADE: Duration = Duration::from_secs(10 * 60);
const REFRESH: Duration = Duration::from_secs(60 * 60);
/// After start, before the first list: the arcade's own start-up work first.
const START_WAIT: Duration = Duration::from_secs(120);

struct St {
    /// The newest list per chain key (also in the store).
    lists: HashMap<&'static str, Value>,
    /// When each chain's list was last made here (this run).
    made: HashMap<&'static str, Instant>,
    /// Chains asked for that have none yet: made first.
    wanted: HashSet<&'static str>,
    /// The chain being made now.
    making: Option<&'static str>,
}

fn st() -> &'static Mutex<St> {
    static S: std::sync::LazyLock<Mutex<St>> = std::sync::LazyLock::new(|| Mutex::new(St { lists: HashMap::new(), made: HashMap::new(), wanted: HashSet::new(), making: None }));
    &S
}

fn store_key(p: &ChainParams) -> Vec<u8> {
    format!("richlist:{}", p.key).into_bytes()
}

/// Whether this arcade can make `p`'s rich list itself: it keeps the chain's blocks, its Addresses index is on, and no
/// address rows wait to be built (a list then would miss them).
pub fn makes(node: &Node, p: &ChainParams) -> bool {
    crate::chains::kept(node, p)
        && node.chain.addresses_on(p.chain_no).unwrap_or(false)
        && !node.chain.addresses_pending(p.chain_no).unwrap_or(true)
}

/// The address a row's coins are paid to: read from its first coin's script (a SegWit address isn't in the script id),
/// else from the id itself (P2PKH / P2SH). None for a script no address names.
fn address_of(node: &Node, p: &'static ChainParams, row: &arcade_chaindb::RichRow) -> Option<String> {
    let (b, pos, vout) = row.first;
    if let Some(tx) = node.chain.tx_at(p, b, pos).ok().flatten() {
        if let Some(o) = tx.outputs.get(vout as usize) {
            if let Some(a) = arcade_ledger::address::wallet_address_of(&o.script_pubkey, p) {
                return Some(a);
            }
        }
    }
    match row.sid[0] {
        0 => Some(arcade_ledger::address::base58check(p.p2pkh_version, &row.sid[1..])),
        1 => Some(arcade_ledger::address::base58check(p.p2sh_version, &row.sid[1..])),
        _ => None,
    }
}

/// Make `p`'s list now (the background thread; slow on a big chain: a walk through every unspent coin).
fn make(node: &Node, p: &'static ChainParams) -> Result<Value, String> {
    let height = node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height).unwrap_or(0);
    let began = Instant::now();
    let r = node.chain.rich_list(p.chain_no, KEEP, arcade_chaindb::cpushare::pace).map_err(|e| e.to_string())?;
    let rows: Vec<Value> = r
        .top
        .iter()
        .map(|x| {
            let a = address_of(node, p, x);
            json!({"address": a, "balance": x.balance, "coins": x.coins, "kind": if a.is_none() { Some("script") } else { None }})
        })
        .collect();
    Ok(json!({"chain": p.key, "height": height, "at": arcade_app::now(), "holders": r.holders, "coins": r.coins, "total": r.total,
              "rows": rows, "secs": began.elapsed().as_secs()}))
}

fn refresh_of(p: &ChainParams) -> Duration {
    if crate::arcsvc::defined().is_some_and(|a| a.key == p.key) { REFRESH_ARCADE } else { REFRESH }
}

/// The next chain to make: one asked for first, else the one whose list is oldest past its refresh.
fn next(node: &Node) -> Option<&'static ChainParams> {
    let s = st().lock().unwrap();
    let mut due: Vec<(Option<Instant>, &'static ChainParams)> = crate::chains::all()
        .filter(|p| makes(node, p))
        .filter_map(|p| {
            let made = s.made.get(p.key).copied();
            (made.is_none_or(|t| t.elapsed() >= refresh_of(p))).then_some((made, p))
        })
        .collect();
    if let Some(i) = due.iter().position(|(_, p)| s.wanted.contains(p.key)) {
        return Some(due.swap_remove(i).1);
    }
    due.sort_by_key(|(m, _)| *m);
    due.first().map(|(_, p)| *p)
}

pub fn start(node: Arc<Node>) {
    std::thread::Builder::new()
        .name("richlist".into())
        .spawn(move || {
            std::thread::sleep(START_WAIT);
            loop {
                if let Some(p) = next(&node) {
                    st().lock().unwrap().making = Some(p.key);
                    match make(&node, p) {
                        Ok(v) => {
                            let _ = node.app.store.write(|w| w.put(arcade_app::META, &store_key(p), v.to_string().as_bytes()));
                            let mut s = st().lock().unwrap();
                            s.lists.insert(p.key, v);
                            s.wanted.remove(p.key);
                        }
                        Err(e) => eprintln!("richlist: {}: {e}", p.key),
                    }
                    let mut s = st().lock().unwrap();
                    s.made.insert(p.key, Instant::now());
                    s.making = None;
                }
                std::thread::sleep(Duration::from_secs(30));
            }
        })
        .ok();
}

/// The kept list of `p` (this run's, else the store's).
fn kept_list(node: &Node, p: &'static ChainParams) -> Option<Value> {
    if let Some(v) = st().lock().unwrap().lists.get(p.key) {
        return Some(v.clone());
    }
    let v: Value = node.app.store.read(|r| r.get(arcade_app::META, &store_key(p))).ok().flatten().and_then(|b| serde_json::from_slice(&b).ok())?;
    st().lock().unwrap().lists.insert(p.key, v.clone());
    Some(v)
}

/// `explorer.richlist {chain?, offset?, limit?}`: a page of `p`'s rich list, biggest first: rank, address (and @tag),
/// balance, coins and share of all the coins held; with the height and time it was made, how many addresses hold any,
/// and `building` while the first list (or a newer one) is being made.
pub fn read(node: &Node, p: &'static ChainParams, req: &Value) -> Result<Value, String> {
    let num = |k: &str| req[k].as_u64().or_else(|| req[k].as_str().and_then(|x| x.trim().parse().ok()));
    let offset = num("offset").unwrap_or(0) as usize;
    let limit = (num("limit").unwrap_or(50) as usize).clamp(1, PAGE_MAX);
    let list = kept_list(node, p);
    let building = {
        let mut s = st().lock().unwrap();
        if list.is_none() {
            s.wanted.insert(p.key);
        }
        s.making == Some(p.key) || list.is_none()
    };
    let unit = crate::explorer::unit(p);
    let Some(v) = list else {
        return Ok(json!({"chain": p.key, "unit": unit, "building": true, "rows": [], "offset": offset, "limit": limit, "kept": KEEP,
                         "note": "this arcade is adding up its addresses for the first rich list: it shows here in a few minutes"}));
    };
    let total = v["total"].as_u64().unwrap_or(0).max(1) as f64;
    let rows: Vec<Value> = v["rows"]
        .as_array()
        .map(|a| a.iter().enumerate().skip(offset).take(limit).map(|(i, r)| {
            let mut r = r.clone();
            r["rank"] = json!(i + 1);
            r["share"] = json!(r["balance"].as_u64().unwrap_or(0) as f64 / total);
            if let Some(a) = r["address"].as_str() {
                r["tag"] = json!(crate::hometag::of_address(node, a));
            }
            r
        }).collect())
        .unwrap_or_default();
    let listed = v["rows"].as_array().map_or(0, |a| a.len());
    Ok(json!({"chain": p.key, "unit": unit, "height": v["height"], "at": v["at"], "holders": v["holders"], "coins": v["coins"],
              "total": v["total"], "rows": rows, "offset": offset, "limit": limit, "listed": listed, "kept": KEEP,
              "next": (offset + limit < listed).then_some(offset + limit), "building": building,
              "refresh_secs": refresh_of(p).as_secs()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_ids_name_their_address_when_no_coin_can_be_read() {
        let (_g, t) = crate::http::tests::node();
        let p = t.node.params;
        let h = [7u8; 20];
        let mut sid = [0u8; 21];
        sid[1..].copy_from_slice(&h);
        // A coin position that isn't there: the id names it (P2PKH, P2SH), a script id doesn't.
        let row = |sid| arcade_chaindb::RichRow { sid, balance: 1, coins: 1, first: (999_999, 0, 0) };
        assert_eq!(address_of(&t.node, p, &row(sid)), Some(arcade_ledger::address::base58check(p.p2pkh_version, &h)));
        sid[0] = 1;
        assert_eq!(address_of(&t.node, p, &row(sid)), Some(arcade_ledger::address::base58check(p.p2sh_version, &h)));
        sid[0] = 2;
        assert_eq!(address_of(&t.node, p, &row(sid)), None);
    }

    #[test]
    fn a_page_of_the_kept_list_ranks_and_shares() {
        let (_g, t) = crate::http::tests::node();
        let p = t.node.params;
        let v = json!({"chain": p.key, "height": 9, "at": 1, "holders": 3, "coins": 4, "total": 1000,
                       "rows": [{"address": "a1", "balance": 600, "coins": 2}, {"address": "a2", "balance": 300, "coins": 1}, {"address": null, "balance": 100, "coins": 1, "kind": "script"}]});
        st().lock().unwrap().lists.insert(p.key, v);
        let r = read(&t.node, p, &json!({"offset": 1, "limit": 1})).unwrap();
        assert_eq!(r["rows"][0]["rank"], 2);
        assert_eq!(r["rows"][0]["address"], "a2");
        assert!((r["rows"][0]["share"].as_f64().unwrap() - 0.3).abs() < 1e-9);
        assert_eq!(r["next"], 2);
        assert_eq!(r["holders"], 3);
        let all = read(&t.node, p, &json!({"limit": 1000})).unwrap();
        assert_eq!(all["rows"].as_array().unwrap().len(), 3);
        assert_eq!(all["limit"], PAGE_MAX);
        assert_eq!(all["next"], Value::Null);
        st().lock().unwrap().lists.remove(p.key);
    }
}
