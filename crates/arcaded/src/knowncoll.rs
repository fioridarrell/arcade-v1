//! Known collections: Nintondo's public lists of Doginals, Pepinals and Bellinals (the original
//! arcade's known_collections.py), part of the collections index (Martin 2026-11-11).
//!
//! Asking Nintondo is the operator's own choice (`collections.known_set`, off by default): an
//! arcade with it off asks nobody outside. When it is on, for every chain with the collections
//! index that Nintondo lists (Dogecoin, Pepecoin, Bellscoin):
//! - the listing (each collection's line: name, supply, owners, ...) is read once a day, a page
//!   a second;
//! - a collection's pieces are read when somebody opens it (a want), 100 a request, and kept.
//!
//! Each kept collection carries the original's fingerprint, so arcades can compare lists:
//!   fp(chain, slug, ids) = sha256(chain "\n" slug "\n" + "\n".join(sorted(ids)))
//! with the original's chain names ("doge", "main", "bel").
//!
//! Tables, in the chain's own database (C = chain number):
//!   coll.v1.kc    C|slug           -> JSON listing line + what is held {count, fp, state, why, ...}
//!   coll.v1.kci   C|slug|pos|iid   -> JSON [id, name, number]     a held collection's pieces, by position
//!   coll.v1.kcof  C|iid            -> JSON [slug, position]       which collection a piece is in

use crate::node::Node;
use arcade_chain::params::ChainParams;
use arcade_store::{KeyBuf, TableDef};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const KC: &str = "coll.v1.kc";
pub const KCI: &str = "coll.v1.kci";
pub const KCOF: &str = "coll.v1.kcof";
pub const TABLES: &[&str] = &[KC, KCI, KCOF];

/// Nintondo's search hosts, by V1.0 chain key, with the original's name of the chain.
const SOURCES: &[(&str, &str, &str)] = &[
    ("doge", "doge", "https://doge-mainnet-search.nintondo.io"),
    ("pep", "main", "https://pepe-mainnet-search.nintondo.io"),
    ("bel", "bel", "https://bells-mainnet-search.nintondo.io"),
];
const MAX_ITEMS: u64 = 100_000;
const MAX_COLLECTIONS: usize = 5_000;
const LIST_PAGE: usize = 50;
const ITEM_PAGE: usize = 100;
const PACE: Duration = Duration::from_secs(1);
const REFRESH: u64 = 24 * 3600;
const SETTING: &[u8] = b"known_collections";

pub fn source(p: &ChainParams) -> Option<(&'static str, &'static str)> {
    SOURCES.iter().find(|s| s.0 == p.key).map(|s| (s.1, s.2))
}

/// The original's fingerprint of a collection's pieces.
pub fn fingerprint(chain: &str, slug: &str, ids: &[String]) -> String {
    let mut ids: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
    ids.sort_unstable();
    hex::encode(Sha256::digest(format!("{chain}\n{slug}\n{}", ids.join("\n")).as_bytes()))
}

fn slug_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 100 && s.as_bytes()[0].is_ascii_alphanumeric() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

/// "Name #12" -> 12.
pub fn position_of(name: &str) -> Option<u64> {
    let t = name.trim_end();
    let digits: String = t.chars().rev().take_while(|c| c.is_ascii_digit()).collect::<String>().chars().rev().collect();
    if digits.is_empty() || digits.len() > 9 {
        return None;
    }
    t[..t.len() - digits.len()].trim_end().ends_with('#').then(|| digits.parse().ok()).flatten()
}

// ---------------- the setting ----------------

pub fn enabled(node: &Node) -> bool {
    node.app.store.read(|r| r.get(arcade_app::META, SETTING)).ok().flatten().is_some_and(|v| v == b"1")
}

/// `collections.known_set {on}`: ask Nintondo for its lists (off by default).
pub fn set(node: &Node, on: bool) -> Result<Value, String> {
    node.app.store.write(|w| w.put(arcade_app::META, SETTING, if on { b"1" } else { b"0" })).map_err(|e| e.to_string())?;
    Ok(json!({"known": on}))
}

// ---------------- the store ----------------

fn k(c: u16) -> KeyBuf {
    KeyBuf::new().u16(c)
}

fn ensure(node: &Node, c: u16) -> Result<std::sync::Arc<arcade_store::Store>, String> {
    let s = node.chain.wstore(c).map_err(|e| e.to_string())?;
    for t in TABLES {
        s.add_table(TableDef::plain(t)).map_err(|e| e.to_string())?;
    }
    Ok(s)
}

