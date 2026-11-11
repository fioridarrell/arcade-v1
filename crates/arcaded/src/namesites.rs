//! Sites on names for app pages (docs/APPCOMPAT.md gap 8): the existing arcades' page routes
//!   GET /r/mesh                      the .mesh names with a site
//!   GET /r/mesh/<address>            a .mesh page ("yours.mesh/about")
//!   GET /r/sites?page=&per=          every name with a site, every chain, newest first
//!   GET /r/site/<address>?chain=     any name's page
//!   GET /site/<chain>/<address>      that page on its own (HTML sandboxed, MeshText drawn)
//! and /content/<inscription id> for the pictures and inscription pages those name.
//!
//! The rules are arcade_ledger::sites. A chain's answer comes from this arcade when it keeps
//! the chain's names index, else from a V1.0 arcade on the mesh that does (the Explorer's
//! questions "explorer.site" and "explorer.sites"; that arcade's word, not checked here).
//!
//! Nothing new is indexed. The site records of a chain (inscriptions whose JSON "p" is "mesh")
//! are found by reading the ord rows once, in the background, newest first; the list is kept in
//! HOME/cache/sites-<chain>.json. The newest TAIL inscriptions are always read live, so a reorg
//! that renumbers them never leaves a stale entry.

use crate::node::Node;
use arcade_chain::params::{by_key, by_net, ChainParams, ALL_CHAINS};
use arcade_ledger::{ord, sites};
use base64::Engine as _;
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Instant;

/// The newest inscriptions, read live on every ask.
const TAIL: u64 = 4096;
/// Inscriptions read per background step.
const STEP: u64 = 16_384;
/// Not listed yet and above the list: read live up to this many on an ask.
const LIVE_MAX: u64 = 4 * STEP;
/// Seconds a directory is kept before it is made again.
const KEEP: u64 = 60;
/// The chain .mesh names are read on (its mesh name, as the existing arcades say it).
const MESH_NET: &str = "main";
/// Inscription bodies kept for /content (they never change).
const CONTENT_KEEP: usize = 32 << 20;

// ---------------- the record list of a chain ----------------

/// Records among the inscriptions numbered lo..hi, oldest first.
#[derive(Default, Clone)]
struct Scan {
    lo: u64,
    hi: u64,
    recs: Vec<(u64, String)>,
}

static SCANS: Mutex<Option<HashMap<u16, Scan>>> = Mutex::new(None);
static RUNNING: Mutex<Option<HashSet<u16>>> = Mutex::new(None);
static NODE: std::sync::OnceLock<std::sync::Weak<Node>> = std::sync::OnceLock::new();

pub fn init(node: &std::sync::Arc<Node>) {
    let _ = NODE.set(std::sync::Arc::downgrade(node));
}

fn file(node: &Node, p: &ChainParams) -> std::path::PathBuf {
    node.home.join("cache").join(format!("sites-{}.json", p.key))
}

fn load(node: &Node, p: &ChainParams, settled: u64) -> Scan {
    let v: Value = std::fs::read(file(node, p)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    match (v["lo"].as_u64(), v["hi"].as_u64(), v["recs"].as_array()) {
        (Some(lo), Some(hi), Some(recs)) if lo <= hi => Scan {
            lo,
            hi,
            recs: recs.iter().filter_map(|r| Some((r[0].as_u64()?, r[1].as_str()?.to_string()))).collect(),
        },
        // Nothing listed yet: start at the newest settled inscription and work down.
        _ => Scan { lo: settled, hi: settled, recs: Vec::new() },
    }
}

fn save(node: &Node, p: &ChainParams, s: &Scan) {
    let f = file(node, p);
    let _ = std::fs::create_dir_all(f.parent().unwrap());
    let tmp = f.with_extension("tmp");
    if std::fs::write(&tmp, json!({"lo": s.lo, "hi": s.hi, "recs": s.recs}).to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &f);
    }
}

