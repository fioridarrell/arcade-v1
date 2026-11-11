//! Ringing a person whose app is closed (arcade.realtime.ring / ringed): the original arcade's
//! rings, wire for wire (its ring module; docs/APPCOMPAT.md gap 7).
//!
//! An app asks its arcade to ring an address. The ring reaches that person only when all of this
//! holds, checked by the arcade of the person being rung:
//!
//! - they turned on Notifications on that app's card;
//! - the caller is in their address book;
//! - the caller is who they say: the ring carries the caller's cert (the one a room join shows),
//!   signed for the arcade the ring came through;
//! - not too often: one ring a pair every `RING_GAP` seconds, `RING_HOUR` an hour.
//!
//! Over the mesh: an arcade holds the room "arcade!ring/<address>" for its own address while
//! somebody can be told here (the desktop app is open) and some app is switched on. A caller's
//! arcade joins that room as a guest, says the ring and stays `SPEAK_FOR` seconds. "!" is no part
//! of a game's name, so no page can join these rooms through arcade.realtime.
//!
//! A ring says who, which app (the page it was sent from), an optional line and a short link the
//! app gets back when it is opened from the ring (arcade.realtime.ringed()). Nothing else.

use crate::node::Node;
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const ROOM: &str = "arcade!ring/";
const SPEAK_FOR: Duration = Duration::from_secs(60);
/// Seconds between two rings from the same caller to the same person.
const RING_GAP: u64 = 15;
/// Rings a pair an hour.
const RING_HOUR: usize = 30;
/// Rings one caller may send a minute, to anybody.
const SEND_MINUTE: usize = 20;
const RING_LINE: usize = 80;
const RING_LINK: usize = 200;
/// The desktop app counts as open this long after it last asked what is ringing.
const SEEN_FOR: Duration = Duration::from_secs(20);
/// How long the open desktop app is told about a ring.
const SHOW_FOR: u64 = 45;
/// How long a ring is remembered for the app it opens.
const KEEP: u64 = 600;
/// A room message carries this many bytes at most (crate::realtime).
const MAX_SAID: usize = 512;
/// How long the app a page belongs to is remembered.
const PAGE_FOR: Duration = Duration::from_secs(60);

#[derive(Clone)]
struct Ring {
    id: String,
    from: String,
    to: String,
    /// The app's id here, and the page the caller sent it from.
    app: Value,
    at: u64,
    line: String,
    link: String,
}

#[derive(Default)]
struct St {
    /// The address whose room is held, and the session holding it.
    held: Option<(String, String)>,
    /// address -> (session, last ring said): rooms this arcade is ringing in.
    speaking: HashMap<String, (String, Instant)>,
    pairs: HashMap<(String, String), Vec<u64>>,
    sent: HashMap<String, Vec<u64>>,
    heard: VecDeque<Ring>,
    seen: Option<Instant>,
    pages: HashMap<String, (Option<Value>, Instant)>,
}

fn st() -> &'static Mutex<St> {
    static S: OnceLock<Mutex<St>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(St::default()))
}

fn room(address: &str) -> String {
    format!("{ROOM}{address}")
}

/// Whitespace runs become one space, control characters go; at most `most` characters.
fn clean_line(text: &str, most: usize) -> String {
    let words: Vec<&str> = text.split(|c: char| c.is_whitespace() || c.is_control()).filter(|w| !w.is_empty()).collect();
    words.join(" ").chars().take(most).collect()
}