pub fn line(node: &Node, c: u16, slug: &str) -> Option<Value> {
    node.chain.store(c).read(|r| r.get(KC, &k(c).var(slug.as_bytes()).done())).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok())
}

fn put_line(node: &Node, c: u16, slug: &str, v: &Value) -> Result<(), String> {
    ensure(node, c)?.write(|w| w.put(KC, &k(c).var(slug.as_bytes()).done(), v.to_string().as_bytes())).map_err(|e| e.to_string())
}

/// Every listed collection of chain `c`.
pub fn lines(node: &Node, c: u16) -> Vec<Value> {
    node.chain.store(c).read(|r| r.scan_prefix(KC, &k(c).done())).unwrap_or_default().into_iter().filter_map(|(_, v)| serde_json::from_slice(&v).ok()).collect()
}

/// A page of a held collection's pieces, by position.
pub fn items(node: &Node, c: u16, slug: &str, offset: u64, n: usize) -> Vec<Value> {
    let pre = k(c).var(slug.as_bytes()).done();
    let mut out = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    let mut skipped = 0;
    loop {
        let rows = node.chain.store(c).read(|r| r.scan_after(KCI, &pre, after.as_deref(), 5_000)).unwrap_or_default();
        if rows.is_empty() {
            return out;
        }
        after = rows.last().map(|x| x.0.clone());
        for (_, v) in rows {
            if skipped < offset {
                skipped += 1;
                continue;
            }
            if out.len() >= n {
                return out;
            }
            out.push(serde_json::from_slice(&v).unwrap_or(Value::Null));
        }
    }
}

/// The known collection a piece is in here: (slug, its position "#k" when its name has one).
pub fn of(node: &Node, c: u16, iid: &[u8; 36]) -> Option<(String, Option<u64>)> {
    let v: Value = serde_json::from_slice(&node.chain.store(c).read(|r| r.get(KCOF, &k(c).bytes(iid).done())).ok()??).ok()?;
    Some((v[0].as_str()?.to_string(), v[1].as_u64()))
}

/// Ask for a collection's pieces (it was opened and isn't held). Returns its line as it stands.
pub fn want(node: &Node, p: &ChainParams, slug: &str) -> Option<Value> {
    let c = p.chain_no;
    let mut v = line(node, c, slug)?;
    if v["state"] == "listed" {
        v["state"] = json!("wanted");
        v["wanted_at"] = json!(arcade_app::now());
        let _ = put_line(node, c, slug, &v);
    }
    Some(v)
}