fn with_scan<T>(node: &Node, p: &ChainParams, settled: u64, f: impl FnOnce(&mut Scan) -> T) -> T {
    let mut g = SCANS.lock().unwrap();
    let s = g.get_or_insert_with(HashMap::new).entry(p.chain_no).or_insert_with(|| load(node, p, settled));
    if s.hi > settled {
        // The chain shrank (reorg): what was above is read live again.
        s.recs.retain(|r| r.0 < settled);
        s.hi = settled;
        s.lo = s.lo.min(settled);
    }
    f(s)
}

/// List what is not listed yet, in the background (one thread per chain at a time): first up
/// to the newest settled inscription, then down to the first.
pub fn ensure(node: &Node, p: &'static ChainParams) {
    let settled = ord::count(&node.chain, p.chain_no).saturating_sub(TAIL);
    if with_scan(node, p, settled, |s| s.lo == 0 && s.hi >= settled) {
        return;
    }
    {
        let mut r = RUNNING.lock().unwrap();
        if !r.get_or_insert_with(HashSet::new).insert(p.chain_no) {
            return;
        }
    }
    let Some(arc) = NODE.get().and_then(|w| w.upgrade()) else {
        RUNNING.lock().unwrap().as_mut().map(|r| r.remove(&p.chain_no));
        return;
    };
    std::thread::spawn(move || {
        let node: &Node = &arc;
        let mut steps = 0;
        loop {
            let settled = ord::count(&node.chain, p.chain_no).saturating_sub(TAIL);
            let (lo, hi) = with_scan(node, p, settled, |s| (s.lo, s.hi));
            let (from, to, up) = if hi < settled {
                (hi, (hi + STEP).min(settled), true)
            } else if lo > 0 {
                (lo.saturating_sub(STEP), lo, false)
            } else {
                break;
            };
            let found = sites::records(&node.chain, p.chain_no, from, to);
            with_scan(node, p, settled, |s| {
                // Unless the list moved meanwhile (a reorg cut it back).
                if up && s.hi == from && to <= settled {
                    s.recs.extend(found);
                    s.hi = to;
                } else if !up && s.lo == to {
                    s.recs.splice(0..0, found);
                    s.lo = from;
                }
            });
            steps += 1;
            if steps % 64 == 0 {
                with_scan(node, p, settled, |s| save(node, p, s));
            }
        }
        let settled = ord::count(&node.chain, p.chain_no).saturating_sub(TAIL);
        with_scan(node, p, settled, |s| save(node, p, s));
        RUNNING.lock().unwrap().as_mut().map(|r| r.remove(&p.chain_no));
    });
}

/// The chain's newest records, newest first (at most sites::SCAN), and whether older ones are
/// still being listed.
fn newest(node: &Node, p: &'static ChainParams) -> (Vec<(u64, String)>, bool) {
    ensure(node, p);
    let total = ord::count(&node.chain, p.chain_no);
    let settled = total.saturating_sub(TAIL);
    let (lo, hi, mut recs) = with_scan(node, p, settled, |s| {
        let from = s.recs.len().saturating_sub(sites::SCAN);
        (s.lo, s.hi, s.recs[from..].to_vec())
    });
    let live_from = hi.max(total.saturating_sub(TAIL + LIVE_MAX));
    recs.extend(sites::records(&node.chain, p.chain_no, live_from, total));
    recs.reverse();
    recs.truncate(sites::SCAN);
    (recs, lo > 0 || live_from > hi)
}

/// Shared storage keeping only part of the chain's inscriptions: a record may be in a part
/// this arcade doesn't hold, so it doesn't answer for sites.
fn whole(node: &Node, p: &ChainParams) -> Result<(), String> {
    match crate::shares::groups_of(&node.share_of(p.key)["ord"]) {
        Some(_) => Err(format!("this arcade keeps only a part of {}'s inscriptions; an arcade keeping all of them answers for sites", p.name)),
        None => Ok(()),
    }
}

type Kept = Mutex<HashMap<String, (Instant, Value)>>;
static DIRS: std::sync::OnceLock<Kept> = std::sync::OnceLock::new();

