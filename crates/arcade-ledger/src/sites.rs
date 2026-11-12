//! Sites on names (the existing arcades' mesh_web.py and the `mesh` / `mesh_directory` lookups of
//! doginal_index.py). A name is an ordinary p:dns name (names.rs); its site is a JSON inscription
//! of its own:
//!
//!   {"p": "mesh", "name": "yours.mesh", "pages": {"/": "# Hello\n...", "/about": "..."}}
//!
//! A record counts when the address holding it now also holds the name's winning claim; of those
//! the newest wins. A ".mesh" site is at most SITE_MAX bytes and its pages are MeshText only (it
//! also loads over radio). Any other name's record is at most RECORD_MAX bytes and a page is
//! MeshText, {"html": "..."} or {"id": "<inscription id>"}.
//!
//! Nothing is stored for this: the records are found by the "proto" every ord row already keeps
//! (`records`), and the caller hands the newest of them to `site` and `directory`.

use crate::names::{self, py_space, py_strip};
use crate::ord;
use arcade_chain::params::ChainParams;
use arcade_chaindb::ChainDb;
use arcade_store::KeyBuf;
use serde::de::{Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

pub const ENDING: &str = ".mesh";
/// A .mesh site's whole inscription.
pub const SITE_MAX: usize = 3000;
/// Any other name's record: as big as a JSON "p" is read from (ord::protocol_of).
pub const RECORD_MAX: usize = 65_536;
pub const PATHS_MAX: usize = 24;
/// Records looked at, newest first, when a name is resolved or the directory made.
pub const SCAN: usize = 400;
pub const PROTO: &str = "mesh";

/// One page of a record.
#[derive(Debug, Clone, PartialEq)]
pub enum Page {
    Text(String),
    Html(String),
    Id(String),
}

impl Page {
    pub fn kind(&self) -> &'static str {
        match self {
            Page::Text(_) => "text",
            Page::Html(_) => "html",
            Page::Id(_) => "id",
        }
    }
    /// As the record carries it: MeshText as a string, the others as {"html"} / {"id"}.
    pub fn to_json(&self) -> Value {
        match self {
            Page::Text(t) => json!(t),
            Page::Html(h) => json!({"html": h}),
            Page::Id(i) => json!({"id": i}),
        }
    }
    pub fn from_json(v: &Value) -> Option<Page> {
        match v {
            Value::String(s) => Some(Page::Text(s.clone())),
            Value::Object(o) => {
                if let Some(Value::String(h)) = o.get("html") {
                    return Some(Page::Html(h.clone()));
                }
                let id = py_strip(o.get("id").and_then(|i| i.as_str()).unwrap_or("")).to_lowercase();
                is_inscription_id(&id).then_some(Page::Id(id))
            }
            _ => None,
        }
    }
}

/// "<64 hex>i<1 to 6 digits>", lowercase.
pub fn is_inscription_id(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() >= 66 && b.len() <= 71 && b[..64].iter().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) && b[64] == b'i' && b[65..].iter().all(u8::is_ascii_digit)
}