/// Replace a collection's pieces with `found` [(id, name, number)] and record its fingerprint.
fn keep(node: &Node, p: &ChainParams, chain_name: &str, slug: &str, found: &[(String, String, Option<u64>)]) -> Result<(u64, String), String> {
    let c = p.chain_no;
    let s = ensure(node, c)?;
    let pre = k(c).var(slug.as_bytes()).done();
    loop {
        let old = s.read(|r| r.scan_prefix_limit(KCI, &pre, 20_000)).map_err(|e| e.to_string())?;
        if old.is_empty() {
            break;
        }
        s.write(|w| {
            for (key, v) in &old {
                let a: Value = serde_json::from_slice(v).unwrap_or(Value::Null);
                if let Some(x) = arcade_ledger::ord::parse_iid(a[0].as_str().unwrap_or("")) {
                    let mine = w.get(KCOF, &k(c).bytes(&x).done())?.and_then(|v| serde_json::from_slice::<Value>(&v).ok()).is_some_and(|v| v[0] == slug);
                    if mine {
                        w.del(KCOF, &k(c).bytes(&x).done())?;
                    }
                }
                w.del(KCI, key)?;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    }
    for chunk in found.chunks(5_000) {
        s.write(|w| {
            for (id, name, number) in chunk {
                let Some(x) = arcade_ledger::ord::parse_iid(id) else { continue };
                let pos = position_of(name).unwrap_or(u64::MAX);
                w.put(KCI, &k(c).var(slug.as_bytes()).u64(pos).bytes(&x).done(), json!([id, name, number]).to_string().as_bytes())?;
                w.put(KCOF, &k(c).bytes(&x).done(), json!([slug, position_of(name)]).to_string().as_bytes())?;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    }
    let ids: Vec<String> = found.iter().map(|f| f.0.clone()).collect();
    Ok((found.len() as u64, fingerprint(chain_name, slug, &ids)))
}

// ---------------- reading Nintondo ----------------

#[derive(Clone, Default)]
struct Progress {
    note: String,
    last_list: u64,
}

static PROGRESS: Mutex<BTreeMap<u16, Progress>> = Mutex::new(BTreeMap::new());

pub fn note(c: u16) -> String {
    PROGRESS.lock().unwrap().get(&c).map(|p| p.note.clone()).unwrap_or_default()
}

fn say(c: u16, s: String) {
    PROGRESS.lock().unwrap().entry(c).or_default().note = s;
}

fn ask(method: &str, url: &str, body: Option<Value>, last: &mut Option<Instant>) -> Result<Value, String> {
    if let Some(t) = *last {
        if let Some(rest) = PACE.checked_sub(t.elapsed()) {
            std::thread::sleep(rest);
        }
    }
    *last = Some(Instant::now());
    let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).user_agent("Arcade V1.0 known collections").build();
    let r = match body {
        Some(b) => agent.request(method, url).send_json(b),
        None => agent.request(method, url).call(),
    };
    r.map_err(|e| e.to_string())?.into_json().map_err(|e| e.to_string())
}

fn read_listing(node: &Node, p: &ChainParams, host: &str, last: &mut Option<Instant>) -> Result<usize, String> {
    let c = p.chain_no;
    let mut seen = 0;
    for page in 1..=(MAX_COLLECTIONS / LIST_PAGE) {
        let got = ask("GET", &format!("{host}/pub/collections?page_size={LIST_PAGE}&page={page}"), None, last)?;
        let rows = got["collections"].as_array().cloned().unwrap_or_default();
        for x in &rows {
            let slug = x["slug"].as_str().unwrap_or("");
            if !slug_ok(slug) {
                continue;
            }
            let mut v = line(node, c, slug).unwrap_or_else(|| json!({"slug": slug, "state": "listed", "count": 0}));
            // The original's signature of a line: supply|first number|last number|size (Python's
            // str() of each, "None" when missing).
            let py = |v: &Value| if v.is_null() { "None".to_string() } else { v.to_string() };
            let nums = x["numbers"].as_array().cloned().unwrap_or_default();
            let sig = format!("{}|{}|{}|{}", py(&x["supply"]), py(nums.first().unwrap_or(&Value::Null)), py(nums.get(1).unwrap_or(&Value::Null)), py(&x["size"]));
            if v["state"] == "held" && v["sig"].as_str() != Some(&sig) {
                v["state"] = json!("wanted"); // held, and its line changed: read again
            }
            v["name"] = json!(x["name"].as_str().unwrap_or(slug).chars().take(120).collect::<String>());
            v["supply"] = x["supply"].clone();
            v["owners"] = x["owners"].clone();
            v["height"] = x["height"].clone().as_u64().map(Value::from).unwrap_or(Value::Null);
            // Milliseconds when that big; an ISO date is kept as it came.
            v["created"] = match x["created"].as_u64() {
                Some(n) if n > 100_000_000_000 => json!(n / 1000),
                _ => x["created"].clone(),
            };
            v["first"] = nums.first().cloned().unwrap_or(Value::Null);
            v["last"] = nums.get(1).cloned().unwrap_or(Value::Null);
            v["size"] = x["size"].clone();
            v["listed_at"] = json!(arcade_app::now());
            v["listed_sig"] = json!(sig);
            put_line(node, c, slug, &v)?;
            seen += 1;
        }
        let pages = got["pages"].as_u64().unwrap_or(0) as usize;
        say(c, format!("reading Nintondo's list: page {page} of {pages}"));
        if rows.is_empty() || page >= pages {
            break;
        }
    }
    Ok(seen)
}

fn read_pieces(node: &Node, p: &ChainParams, chain_name: &str, host: &str, slug: &str, last: &mut Option<Instant>) -> Result<(), String> {
    let c = p.chain_no;
    let mut v = line(node, c, slug).ok_or("not listed")?;
    if v["supply"].as_u64().unwrap_or(0) > MAX_ITEMS {
        v["state"] = json!("skipped");
        v["why"] = json!(format!("{} pieces, more than {MAX_ITEMS} are kept", v["supply"]));
        return put_line(node, c, slug, &v);
    }
    v["state"] = json!("loading");
    put_line(node, c, slug, &v)?;
    let mut found: Vec<(String, String, Option<u64>)> = Vec::new();
    let mut page = 1;
    loop {
        let got = ask("POST", &format!("{host}/pub/search"), Some(json!({"collection": slug, "page": page, "page_size": ITEM_PAGE})), last)?;
        let rows = got["inscriptions"].as_array().cloned().unwrap_or_default();
        for x in &rows {
            let id = x["id"].as_str().unwrap_or("").to_lowercase();
            if arcade_ledger::ord::parse_iid(&id).is_none() || found.iter().any(|f| f.0 == id) {
                continue;
            }
            let name = x["collection"]["meta"]["name"].as_str().unwrap_or("").chars().take(120).collect();
            found.push((id, name, x["number"].as_u64()));
        }
        let pages = got["pages"].as_u64().unwrap_or(0);
        v["have"] = json!(found.len());
        say(c, format!("reading {slug}: {} pieces", found.len()));
        if found.len() as u64 > MAX_ITEMS {
            v["state"] = json!("skipped");
            v["why"] = json!(format!("more than {MAX_ITEMS} pieces"));
            return put_line(node, c, slug, &v);
        }
        if rows.is_empty() || page >= pages {
            break;
        }
        page += 1;
        if page % 10 == 0 {
            put_line(node, c, slug, &v)?;
        }
    }
    let (count, fp) = keep(node, p, chain_name, slug, &found)?;
    v["state"] = json!("held");
    v["count"] = json!(count);
    v["fp"] = json!(fp);
    v["sig"] = v["listed_sig"].clone();
    v["source"] = json!("nintondo");
    v["fetched_at"] = json!(arcade_app::now());
    v["cover"] = json!(found.iter().min_by_key(|f| position_of(&f.1).unwrap_or(u64::MAX)).map(|f| f.0.clone()));
    put_line(node, c, slug, &v)
}

/// In the collections thread: Nintondo's lists for the chains that have them, when the operator
/// switched it on. One step a call: a listing round when due, then each wanted collection.
pub fn step(node: &Node, p: &'static ChainParams) {
    let Some((chain_name, host)) = source(p) else { return };
    if !enabled(node) {
        return;
    }
    let c = p.chain_no;
    let mut last = None;
    let due = PROGRESS.lock().unwrap().get(&c).map(|x| x.last_list).unwrap_or(0) + REFRESH <= arcade_app::now();
    if due {
        match read_listing(node, p, host, &mut last) {
            Ok(n) => {
                PROGRESS.lock().unwrap().entry(c).or_default().last_list = arcade_app::now();
                say(c, format!("Nintondo lists {n} collections"));
            }
            Err(e) => say(c, format!("could not read Nintondo's list: {e}")),
        }
    }
    let mut wanted: Vec<Value> = lines(node, c).into_iter().filter(|v| v["state"] == "wanted" || v["state"] == "loading").collect();
    wanted.sort_by_key(|v| v["wanted_at"].as_u64().unwrap_or(0));
    for v in wanted {
        let slug = v["slug"].as_str().unwrap_or("").to_string();
        if let Err(e) = read_pieces(node, p, chain_name, host, &slug, &mut last) {
            say(c, format!("could not read {slug}: {e}"));
            break;
        }
    }
}

/// Every 30 s: each chain with the collections index that Nintondo lists.
pub fn run(node: std::sync::Arc<Node>) {
    loop {
        for p in crate::collsvc::chains(&node) {
            step(&node, p);
        }
        std::thread::sleep(Duration::from_secs(30));
    }
}

/// Every row of the known-collection tables on chain `c` goes (the index is removed).
pub fn wipe(node: &Node, c: u16) {
    let tables: Vec<String> = TABLES.iter().map(|t| t.to_string()).collect();
    while node.chain.clear_rows_batch(c, &tables, 50_000).map(|x| x > 0).unwrap_or(false) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_and_positions_are_the_originals() {
        // python3: hashlib.sha256(("doge\nbone\n" + "\n".join(sorted(ids))).encode()).hexdigest()
        let ids = vec!["bb".repeat(32) + "i0", "aa".repeat(32) + "i0"];
        let want = hex::encode(Sha256::digest(format!("doge\nbone\n{}i0\n{}i0", "aa".repeat(32), "bb".repeat(32)).as_bytes()));
        assert_eq!(fingerprint("doge", "bone", &ids), want);
        assert_eq!(position_of("Bone #12"), Some(12));
        assert_eq!(position_of("Bone # 7 "), Some(7));
        assert_eq!(position_of("Bone 12"), None);
        assert_eq!(position_of("#"), None);
        assert!(slug_ok("doginal-dogs") && slug_ok("a.b_c") && !slug_ok("-x") && !slug_ok("a b") && !slug_ok(""));
    }
}