/// "explorer.site": {name, claim, holder, page, scanning} from this arcade's own index.
pub fn site_here(node: &Node, p: &'static ChainParams, name: &str) -> Result<Value, String> {
    whole(node, p)?;
    let (recs, scanning) = newest(node, p);
    let mut v = sites::site(&node.chain, p, name, &recs);
    v["scanning"] = json!(scanning);
    Ok(v)
}

/// "explorer.sites": {names: [{name, page, number, height, title}], scanning}, newest first;
/// `all`: every name, not only .mesh names.
pub fn directory_here(node: &Node, p: &'static ChainParams, all: bool) -> Result<Value, String> {
    whole(node, p)?;
    let key = format!("{}|{all}", p.key);
    let kept = DIRS.get_or_init(Default::default);
    if let Some((at, v)) = kept.lock().unwrap().get(&key) {
        if at.elapsed().as_secs() < KEEP {
            return Ok(v.clone());
        }
    }
    let (recs, scanning) = newest(node, p);
    let v = json!({"names": sites::directory(&node.chain, p, &recs, !all), "scanning": scanning});
    kept.lock().unwrap().insert(key, (Instant::now(), v.clone()));
    Ok(v)
}

// ---------------- asking: here, else the mesh ----------------

fn keeps(node: &Node, ix: &str) -> Vec<&'static ChainParams> {
    let mut out = Vec::new();
    if crate::chains::home_indexes(node).iter().any(|i| i == ix) {
        out.push(node.params);
    }
    for r in node.extra.lock().unwrap().values() {
        if r.indexes.iter().any(|i| i == ix) {
            out.push(r.params);
        }
    }
    out
}

/// Chains whose names can be read: kept here, or by a V1.0 arcade on the mesh. The .mesh
/// chain first.
fn chains(node: &Node) -> Vec<&'static ChainParams> {
    let here = keeps(node, "names");
    let mut out: Vec<&'static ChainParams> = ALL_CHAINS
        .iter()
        .copied()
        .filter(|p| here.iter().any(|h| h.chain_no == p.chain_no) || !crate::shares::index_holders(node, p.net, "names").is_empty())
        .collect();
    out.sort_by_key(|p| p.net != MESH_NET);
    out
}

fn ask_site(node: &Node, p: &'static ChainParams, name: &str) -> Result<Value, String> {
    crate::explorer::handle(node, "explorer.site", &json!({"chain": p.key, "name": name}))
}

fn ask_directory(node: &Node, p: &'static ChainParams, all: bool) -> Result<Value, String> {
    crate::explorer::handle(node, "explorer.sites", &json!({"chain": p.key, "all": all}))
}

/// One question per chain, asked at the same time.
fn each<T: Send>(list: &[&'static ChainParams], f: impl Fn(&'static ChainParams) -> T + Sync) -> Vec<(&'static ChainParams, T)> {
    std::thread::scope(|s| {
        let f = &f;
        let hands: Vec<_> = list.iter().map(|p| (*p, s.spawn(move || f(p)))).collect();
        hands.into_iter().filter_map(|(p, h)| h.join().ok().map(|v| (p, v))).collect()
    })
}

fn mesh_chain() -> Result<&'static ChainParams, String> {
    by_net(MESH_NET).ok_or_else(|| "this arcade does not know the .mesh chain".to_string())
}

/// The .mesh directory.
fn mesh_names(node: &Node) -> Result<Vec<Value>, String> {
    Ok(ask_directory(node, mesh_chain()?, false)?["names"].as_array().cloned().unwrap_or_default())
}