/// The shape of an address (any chain's): letters and digits, 25 to 64 of them.
fn is_address(s: &str) -> bool {
    (25..=64).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn is_page(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn recent(times: &mut Vec<u64>, within: u64, now: u64) {
    times.retain(|t| now.saturating_sub(*t) < within);
}

fn may_send(s: &mut St, caller: &str, now: u64) -> bool {
    let times = s.sent.entry(caller.to_string()).or_default();
    recent(times, 60, now);
    if times.len() >= SEND_MINUTE {
        return false;
    }
    times.push(now);
    true
}

fn may_ring(s: &mut St, caller: &str, callee: &str, now: u64) -> bool {
    let times = s.pairs.entry((caller.to_string(), callee.to_string())).or_default();
    recent(times, 3600, now);
    if times.len() >= RING_HOUR || times.last().is_some_and(|t| now.saturating_sub(*t) < RING_GAP) {
        return false;
    }
    times.push(now);
    true
}

/// The app a page belongs to, remembered for a minute (finding it reads every app card).
fn app_of(node: &Node, page: &str) -> Option<Value> {
    if let Some((v, at)) = st().lock().unwrap().pages.get(page) {
        if at.elapsed() < PAGE_FOR {
            return v.clone();
        }
    }
    let v = crate::appsvc::of_page(node, page);
    let mut s = st().lock().unwrap();
    s.pages.retain(|_, (_, at)| at.elapsed() < PAGE_FOR);
    s.pages.insert(page.to_string(), (v.clone(), Instant::now()));
    v
}

fn my_address(node: &Node) -> Option<String> {
    crate::msgsvc::sender_key(node).ok().map(|k| k.0)
}

// ---------------- the arcade of the person being rung ----------------

/// The address to hold a ring room for now: ours, while the desktop app is open and some app
/// has Notifications on.
fn wanted(node: &Node) -> Option<String> {
    if !st().lock().unwrap().seen.is_some_and(|t| t.elapsed() < SEEN_FOR) {
        return None;
    }
    if crate::appsvc::notify_ids(node).is_empty() {
        return None;
    }
    my_address(node)
}

/// A ring said for `to` (this arcade's address), heard through the arcade `origin`: kept only if
/// every rule holds.
/// What a said ring must be before anything is looked up: for this address, from someone else,
/// about a page, and naming the arcade it came through (its cert is the caller's for that arcade).
fn well_said(said: &Value, to: &str, origin: &str) -> bool {
    let (caller, page) = (said["f"].as_str().unwrap_or(""), said["app"].as_str().unwrap_or(""));
    is_page(page) && is_address(caller) && caller != to && said["to"].as_str() == Some(to) && said["n"].as_str() == Some(origin)
}

fn deliver(node: &Node, to: &str, said: &Value, origin: &str) -> bool {
    if !well_said(said, to, origin) {
        return false;
    }
    let (caller, page) = (said["f"].as_str().unwrap_or(""), said["app"].as_str().unwrap_or(""));
    if !crate::msgsvc::in_book(node, caller) {
        return false;
    }
    let Some(app) = app_of(node, page) else { return false };
    if crate::appsvc::perms(node, app["id"].as_str().unwrap_or(""))["notify"] != true {
        return false;
    }
    if !crate::realtime::cert_ok(node, origin, caller, said["x"].as_u64().unwrap_or(0), said["s"].as_str().unwrap_or("")) {
        return false;
    }
    let now = arcade_app::now();
    let mut s = st().lock().unwrap();
    if !may_ring(&mut s, caller, to, now) {
        return false;
    }
    let mut app = app;
    app["page"] = json!(page);
    s.heard.push_back(Ring {
        id: arcade_mesh::random::hex(8),
        from: caller.to_string(),
        to: to.to_string(),
        app,
        at: now,
        line: clean_line(said["line"].as_str().unwrap_or(""), RING_LINE),
        link: said["link"].as_str().unwrap_or("").chars().filter(|c| !c.is_control()).take(RING_LINK).collect(),
    });
    while s.heard.front().is_some_and(|r| now.saturating_sub(r.at) > KEEP) {
        s.heard.pop_front();
    }
    true
}

pub fn run(node: Arc<Node>) {
    loop {
        // Hold exactly the room wanted now.
        let want = wanted(&node);
        let held = st().lock().unwrap().held.clone();
        if held.as_ref().map(|h| &h.0) != want.as_ref() {
            if let Some((_, tok)) = &held {
                let _ = crate::realtime::leave(&node, tok);
            }
            let now = want.and_then(|a| crate::realtime::join_guest(&node, &room(&a)).ok().map(|tok| (a, tok)));
            st().lock().unwrap().held = now;
        }
        // Rooms this arcade rang in: left SPEAK_FOR after the last ring, kept until then.
        let speaking: Vec<(String, String, Instant)> = st().lock().unwrap().speaking.iter().map(|(a, (t, at))| (a.clone(), t.clone(), *at)).collect();
        for (address, tok, at) in speaking {
            if at.elapsed() > SPEAK_FOR {
                st().lock().unwrap().speaking.remove(&address);
                let _ = crate::realtime::leave(&node, &tok);
            } else {
                let _ = crate::realtime::poll(&tok, 0);
            }
        }
        // What the held room heard.
        let held = st().lock().unwrap().held.clone();
        let Some((address, tok)) = held else {
            std::thread::sleep(Duration::from_secs(1));
            continue;
        };
        let got = crate::realtime::poll(&tok, 1000).unwrap_or(Value::Null);
        if got["gone"] == true {
            st().lock().unwrap().held = None;
            continue;
        }
        for ev in got["events"].as_array().into_iter().flatten() {
            if ev["type"] != "message" {
                continue;
            }
            let Some(said) = ev["data"].as_str().and_then(|d| serde_json::from_str::<Value>(d).ok()) else { continue };
            if said["t"] == "ring" {
                deliver(&node, &address, &said, ev["member"]["node"].as_str().unwrap_or(""));
            }
        }
    }
}

fn shown(node: &Node, r: &Ring) -> (String, String) {
    let tag = arcade_ledger::index::tag_of(&node.chain, node.params.chain_no, &r.from).ok().flatten().unwrap_or_default();
    let from = if tag.is_empty() { format!("{}\u{2026}", r.from.chars().take(10).collect::<String>()) } else { format!("@{tag}") };
    (tag, from)
}

/// ring.waiting: the desktop app is open (it can be rung here), and what is ringing now.
pub fn waiting(node: &Node) -> Value {
    let me = my_address(node).unwrap_or_default();
    let now = arcade_app::now();
    let rings: Vec<Ring> = {
        let mut s = st().lock().unwrap();
        s.seen = Some(Instant::now());
        s.heard.iter().filter(|r| r.to == me && now.saturating_sub(r.at) < SHOW_FOR).cloned().collect()
    };
    let out: Vec<Value> = rings
        .iter()
        .map(|r| {
            let (tag, from) = shown(node, r);
            json!({"id": r.id, "from": from, "address": r.from, "tag": tag, "app": r.app["name"], "chain": r.app["chain"], "txid": r.app["txid"],
                   "line": r.line, "at": r.at})
        })
        .collect();
    json!({"rings": out})
}

/// ring.get: the ring an app window was opened from, for that app only.
pub fn get(node: &Node, id: &str, page: &str) -> Value {
    let me = my_address(node).unwrap_or_default();
    let ring = st().lock().unwrap().heard.iter().find(|r| r.id == id && r.to == me).cloned();
    let Some(r) = ring else { return json!({"ring": null}) };
    if r.app["page"] != page && app_of(node, page).map(|a| a["id"].clone()) != Some(r.app["id"].clone()) {
        return json!({"ring": null});
    }
    let (tag, _) = shown(node, &r);
    json!({"ring": {"from": r.from, "tag": tag, "link": r.link, "line": r.line, "at": r.at}})
}

// ---------------- the caller's arcade ----------------

/// ring.send: ring `to` from this arcade's player, for the app whose window asks (`page`).
pub fn send(node: &Node, page: &str, to: &str, line: &str, link: &str) -> Result<Value, String> {
    let Some(me) = my_address(node) else { return Err("unlock your wallet: a ring says who you are".into()) };
    let to = to.trim();
    if !is_page(page) || !is_address(to) || to == me {
        return Err("ring whom, from which app?".into());
    }
    if node.links.lock().unwrap().is_empty() {
        return Ok(json!({"rung": false, "why": "this arcade is not on the mesh"}));
    }
    let Some((me, cert)) = crate::realtime::my_cert(node) else { return Err("unlock your wallet: a ring says who you are".into()) };
    let ring = json!({"t": "ring", "f": me, "x": cert["x"], "s": cert["s"], "n": node.id_hex(), "to": to, "app": page,
                      "line": clean_line(line, RING_LINE), "link": link.chars().filter(|c| !c.is_control()).take(RING_LINK).collect::<String>()});
    let said = ring.to_string();
    if said.len() > MAX_SAID {
        let over = said.len() - MAX_SAID;
        return Err(format!("too long: the ring's line and link are {over} byte{} more than a ring carries", if over == 1 { "" } else { "s" }));
    }
    if !may_send(&mut st().lock().unwrap(), &me, arcade_app::now()) {
        return Err("too many rings: wait a minute".into());
    }
    let full = room(to);
    if !crate::realtime::room_live(&full) {
        return Ok(json!({"rung": false, "why": "they have no phone to wake for this app, or their arcade is away"}));
    }
    let tok = st().lock().unwrap().speaking.get_mut(to).map(|s| {
        s.1 = Instant::now();
        s.0.clone()
    });
    match tok {
        Some(tok) => {
            crate::realtime::send(node, &tok, Some(&said), None)?;
        }
        None => {
            let tok = crate::realtime::join_guest(node, &full)?;
            st().lock().unwrap().speaking.insert(to.to_string(), (tok.clone(), Instant::now()));
            // The room's other members hear of the newcomer first.
            std::thread::sleep(Duration::from_millis(300));
            crate::realtime::send(node, &tok, Some(&said), None)?;
        }
    }
    Ok(json!({"rung": true}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ring_is_for_this_address_through_the_arcade_it_names() {
        let (me, them, node, page) = ("ngaWgK7LWYfkSPgSN2zARCyyEQypJxaRWY", "nVrY9PSLXmV5LP6NN5h4XjqfDNu8uq24N2", "ab".repeat(32), "7".repeat(64));
        let good = json!({"t": "ring", "f": them, "n": node, "to": me, "app": page});
        assert!(well_said(&good, me, &node));
        let with = |k: &str, v: &str| { let mut r = good.clone(); r[k] = json!(v); r };
        assert!(!well_said(&good, me, &"cd".repeat(32)), "said through another arcade than it names");
        assert!(!well_said(&with("n", &"cd".repeat(32)), me, &node));
        assert!(!well_said(&with("to", them), me, &node), "a ring for someone else");
        assert!(!well_said(&with("f", me), me, &node), "nobody rings themselves");
        assert!(!well_said(&with("f", "x"), me, &node) && !well_said(&with("app", "page"), me, &node));
    }

    #[test]
    fn lines_are_cleaned_like_the_original() {
        // python3: " ".join("  a \n b\t c ".split())[:4]
        assert_eq!(clean_line("  a \n b\t c ", 4), "a b ");
        assert_eq!(clean_line("é☃😀x", 3), "é☃😀");
        assert_eq!(clean_line("", 80), "");
        assert_eq!(clean_line("come  and\nplay\u{7}", 80), "come and play");
        assert!(is_address("nVrY9PSLXmV5LP6NN5h4XjqfDNu8uq24N2") && !is_address("not-an-address") && !is_address("") && !is_address(&"a".repeat(65)));
    }

    #[test]
    fn a_pair_rings_once_every_gap_and_thirty_an_hour() {
        let mut s = St::default();
        assert!(may_ring(&mut s, "a", "b", 1000));
        assert!(!may_ring(&mut s, "a", "b", 1000 + RING_GAP - 1));
        assert!(may_ring(&mut s, "b", "a", 1001), "the other way is its own pair");
        assert!(may_ring(&mut s, "a", "b", 1000 + RING_GAP));
        let mut t = 1000 + RING_GAP;
        for _ in 2..RING_HOUR {
            t += RING_GAP;
            assert!(may_ring(&mut s, "a", "b", t));
        }
        assert!(!may_ring(&mut s, "a", "b", t + RING_GAP), "the hour's rings are used up");
        assert!(may_ring(&mut s, "a", "b", 1000 + 3600), "the first ring is an hour old");
    }

    #[test]
    fn a_caller_sends_twenty_a_minute() {
        let mut s = St::default();
        for i in 0..SEND_MINUTE as u64 {
            assert!(may_send(&mut s, "a", 500 + i));
        }
        assert!(!may_send(&mut s, "a", 530));
        assert!(may_send(&mut s, "b", 530));
        assert!(may_send(&mut s, "a", 560));
    }

    #[test]
    fn ring_rooms_are_no_page_room() {
        assert_eq!(room("nAbc"), "arcade!ring/nAbc");
        assert!(is_page(&"ab01".repeat(16)));
        assert!(!is_page(&"AB01".repeat(16)));
        assert!(!is_page("ab01"));
    }
}