/// "/" or up to four short words of a-z 0-9 . _ - between slashes.
pub fn is_path(s: &str) -> bool {
    let Some(rest) = s.strip_prefix('/') else { return false };
    if rest.is_empty() {
        return true;
    }
    let words: Vec<&str> = rest.split('/').collect();
    words.len() <= 4 && words.iter().all(|w| (1..=32).contains(&w.len()) && w.bytes().all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-')))
}

pub fn is_mesh(name: &str) -> bool {
    name.ends_with(ENDING)
}

/// Any p:dns name a site may be made for: the spec reading's, else the JSON reading's; never a
/// block map.
pub fn site_name(raw: &str) -> Option<String> {
    let raw = py_strip(raw);
    let made = names::normalise("spec", "json", raw).or_else(|| names::normalise("json", "json", raw))?;
    let ending = made.rsplit('.').next().unwrap_or("");
    (!made.contains('/') && !names::is_block_map(ending)).then_some(made)
}

/// A .mesh name as the spec reading makes it.
pub fn name_of(raw: &str) -> Option<String> {
    names::normalise("spec", "json", py_strip(raw)).filter(|n| is_mesh(n))
}

/// "yours.mesh/about" -> (Some("yours.mesh"), "/about"); a bare name is its "/". `any_name`: every
/// p:dns name, not only .mesh.
pub fn split_address(address: &str, any_name: bool) -> (Option<String>, String) {
    let mut s = py_strip(address);
    if s.len() >= 7 && s.is_char_boundary(7) && s[..7].eq_ignore_ascii_case("mesh://") {
        s = &s[7..];
    }
    let (name, path) = match s.split_once('/') {
        Some((n, p)) => (n, format!("/{}", p.trim_matches('/'))),
        None => (s, "/".to_string()),
    };
    (if any_name { site_name(name) } else { name_of(name) }, path.to_lowercase())
}

// ---------------- reading a record ----------------

/// A JSON object's members in the order written (a later duplicate replaces the value, as
/// Python's dict does), or Other for any other JSON value.
enum Members {
    Map(Vec<(String, Value)>),
    Other,
}

fn put<T>(list: &mut Vec<(String, T)>, key: String, value: T) {
    match list.iter_mut().find(|(k, _)| *k == key) {
        Some(slot) => slot.1 = value,
        None => list.push((key, value)),
    }
}

impl<'de> serde::Deserialize<'de> for Members {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Members, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Members;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Members, A::Error> {
                let mut out = Vec::new();
                while let Some(key) = m.next_key::<String>()? {
                    let value = m.next_value::<Value>()?;
                    put(&mut out, key, value);
                }
                Ok(Members::Map(out))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut s: A) -> Result<Members, A::Error> {
                while s.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Members::Other)
            }
            fn visit_bool<E>(self, _: bool) -> Result<Members, E> {
                Ok(Members::Other)
            }
            fn visit_i64<E>(self, _: i64) -> Result<Members, E> {
                Ok(Members::Other)
            }
            fn visit_u64<E>(self, _: u64) -> Result<Members, E> {
                Ok(Members::Other)
            }
            fn visit_f64<E>(self, _: f64) -> Result<Members, E> {
                Ok(Members::Other)
            }
            fn visit_str<E>(self, _: &str) -> Result<Members, E> {
                Ok(Members::Other)
            }
            fn visit_unit<E>(self) -> Result<Members, E> {
                Ok(Members::Other)
            }
        }
        d.deserialize_any(V)
    }
}

struct Top {
    p: Option<Value>,
    name: Option<Value>,
    pages: Option<Members>,
}

impl<'de> serde::Deserialize<'de> for Top {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Top, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Top;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> Result<Top, A::Error> {
                let mut top = Top { p: None, name: None, pages: None };
                while let Some(key) = m.next_key::<String>()? {
                    match key.as_str() {
                        "p" => top.p = Some(m.next_value()?),
                        "name" => top.name = Some(m.next_value()?),
                        "pages" => top.pages = Some(m.next_value()?),
                        _ => {
                            m.next_value::<Value>()?;
                        }
                    }
                }
                Ok(top)
            }
        }
        d.deserialize_map(V)
    }
}

/// A site record: its name and its pages in the order written.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub name: String,
    pub pages: Vec<(String, Page)>,
}

impl Record {
    pub fn page(&self, path: &str) -> Option<&Page> {
        self.pages.iter().find(|(p, _)| p == path).map(|(_, v)| v)
    }
    pub fn pages_json(&self) -> Value {
        Value::Object(self.pages.iter().map(|(p, v)| (p.clone(), v.to_json())).collect::<Map<String, Value>>())
    }
}