/// Every name with a site on every chain that can be read, newest first, each with its
/// "chain". Kept KEEP seconds.
fn all_sites(node: &Node) -> Result<Vec<Value>, String> {
    static ALL: Mutex<Option<(Instant, Vec<Value>)>> = Mutex::new(None);
    if let Some((at, names)) = ALL.lock().unwrap().as_ref() {
        if at.elapsed().as_secs() < KEEP {
            return Ok(names.clone());
        }
    }
    let list = chains(node);
    let mut names = Vec::new();
    let (mut answered, mut why) = (false, format!("no arcade on the mesh offers the names index of {} right now", mesh_chain()?.name));
    for (p, said) in each(&list, |p| ask_directory(node, p, true)) {
        match said {
            Ok(v) => {
                answered = true;
                for n in v["names"].as_array().into_iter().flatten() {
                    let mut n = n.clone();
                    n["chain"] = json!(p.net);
                    names.push(n);
                }
            }
            Err(e) if p.net == MESH_NET => why = e,
            Err(_) => {} // a chain not read just now: the others
        }
    }
    if !answered {
        return Err(why);
    }
    names.sort_by_key(|n| std::cmp::Reverse(n["height"].as_u64().unwrap_or(0)));
    *ALL.lock().unwrap() = Some((Instant::now(), names.clone()));
    Ok(names)
}

// ---------------- the pages ----------------

fn page_fields(out: &mut Value, page: &Value) -> Map<String, Value> {
    let pages = page["pages"].as_object().cloned().unwrap_or_default();
    let mut paths: Vec<&String> = pages.keys().collect();
    paths.sort();
    out["page"] = page["id"].clone();
    out["number"] = page["number"].clone();
    out["size"] = page["size"].clone();
    out["sha256"] = page["sha256"].clone();
    out["paths"] = json!(paths);
    out["title"] = json!(sites::title_of(pages.get("/").and_then(sites::Page::from_json).as_ref()));
    pages
}

/// Where each inscription a page names was seen (its chain), for /content.
static SEEN: Mutex<Option<HashMap<String, &'static ChainParams>>> = Mutex::new(None);

fn note_seen(ids: &[String], p: &'static ChainParams) {
    let mut g = SEEN.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    if m.len() > 20_000 {
        m.clear();
    }
    for id in ids {
        m.insert(id.clone(), p);
    }
}

fn directory_page(node: &Node) -> Result<Value, String> {
    let names = mesh_names(node)?;
    let mut lines = vec!["# The .mesh web".to_string(), "Every .mesh site, newest first.".to_string(), String::new()];
    for n in names.iter().take(60) {
        let name = n["name"].as_str().unwrap_or("");
        let title = n["title"].as_str().filter(|t| !t.is_empty()).map(|t| format!(" - {t}")).unwrap_or_default();
        lines.push(format!("=> {name} {name}{title}"));
    }
    if names.is_empty() {
        lines.push("Nobody has published a page yet. Claim a .mesh name and publish a page on it.".to_string());
    }
    let body = lines.join("\n");
    let digest = hex::encode(<sha2::Sha256 as sha2::Digest>::digest(body.as_bytes()));
    Ok(json!({"name": "directory", "path": "/", "page": format!("directory-{}", &digest[..16]), "title": "The .mesh web", "body": body,
              "paths": ["/"], "size": body.len(), "number": null, "holder": null, "claim": null}))
}

/// /r/mesh/<address>: (status, answer).
fn mesh_page(node: &Node, address: &str) -> Result<(u16, Value), String> {
    let bare = address.trim_matches(|c| c == '/' || c == ' ').to_lowercase();
    if matches!(bare.as_str(), "" | "directory" | "mesh://directory") {
        return Ok((200, directory_page(node)?));
    }
    let (name, path) = sites::split_address(address, false);
    let Some(name) = name else { return Ok((400, json!({"detail": "not a .mesh name", "address": address}))) };
    let p = mesh_chain()?;
    let said = ask_site(node, p, &name)?;
    let mut out = json!({"name": name, "path": path, "holder": said["holder"], "claim": said["claim"]});
    if said["claim"].is_null() {
        out["detail"] = json!(format!("Nobody has claimed {name}."));
        return Ok((404, out));
    }
    if said["page"].is_null() {
        out["detail"] = json!(format!("{name} has no page yet."));
        return Ok((404, out));
    }
    let pages = page_fields(&mut out, &said["page"]);
    let Some(Value::String(body)) = pages.get(&path) else {
        out["detail"] = json!(format!("{name} has no page {path}."));
        return Ok((404, out));
    };
    let pictures = sites::pictures_of(body);
    note_seen(&pictures, p);
    out["body"] = json!(body);
    out["pictures"] = json!(pictures);
    Ok((200, out))
}

/// /r/site/<address>?chain=: (status, answer, the page itself when there is one).
fn any_site(node: &Node, address: &str, chain: &str) -> (u16, Value, Option<sites::Page>) {
    let (name, path) = sites::split_address(address, true);
    let Some(name) = name else {
        return (400, json!({"detail": "not a name: label.ending, like yours.mesh or yours.pepe", "address": address}), None);
    };
    let nets: Vec<&'static ChainParams> = if !chain.is_empty() {
        by_net(chain).or_else(|| by_key(chain)).into_iter().collect()
    } else if sites::is_mesh(&name) {
        by_net(MESH_NET).into_iter().collect()
    } else {
        chains(node)
    };
    let found: Vec<(&'static ChainParams, Value)> = each(&nets, |p| ask_site(node, p, &name)).into_iter().filter_map(|(p, r)| r.ok().filter(|s| !s["claim"].is_null()).map(|s| (p, s))).collect();
    let with_page: Vec<&(&'static ChainParams, Value)> = found.iter().filter(|(_, s)| !s["page"].is_null()).collect();
    let mut out = json!({"name": name, "path": path});
    if with_page.len() > 1 {
        out["detail"] = json!(format!("{name} has a site on more than one chain"));
        out["choices"] = with_page.iter().map(|(p, s)| json!({"chain": p.net, "page": s["page"]["id"], "holder": s["holder"]})).collect();
        return (300, out, None);
    }
    if found.is_empty() {
        // A mistyped name is offered the sites it is closest to.
        let mut known: Vec<String> = all_sites(node).unwrap_or_default().iter().filter_map(|n| n["name"].as_str().map(String::from)).collect();
        known.sort();
        known.dedup();
        let close = close_matches(&name, &known, 3, 0.75);
        let on = if chain.is_empty() { String::new() } else { format!(" on {chain}") };
        let meant = if close.is_empty() { String::new() } else { format!(" Did you mean {}?", close.join(" or ")) };
        out["detail"] = json!(format!("Nobody has claimed {name}{on}.{meant}"));
        out["suggest"] = json!(close);
        return (404, out, None);
    }
    let (p, said) = with_page.first().copied().unwrap_or(&found[0]);
    out["chain"] = json!(p.net);
    out["holder"] = said["holder"].clone();
    out["claim"] = said["claim"].clone();
    if said["page"].is_null() {
        out["detail"] = json!(format!("{name} has no site yet."));
        return (404, out, None);
    }
    let pages = page_fields(&mut out, &said["page"]);
    let Some(page) = pages.get(&path).and_then(sites::Page::from_json) else {
        out["detail"] = json!(format!("{name} has no page {path}."));
        return (404, out, None);
    };
    out["kind"] = json!(page.kind());
    match &page {
        sites::Page::Text(body) => {
            let pictures = sites::pictures_of(body);
            note_seen(&pictures, p);
            out["body"] = json!(body);
            out["pictures"] = json!(pictures);
        }
        other => {
            out["url"] = json!(format!("/site/{}/{}{path}", quote(p.net), quote(&name)));
            if let sites::Page::Id(id) = other {
                note_seen(std::slice::from_ref(id), p);
                out["inscription"] = json!(id);
            }
        }
    }
    (200, out, Some(page))
}

// ---------------- close names (Python's difflib.get_close_matches) ----------------

/// Characters of `a` and `b` in matching blocks, longest block first and then each side of it
/// (difflib.SequenceMatcher without junk).
fn matching(a: &[char], b: &[char]) -> usize {
    if a.is_empty() || b.is_empty() {
        return 0;
    }
    // The longest block; of equals the one ending first in a, then first in b.
    let (mut best_i, mut best_j, mut best) = (0, 0, 0);
    let mut prev = vec![0usize; b.len() + 1];
    for i in 0..a.len() {
        let mut row = vec![0usize; b.len() + 1];
        for j in 0..b.len() {
            if a[i] == b[j] {
                let k = prev[j] + 1;
                row[j + 1] = k;
                if k > best {
                    (best_i, best_j, best) = (i + 1 - k, j + 1 - k, k);
                }
            }
        }
        prev = row;
    }
    if best == 0 {
        return 0;
    }
    best + matching(&a[..best_i], &b[..best_j]) + matching(&a[best_i + best..], &b[best_j + best..])
}

fn ratio(a: &str, b: &str) -> f64 {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    2.0 * matching(&a, &b) as f64 / (a.len() + b.len()) as f64
}

/// The `n` of `known` most like `word`, the closest first, each at least `cutoff` alike.
fn close_matches(word: &str, known: &[String], n: usize, cutoff: f64) -> Vec<String> {
    let mut scored: Vec<(f64, &String)> = known.iter().map(|x| (ratio(x, word), x)).filter(|(r, _)| *r >= cutoff).collect();
    scored.sort_by(|x, y| y.0.partial_cmp(&x.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| y.1.cmp(x.1)));
    scored.into_iter().take(n).map(|(_, x)| x.clone()).collect()
}

// ---------------- drawing ----------------

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#x27;")
}