/// A record inscription's name and pages, or None when the body is not one that counts.
pub fn parse(body: &[u8]) -> Option<Record> {
    if body.is_empty() || body.len() > RECORD_MAX {
        return None;
    }
    let top: Top = serde_json::from_slice(body).ok()?;
    if top.p.as_ref().and_then(|p| p.as_str()) != Some(PROTO) {
        return None;
    }
    let Some(Members::Map(members)) = top.pages else { return None };
    // (A number is read as its text, as the existing arcades do: 1.5 is the name "1.5".)
    let name = match &top.name {
        Some(Value::String(n)) => site_name(n),
        Some(Value::Number(n)) => site_name(&n.to_string()),
        _ => None,
    }?;
    if is_mesh(&name) && body.len() > SITE_MAX {
        return None;
    }
    let mut pages: Vec<(String, Page)> = Vec::new();
    for (path, value) in members.into_iter().take(PATHS_MAX) {
        let path = path.to_lowercase();
        let Some(page) = Page::from_json(&value) else { continue };
        if !is_path(&path) || (is_mesh(&name) && page.kind() != "text") {
            continue;
        }
        put(&mut pages, path, page);
    }
    (!pages.is_empty()).then_some(Record { name, pages })
}

/// Python's str.splitlines().
pub fn py_lines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, ch)) = it.next() {
        if matches!(ch, '\n' | '\r' | '\x0b' | '\x0c' | '\x1c' | '\x1d' | '\x1e' | '\u{85}' | '\u{2028}' | '\u{2029}') {
            out.push(&s[start..i]);
            let mut next = i + ch.len_utf8();
            if ch == '\r' && it.peek().is_some_and(|(_, c)| *c == '\n') {
                it.next();
                next += 1;
            }
            start = next;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

fn first_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

/// A page's first heading, else its first line; an HTML page's <title>; an inscription's: none.
pub fn title_of(page: Option<&Page>) -> String {
    match page {
        Some(Page::Html(h)) => {
            static T: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
            let re = T.get_or_init(|| regex::Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
            match re.captures(h) {
                Some(c) => {
                    let words: Vec<&str> = c[1].split(py_space).filter(|w| !w.is_empty()).collect();
                    first_chars(&words.join(" "), 80)
                }
                None => String::new(),
            }
        }
        Some(Page::Text(t)) => {
            let mut first = "";
            for line in py_lines(t) {
                let s = py_strip(line);
                if s.starts_with('#') {
                    return first_chars(py_strip(s.trim_start_matches('#')), 80);
                }
                if !s.is_empty() && first.is_empty() && !s.starts_with("=>") && !s.starts_with("```") && !s.starts_with("![") {
                    first = s;
                }
            }
            first_chars(first, 80)
        }
        _ => String::new(),
    }
}

/// The inscription id of each picture line ("![alt](id)") outside preformatted blocks, in order.
pub fn pictures_of(text: &str) -> Vec<String> {
    static P: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = P.get_or_init(|| regex::Regex::new(r"^!\[[^\]]*\]\(\s*([0-9a-fA-F]{64}[iI][0-9]{1,6})\s*\)\s*$").unwrap());
    let (mut out, mut pre) = (Vec::new(), false);
    for line in py_lines(text) {
        if line.starts_with("```") {
            pre = !pre;
            continue;
        }
        if !pre {
            if let Some(c) = re.captures(py_strip(line)) {
                out.push(c[1].to_lowercase());
            }
        }
    }
    out
}

// ---------------- reading the chain ----------------

/// (number, id) of the inscriptions numbered lo..hi whose JSON "p" is "mesh", oldest first.
pub fn records(db: &ChainDb, c: u16, lo: u64, hi: u64) -> Vec<(u64, String)> {
    if hi <= lo {
        return Vec::new();
    }
    let pre = KeyBuf::new().u16(c).done();
    let mut out = Vec::new();
    db.store(c)
        .read(|r| {
            let after = KeyBuf::new().u16(c).u64(lo.saturating_sub(1)).done();
            for (key, iid) in r.scan_after(ord::NUM, &pre, (lo > 0).then_some(&after[..]), (hi - lo) as usize)? {
                let n = u64::from_be_bytes(key[2..10].try_into().unwrap());
                if n >= hi {
                    break;
                }
                if let Some(v) = r.get(ord::INSC, &KeyBuf::new().u16(c).bytes(&iid).done())? {
                    // Cheap test first: most rows are not JSON with this "p".
                    if v.windows(6).any(|w| w == b"\"mesh\"") && serde_json::from_slice::<Value>(&v).is_ok_and(|v| v["proto"] == PROTO) {
                        out.push((n, ord::iid_str(&iid)));
                    }
                }
            }
            Ok(())
        })
        .ok();
    out
}

/// (the name's winning claim, the address holding that claim now): the spec reading for a .mesh
/// name; for any other, the first reading that has a claim of it.
pub fn holder(db: &ChainDb, p: &ChainParams, name: &str) -> (Option<String>, Option<String>) {
    let wins = names::lookup(db, p.chain_no, name);
    let rules: &[&str] = if is_mesh(name) { &names::RULES[..1] } else { &names::RULES[..] };
    match rules.iter().find_map(|r| wins.iter().find(|w| w.0 == *r)) {
        Some((_, _, claim)) => (Some(claim.clone()), ord::inscription(db, p, claim).and_then(|v| v["owner"].as_str().map(String::from))),
        None => (None, None),
    }
}

/// One record, when it is a record of `name` (any name: None) sitting at `holder`'s address now:
/// {id, number, height, name, pages, size, sha256}.
pub fn record(db: &ChainDb, p: &ChainParams, id: &str, name: Option<&str>, holder: Option<&str>) -> Option<Value> {
    let holder = holder?;
    let (_, body) = ord::content(db, p, id).ok()?;
    let rec = parse(&body)?;
    if name.is_some_and(|n| n != rec.name) {
        return None;
    }
    let meta = ord::inscription(db, p, id)?;
    if meta["owner"].as_str() != Some(holder) {
        return None;
    }
    Some(json!({"id": id, "number": meta["n"], "height": meta["h"], "name": rec.name, "pages": rec.pages_json(),
                "size": body.len(), "sha256": hex::encode(Sha256::digest(&body))}))
}

/// A name's site: {name, claim, holder, page}. `page` is the newest record its holder holds (null
/// when there is none); `holder` is null when nobody holds the name. `newest`: the chain's
/// records, newest first (`records`, at most SCAN are read).
pub fn site(db: &ChainDb, p: &ChainParams, name: &str, newest: &[(u64, String)]) -> Value {
    let made = site_name(name);
    let mut out = json!({"name": made, "claim": null, "holder": null, "page": null});
    let Some(made) = made else { return out };
    let (claim, who) = holder(db, p, &made);
    out["claim"] = json!(claim);
    out["holder"] = json!(who);
    if let Some(who) = who {
        if let Some(page) = newest.iter().take(SCAN).find_map(|(_, id)| record(db, p, id, Some(&made), Some(&who))) {
            out["page"] = page;
        }
    }
    out
}

/// Every name with a record that counts, newest record first: [{name, page, number, height,
/// title}]. `mesh_only`: .mesh names only.
pub fn directory(db: &ChainDb, p: &ChainParams, newest: &[(u64, String)], mesh_only: bool) -> Vec<Value> {
    let mut holders: std::collections::HashMap<String, Option<String>> = Default::default();
    let mut done: std::collections::HashSet<String> = Default::default();
    let mut out = Vec::new();
    for (_, id) in newest.iter().take(SCAN) {
        let Some(rec) = ord::content(db, p, id).ok().and_then(|(_, body)| parse(&body)) else { continue };
        if done.contains(&rec.name) || (mesh_only && !is_mesh(&rec.name)) {
            continue;
        }
        let who = holders.entry(rec.name.clone()).or_insert_with(|| holder(db, p, &rec.name).1).clone();
        if let Some(found) = record(db, p, id, Some(&rec.name), who.as_deref()) {
            done.insert(rec.name.clone());
            out.push(json!({"name": found["name"], "page": found["id"], "number": found["number"], "height": found["height"],
                            "title": title_of(rec.page("/"))}));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_addresses() {
        assert_eq!(site_name(" Yours.Mesh ").as_deref(), Some("yours.mesh"));
        assert_eq!(site_name("100.pepemap"), None);
        assert_eq!(site_name("nodot"), None);
        assert_eq!(name_of("yours.pepe"), None);
        assert_eq!(split_address("mesh://Yours.mesh/About/", false), (Some("yours.mesh".into()), "/about".into()));
        assert_eq!(split_address("yours.pepe", false), (None, "/".into()));
        assert_eq!(split_address("yours.pepe/a/b", true), (Some("yours.pepe".into()), "/a/b".into()));
        assert!(is_path("/") && is_path("/a/b.c/d_e/f-g") && !is_path("/a/b/c/d/e") && !is_path("/A") && !is_path("a") && !is_path("//"));
    }

    #[test]
    fn records_read() {
        let r = parse(br##"{"p":"mesh","name":"yours.mesh","pages":{"/":"# Hello\nhi","/About":"x","/pic":{"id":"00"},"bad path":"y"}}"##).unwrap();
        assert_eq!(r.name, "yours.mesh");
        assert_eq!(r.pages, vec![("/".to_string(), Page::Text("# Hello\nhi".into())), ("/about".to_string(), Page::Text("x".into()))]);
        // A .mesh site holds MeshText only and is small; another name's may hold HTML or an id.
        assert!(parse(br#"{"p":"mesh","name":"yours.mesh","pages":{"/":{"html":"<p>"}}}"#).is_none());
        let id = format!("{}i0", "ab".repeat(32));
        let body = format!(r#"{{"p":"mesh","name":"yours.pepe","pages":{{"/":{{"html":"<TITLE>A  B</title>"}},"/g":{{"id":" {} "}}}}}}"#, id.to_uppercase().replace('I', "i"));
        let r = parse(body.as_bytes()).unwrap();
        assert_eq!(r.page("/g"), Some(&Page::Id(id)));
        assert_eq!(title_of(r.page("/")), "A B");
        assert!(parse(br#"{"p":"dns","name":"yours.pepe","pages":{"/":"x"}}"#).is_none());
        assert!(parse(br#"{"p":"mesh","name":"yours.pepe","pages":["x"]}"#).is_none());
        let big = |n: usize| format!(r#"{{"p":"mesh","name":"yours.pepe","pages":{{"/":"{}"}}}}"#, "a".repeat(n - r#"{"p":"mesh","name":"yours.pepe","pages":{"/":""}}"#.len()));
        assert!(parse(big(RECORD_MAX).as_bytes()).is_some() && parse(big(RECORD_MAX + 1).as_bytes()).is_none());
        assert!(parse(format!(r#"{{"p":"mesh","name":"yours.mesh","pages":{{"/":"{}"}}}}"#, "a".repeat(SITE_MAX)).as_bytes()).is_none());
        // The first 24 paths written count, in that order; a later duplicate replaces the value.
        let many: Vec<String> = (0..30).rev().map(|i| format!(r#""/p{i}":"{i}""#)).collect();
        let r = parse(format!(r#"{{"p":"mesh","name":"yours.pepe","pages":{{{},"/p29":"again"}}}}"#, many.join(",")).as_bytes()).unwrap();
        assert_eq!(r.pages.len(), 24);
        assert_eq!(r.pages[0], ("/p29".to_string(), Page::Text("again".into())));
        assert!(r.page("/p5").is_none());
    }

    #[test]
    fn titles_and_pictures() {
        let t = |s: &str| title_of(Some(&Page::Text(s.to_string())));
        assert_eq!(t("\n=> a.mesh A\nfirst line\n## Heading  \nmore"), "Heading");
        assert_eq!(t("```\nart\n```\nplain"), "art");
        assert_eq!(t("![x](y)\nplain\r\nnext"), "plain");
        assert_eq!(t(""), "");
        assert_eq!(title_of(Some(&Page::Id("x".into()))), "");
        let id = format!("{}i12", "0f".repeat(32));
        let text = format!("![A cat]( {} )\n```\n![no]({id})\n```\n  ![]({id})  \n![bad](xyz)", id.to_uppercase());
        assert_eq!(pictures_of(&text), vec![id.clone(), id]);
    }
}