/// Percent-encoding of a URL path part (Python's urllib.parse.quote).
fn quote(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// MeshText as HTML, line by line. Links open under `base`; `here` is the site's own name.
fn draw(text: &str, here: &str, base: &str) -> String {
    let (mut out, mut pre) = (Vec::new(), false);
    for line in sites::py_lines(text) {
        if line.starts_with("```") {
            out.push(if pre { "</pre>".to_string() } else { "<pre>".to_string() });
            pre = !pre;
            continue;
        }
        if pre {
            out.push(esc(line));
            continue;
        }
        let s = line.trim_end();
        if let Some(rest) = s.strip_prefix("=>") {
            let mut parts = rest.trim().splitn(2, char::is_whitespace);
            if let Some(target) = parts.next().filter(|t| !t.is_empty()) {
                let to = if target.starts_with('/') { format!("{here}{target}") } else { target.strip_prefix("mesh://").unwrap_or(target).to_string() };
                let label = parts.next().map(str::trim_start).filter(|l| !l.is_empty()).unwrap_or(target);
                out.push(format!("<p class=\"l\"><a href=\"{}\">{}</a></p>", esc(&format!("{base}{to}")), esc(label)));
            }
        } else if let Some((alt, id)) = picture_line(s.trim()) {
            out.push(format!("<p class=\"pic\"><img src=\"/content/{}\" alt=\"{}\" style=\"max-width:100%;image-rendering:pixelated\"></p>", esc(&id), esc(alt)));
        } else if let Some(h) = s.strip_prefix("### ") {
            out.push(format!("<h3>{}</h3>", esc(h)));
        } else if let Some(h) = s.strip_prefix("## ") {
            out.push(format!("<h2>{}</h2>", esc(h)));
        } else if let Some(h) = s.strip_prefix("# ") {
            out.push(format!("<h1>{}</h1>", esc(h)));
        } else if let Some(li) = s.strip_prefix("* ") {
            out.push(format!("<p class=\"li\">&bull; {}</p>", esc(li)));
        } else if let Some(q) = s.strip_prefix('>') {
            out.push(format!("<blockquote>{}</blockquote>", esc(q.trim())));
        } else if !s.is_empty() {
            out.push(format!("<p>{}</p>", esc(s)));
        }
    }
    if pre {
        out.push("</pre>".to_string());
    }
    out.join("\n")
}

/// "![alt](inscription id)" -> (alt, id).
fn picture_line(s: &str) -> Option<(&str, String)> {
    let rest = s.strip_prefix("![")?;
    let (alt, rest) = rest.split_once("](")?;
    let id = rest.strip_suffix(')')?.trim().to_lowercase();
    (!alt.contains(']') && sites::is_inscription_id(&id)).then_some((alt, id))
}

// ---------------- the routes ----------------

pub struct Reply {
    pub code: u16,
    pub ctype: &'static str,
    pub location: Option<String>,
    pub body: Vec<u8>,
}

fn reply_json(code: u16, v: &Value) -> Reply {
    Reply { code, ctype: "application/json", location: None, body: v.to_string().into_bytes() }
}

fn reply_html(code: u16, body: String) -> Reply {
    Reply { code, ctype: "text/html; charset=utf-8", location: None, body: body.into_bytes() }
}

fn unavailable(e: String) -> Reply {
    reply_json(503, &json!({"detail": e}))
}

/// A path's address part as typed ("+" stays a plus).
fn decode(s: &str) -> String {
    crate::http::pct_decode(&s.replace('+', "%2B"))
}

fn unchanged(code: u16, mut out: Value, have: &str) -> Value {
    if code == 200 && !have.is_empty() && out["page"] == have && !out["body"].is_null() {
        out.as_object_mut().unwrap().remove("body");
        out["unchanged"] = json!(true);
    }
    out
}

/// The page routes above, or None when `path` is not one of them. /r/mesh/picture and
/// /r/search are not served.
pub fn route(node: &Node, path: &str, q: &Map<String, Value>) -> Option<Reply> {
    let qs = |k: &str| q.get(k).map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).unwrap_or_default();
    let qn = |k: &str, d: u64| q.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(d);
    if path == "/r/mesh" {
        return Some(match mesh_names(node) {
            Ok(names) => reply_json(200, &json!({"names": names})),
            Err(e) => unavailable(e),
        });
    }
    if path == "/r/sites" {
        let (page, per) = (qn("page", 0) as usize, qn("per", 50).clamp(1, 200) as usize);
        return Some(match all_sites(node) {
            Ok(names) => {
                let part: Vec<&Value> = names.iter().skip(page.saturating_mul(per)).take(per).collect();
                reply_json(200, &json!({"names": part, "total": names.len(), "page": page, "pages": names.len().div_ceil(per)}))
            }
            Err(e) => unavailable(e),
        });
    }
    if let Some(address) = path.strip_prefix("/r/mesh/") {
        if address.starts_with("picture/") {
            return None;
        }
        return Some(match mesh_page(node, &decode(address)) {
            Ok((code, out)) => reply_json(code, &unchanged(code, out, &qs("have"))),
            Err(e) => unavailable(e),
        });
    }
    if let Some(address) = path.strip_prefix("/r/site/") {
        let (code, out, _) = any_site(node, &decode(address), &qs("chain"));
        return Some(reply_json(code, &unchanged(code, out, &qs("have"))));
    }
    if let Some(rest) = path.strip_prefix("/site/") {
        let (chain, address) = rest.split_once('/').unwrap_or((rest, ""));
        let chain = decode(chain);
        let (code, out, page) = any_site(node, &decode(address), &chain);
        let head = "<!doctype html><meta charset=utf-8>";
        return Some(match page {
            Some(sites::Page::Id(id)) => Reply { code: 307, ctype: "text/plain", location: Some(format!("/content/{}", quote(&id))), body: Vec::new() },
            Some(sites::Page::Html(html)) => reply_html(200, html),
            Some(sites::Page::Text(body)) => {
                let name = out["name"].as_str().unwrap_or("");
                let title = out["title"].as_str().filter(|t| !t.is_empty()).unwrap_or(name);
                let base = format!("/site/{}/", quote(out["chain"].as_str().unwrap_or(&chain)));
                reply_html(200, format!("{head}<meta name=viewport content='width=device-width,initial-scale=1'><title>{}</title>{}", esc(title), draw(&body, name, &base)))
            }
            None => reply_html(code, format!("{head}<p>{}</p>", esc(out["detail"].as_str().unwrap_or("")))),
        });
    }
    None
}

// ---------------- /content/<inscription id> ----------------

/// An ord inscription's (content type, bytes) for /content: from a chain this arcade keeps the
/// ord index of, else (`ask`: the request came from this computer) from an arcade on the mesh,
/// which sends pictures up to 4 MB and text up to 64 KB.
pub fn content(node: &Node, id: &str, ask: bool) -> Option<(String, Vec<u8>)> {
    let id = id.to_lowercase();
    if !sites::is_inscription_id(&id) {
        return None;
    }
    static KEPT: Mutex<Option<(usize, HashMap<String, (String, Vec<u8>)>)>> = Mutex::new(None);
    if let Some(hit) = KEPT.lock().unwrap().as_ref().and_then(|k| k.1.get(&id).cloned()) {
        return Some(hit);
    }
    let here = keeps(node, "ord");
    let mut got = here.iter().find_map(|p| ord::content(&node.chain, p, &id).ok());
    if got.is_none() && ask {
        let seen = SEEN.lock().unwrap().as_ref().and_then(|m| m.get(&id).copied());
        let mut list: Vec<&'static ChainParams> = seen.into_iter().collect();
        if seen.is_none() {
            list = chains(node);
        }
        list.retain(|p| !here.iter().any(|h| h.chain_no == p.chain_no));
        got = each(&list, |p| crate::explorer::handle(node, "explorer.inscription", &json!({"chain": p.key, "id": id}))).into_iter().find_map(|(_, r)| {
            let c = r.ok()?["content"].take();
            let ct = c["type"].as_str().unwrap_or("").to_string();
            match c["kind"].as_str()? {
                "image" => {
                    let b64 = c["data"].as_str()?.split_once(";base64,")?.1;
                    Some((ct, base64::engine::general_purpose::STANDARD.decode(b64).ok()?))
                }
                // Whole only: a cut text is not the inscription.
                "text" | "html" => c["text"].as_str().filter(|t| Some(t.len() as u64) == c["size"].as_u64()).map(|t| (ct, t.as_bytes().to_vec())),
                _ => None,
            }
        });
    }
    if let Some((ct, body)) = &got {
        let mut g = KEPT.lock().unwrap();
        let k = g.get_or_insert_with(|| (0, HashMap::new()));
        if k.0 + body.len() > CONTENT_KEEP {
            *k = (0, HashMap::new());
        }
        if body.len() <= CONTENT_KEEP / 4 {
            k.0 += body.len();
            k.1.insert(id, (ct.clone(), body.clone()));
        }
    }
    got
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn close_names() {
        // Values from Python: difflib.SequenceMatcher(None, a, b).ratio().
        assert!((ratio("me-seeks.mesh", "mee-seeks.mesh") - 0.9629629629629629).abs() < 1e-12);
        assert!((ratio("ribbit.pepe", "rabbit.pepe") - 0.9090909090909091).abs() < 1e-12);
        assert!((ratio("abcd", "bcde") - 0.75).abs() < 1e-12);
        assert_eq!(ratio("abc", "xyz"), 0.0);
        let known: Vec<String> = ["apple.mesh", "ape.mesh", "apply.mesh", "maple.mesh", "zebra.pepe"].iter().map(|s| s.to_string()).collect();
        assert_eq!(close_matches("appel.mesh", &known, 3, 0.75), vec!["apply.mesh", "apple.mesh", "ape.mesh"]);
        assert!(close_matches("nothing.like", &known, 3, 0.75).is_empty());
    }

    #[test]
    fn drawn() {
        let id = format!("{}i0", "ab".repeat(32));
        let text = format!("# Hi <you>\n=> /about About us\n=> mesh://other.mesh\n![A \"cat\"]({id})\n* one\n> said\n```\n<b>\n```\nplain  ");
        let html = draw(&text, "yours.mesh", "/site/main/");
        assert_eq!(html, format!("<h1>Hi &lt;you&gt;</h1>\n<p class=\"l\"><a href=\"/site/main/yours.mesh/about\">About us</a></p>\n<p class=\"l\"><a href=\"/site/main/other.mesh\">mesh://other.mesh</a></p>\n<p class=\"pic\"><img src=\"/content/{id}\" alt=\"A &quot;cat&quot;\" style=\"max-width:100%;image-rendering:pixelated\"></p>\n<p class=\"li\">&bull; one</p>\n<blockquote>said</blockquote>\n<pre>\n&lt;b&gt;\n</pre>\n<p>plain</p>"));
        assert_eq!(quote("yours.mesh/a b"), "yours.mesh/a%20b");
    }
}
