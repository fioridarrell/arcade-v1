//! The ARCADE faucets: two of them (Martin 2026-11-17), both paying in two coins (an a:// act spends two).
//!
//! The user faucet (2026-11-15): a user who signs in through a website gets their ARCADE address topped up to
//! 0.05 ARCADE once a day (0.01 until 2026-11-16: too little for a comment, whose text blob costs about 0.02), so
//! they can publish an a:// profile, like, comment and tip without buying coins first. Only a website's arcade asks,
//! with the user's address (`faucet.arc {address}`); an arcade doesn't ask it for its own wallet.
//!
//! The node faucet: an arcade (new, or running already) gets 1 ARCADE once, from one of the others, so it can act on
//! the network from the start and then earn its own. Once per node ID and per address: a payer pays an address that
//! received no more than top-ups (TARGET in all), from a node no payer has paid (payment notices name the node).
//!
//! Mesh frame `t:"af"`, a kind of its own (not the welcome protocol's `wl`, which old arcades
//! answer with test PEP). Records are Ed25519-signed by the arcade's node key `a`, as `wl`'s:
//!   ask  {a, addr, at, k:"ask"}               top `addr` up to TARGET, please (one per address a day)
//!   node {a, addr, at, k:"node"}              node `a` asks NODE_GIFT for its wallet's `addr`, once
//!   sent {a, addr, at, k:"sent", txid[, node]} paid it in `txid` (`node`: the node faucet's payment to that node)
//! Asks are kept per address, so one arcade (a website with many users) may ask for many.
//!
//! The payer looks at the address on its own copy of the chain (blocks and pool) and sends only what is missing, so
//! a second payer that comes later sends nothing. Limits per payer and UTC day: DAILY_CAP top-ups, PER_ASKER of them
//! for one asking arcade, NODE_DAILY_CAP node gifts; it keeps KEEP for itself.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

/// What a signed-in user's ARCADE address is topped up to: 0.05 ARCADE (about two comments, or eighty likes).
pub const TARGET: u64 = 5_000_000;
/// Top-ups one arcade pays per UTC day (at most 25 ARCADE a day).
pub const DAILY_CAP: usize = 500;
/// Of those, for addresses one asking arcade named.
pub const PER_ASKER: usize = 200;
/// What the node faucet gives an arcade once: 1 ARCADE, in two coins.
pub const NODE_GIFT: u64 = 100_000_000;
/// Node gifts one arcade pays per UTC day (at most 100 ARCADE a day).
pub const NODE_DAILY_CAP: usize = 100;
/// An arcade asks for its node gift again this often until it has it (an ask made before payers linked is heard).
const NODE_ASK_EVERY: u64 = 300;
/// Most nodes kept as paid.
const MAX_NODES: usize = 20_000;
/// Never given away: kept for the payer's own a:// records.
pub const KEEP: u64 = 100_000_000;
/// Kept back for the payment's own fee.
const FEE_MARGIN: u64 = 1_000_000;
/// An ask counts this long.
const ASK_SECS: u64 = 86_400;
/// Clock difference allowed for a record dated ahead.
const SKEW_SECS: u64 = 600;
/// A payer waits a share of this after an ask, different per payer, so payers don't all pay
/// the same ask at once (the one that comes later sees the coins and sends nothing).
const SPREAD_SECS: u64 = 60;
/// A payment notice keeps others from paying the address for this long; after it the balance
/// alone decides (a notice is only its sender's word).
const SENT_HOLD: u64 = 600;
/// Most addresses kept with an open ask, and with payment notices: past it a new one is
/// neither kept nor passed on (asks are signed, but node keys cost nothing to make).
const MAX_ADDRS: usize = 20_000;

#[derive(Default)]
struct State {
    /// The newest ask per address.
    asks: HashMap<String, Value>,
    /// Payment notices seen per address: (txid, at).
    sent: HashMap<String, Vec<(String, u64)>>,
    /// Addresses found at TARGET or more (or paid) by this arcade, with when.
    done: HashMap<String, u64>,
    /// The newest node-gift ask per node ID.
    nodes: HashMap<String, Value>,
    /// Nodes paid, as this arcade paid or heard (node ID -> (address, when)); kept in the store too.
    paid_nodes: Option<HashMap<String, (String, u64)>>,
    /// Addresses found past their node gift (they hold or received it): not paid one.
    gifted: HashSet<String>,
    /// When this arcade last asked its own node gift (seconds), and its signed ask.
    node_asked: u64,
    own_ask: Option<Value>,
    /// This arcade has its node gift: it stops asking.
    node_done: bool,
}

fn state() -> &'static Mutex<State> {
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// The ARCADE chain running here, or why there is none.
fn chain() -> Result<&'static ChainParams, String> {
    crate::arcledger::chain()
}

fn script_of(p: &ChainParams, addr: &str) -> Result<Vec<u8>, String> {
    arcade_ledger::address::p2pkh_script_for_address(p, addr).ok_or_else(|| format!("{addr} is not an {} address", p.name))
}

/// The address's ARCADE here: confirmed and waiting, not spent by a waiting transaction.
fn balance(node: &Node, p: &ChainParams, addr: &str) -> Option<u64> {
    let v = crate::sitesvc::coins(node, &json!({"chain": p.key, "address": addr})).ok()?;
    v["addresses"][addr]["total"].as_u64()
}

fn day_key(prefix: &str, now: u64) -> Vec<u8> {
    format!("{prefix}:{}", now / 86_400).into_bytes()
}

fn read_list(node: &Node, key: &[u8]) -> Vec<Value> {
    node.app.store.read(|r| r.get(arcade_app::META, key)).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or_default()
}

fn write_list(node: &Node, key: &[u8], list: &[Value]) {
    let _ = node.app.store.write(|w| w.put(arcade_app::META, key, &serde_json::to_vec(list).unwrap()));
}

fn frame(rec: Value) -> Value {
    json!({"t": "af", "recs": [rec]})
}

/// Ask the mesh to top `addr` up to 0.05 ARCADE: once per address and UTC day, and only when it
/// has less than that (as far as this arcade can see). This arcade may pay its own ask too.
pub fn ask(node: &Node, addr: &str) -> Result<Value, String> {
    let addr = addr.trim();
    let now = arcade_app::now();
    // ARCADE stalled: the user's writes go to Pepecoin testnet meanwhile, so the same key's address there gets test PEP.
    if crate::arcsvc::stalled() {
        if let Some(pep) = crate::hometag::address_on(node.params, addr) {
            let _ = crate::faucet::ask_if_needed(node, &pep, "");
        }
    }
    // Without the chain here the address is checked by its form only (the payer checks again).
    let p = crate::arcsvc::defined().ok_or("this arcade doesn't know the ARCADE chain")?;
    script_of(p, addr)?;
    if let Ok(p) = chain() {
        if let Some(have) = balance(node, p, addr).filter(|h| *h + arcade_arc::params::DUST > TARGET) {
            return Ok(json!({"asked": false, "address": addr, "balance": have, "why": "it has 0.05 ARCADE already"}));
        }
    }
    let key = day_key("arc_faucet_asked", now);
    let mut asked = read_list(node, &key);
    if asked.iter().any(|a| a == addr) {
        return Ok(json!({"asked": false, "address": addr, "why": "asked for it today already"}));
    }
    let rec = arcade_mesh::welcome::sign(json!({"a": node.id_hex(), "addr": addr, "at": now, "k": "ask"}), &node.identity);
    state().lock().unwrap().asks.insert(addr.to_string(), rec.clone());
    node.mesh_outbox.lock().unwrap().push(frame(rec));
    asked.push(json!(addr));
    write_list(node, &key, &asked);
    let _ = node.app.event("faucet", &format!("asked the mesh to top {addr} up to 0.05 ARCADE"));
    Ok(json!({"asked": true, "address": addr, "target": TARGET}))
}

/// The nodes paid a node gift, loaded from the store on first use.
fn paid_nodes<'a>(node: &Node, st: &'a mut State) -> &'a mut HashMap<String, (String, u64)> {
    st.paid_nodes.get_or_insert_with(|| {
        read_list(node, b"arc_node_paid").iter().filter_map(|e| Some((e[0].as_str()?.to_string(), (e[1].as_str()?.to_string(), e[2].as_u64()?)))).collect()
    })
}

/// Note node `id` paid at `addr` (by this arcade, or by a notice), in memory and in the store.
fn note_paid_node(node: &Node, id: &str, addr: &str, at: u64) {
    let list = {
        let mut st = state().lock().unwrap();
        let paid = paid_nodes(node, &mut st);
        if paid.contains_key(id) || paid.len() >= MAX_NODES {
            return;
        }
        paid.insert(id.to_string(), (addr.to_string(), at));
        paid.iter().map(|(n, (a, t))| json!([n, a, t])).collect::<Vec<_>>()
    };
    write_list(node, b"arc_node_paid", &list);
}

/// The node faucet, asking side (every 15 seconds; it acts every NODE_ASK_EVERY): this arcade's wallet address (its
/// first ARCADE key's) asks for its node gift, the same signed ask said again, until the address is past it.
pub fn node_gift_ask(node: &Node) {
    let now = arcade_app::now();
    {
        let mut st = state().lock().unwrap();
        if st.node_done || now.saturating_sub(st.node_asked) < NODE_ASK_EVERY {
            return;
        }
        st.node_asked = now;
    }
    if !node.app.has_wallet().unwrap_or(false) || node.links.lock().unwrap().is_empty() {
        state().lock().unwrap().node_asked = 0;
        return;
    }
    // ARCADE stalled (or this copy still syncing): writes go to Pepecoin testnet, so test PEP pays for them meanwhile
    // (the welcome faucet, once per address while needed).
    if crate::arcsvc::stalled() {
        if let Ok((k, _)) = node.app.tag_key(node.params, 0) {
            let _ = crate::faucet::ask_if_needed(node, &k.address, "");
        }
    }
    let Some(p) = crate::arcsvc::defined() else { return };
    let Ok((k, _)) = node.app.tag_key(p, 0) else { return };
    // Past it (holding or having received its gift, on this copy or a lender's): done for good.
    let past = match chain() {
        Ok(c) => held_before(node, c, &k.address),
        Err(_) => crate::sitesvc::coins(node, &json!({"chain": p.key, "address": k.address})).ok().and_then(|v| v["addresses"][&k.address]["total"].as_u64()).is_some_and(|t| t >= NODE_GIFT / 2),
    };
    if past {
        state().lock().unwrap().node_done = true;
        return;
    }
    let rec = {
        let mut st = state().lock().unwrap();
        // A new signed ask when there is none for this address, or it is half way to aging out.
        let keep = st.own_ask.as_ref().is_some_and(|r| r["addr"] == k.address.as_str() && r["at"].as_u64().is_some_and(|t| now.saturating_sub(t) < ASK_SECS / 2));
        if !keep {
            let first = st.own_ask.is_none();
            st.own_ask = Some(arcade_mesh::welcome::sign(json!({"a": node.id_hex(), "addr": k.address, "at": now, "k": "node"}), &node.identity));
            if first {
                let _ = node.app.event("faucet", &format!("asked the mesh for this arcade's 1 ARCADE (node faucet) at {}", k.address));
            }
        }
        let rec = st.own_ask.clone().unwrap();
        st.nodes.insert(node.id_hex(), rec.clone());
        rec
    };
    node.mesh_outbox.lock().unwrap().push(frame(rec));
}

/// Whether `addr` is past its node gift on this arcade's copy: it received more than top-ups would bring (TARGET in
/// all), or holds half a gift now (the pool counted). An earlier top-up doesn't use the gift up.
fn held_before(node: &Node, p: &ChainParams, addr: &str) -> bool {
    let Ok(script) = script_of(p, addr) else { return true };
    let Ok(hist) = node.chain.history_for_script(p.chain_no, &script) else { return true };
    let Some(have) = balance(node, p, addr) else { return true };
    past_gift(hist.iter().map(|(_, _, recv, _)| *recv).sum(), have)
}

/// [`held_before`] from what the address received in all and holds now.
fn past_gift(received: u64, have: u64) -> bool {
    received > TARGET || have >= NODE_GIFT / 2
}

/// An amount as the faucets pay it: in two coins (half each, the odd unit in the first), or one when half would be
/// below the chain's smallest output.
fn two_coins(amount: u64) -> Vec<u64> {
    let half = amount / 2;
    if half >= arcade_arc::params::DUST {
        vec![amount - half, half]
    } else {
        vec![amount]
    }
}

fn fresh(at: u64, now: u64) -> bool {
    at <= now + SKEW_SECS && now.saturating_sub(at) < ASK_SECS
}

/// An `af` frame arrived: keep asks and payment notices; pass each new one on once.
pub fn on_frame(node: &Node, head: &Value) {
    let now = arcade_app::now();
    let Some(recs) = head["recs"].as_array() else { return };
    for r in recs {
        if !arcade_mesh::welcome::verify(r) {
            continue;
        }
        let (Some(addr), Some(at)) = (r["addr"].as_str(), r["at"].as_u64()) else { continue };
        if !fresh(at, now) || addr.len() > 64 {
            continue;
        }
        // Only an ARCADE address is kept and passed on.
        if crate::arcsvc::defined().is_some_and(|p| script_of(p, addr).is_err()) {
            continue;
        }
        let new = {
            let mut st = state().lock().unwrap();
            match r["k"].as_str() {
                Some("ask") => {
                    if !st.asks.contains_key(addr) && st.asks.len() >= MAX_ADDRS {
                        continue;
                    }
                    let newer = st.asks.get(addr).map(|o| o["at"].as_u64() < Some(at)).unwrap_or(true);
                    if newer {
                        st.asks.insert(addr.to_string(), r.clone());
                    }
                    newer
                }
                Some("node") => {
                    let Some(id) = r["a"].as_str() else { continue };
                    if !st.nodes.contains_key(id) && st.nodes.len() >= MAX_ADDRS {
                        continue;
                    }
                    // The same signed ask said again is passed on once only.
                    let newer = st.nodes.get(id).map(|o| o["at"].as_u64() < Some(at) || o["addr"] != r["addr"]).unwrap_or(true);
                    if newer {
                        st.nodes.insert(id.to_string(), r.clone());
                        drop(st);
                        note_welcome(node, id, addr, at);
                    }
                    newer
                }
                Some("sent") => {
                    let Some(txid) = r["txid"].as_str() else { continue };
                    if !st.sent.contains_key(addr) && st.sent.len() >= MAX_ADDRS {
                        continue;
                    }
                    let list = st.sent.entry(addr.to_string()).or_default();
                    let new = !list.iter().any(|(t, _)| t == txid);
                    if new {
                        list.push((txid.to_string(), at));
                        if list.len() > 8 {
                            list.remove(0);
                        }
                    }
                    if new {
                        if let Some(id) = r["node"].as_str() {
                            drop(st);
                            note_paid_node(node, id, addr, at);
                        }
                    }
                    new
                }
                _ => false,
            }
        };
        if new {
            node.mesh_outbox.lock().unwrap().push(frame(r.clone()));
        }
    }
}

/// How long after an ask this arcade (`me`) waits before paying it: fixed per pair.
fn delay(me: &str, addr: &str) -> u64 {
    let h = Sha256::digest(format!("{me}:{addr}").as_bytes());
    u64::from_le_bytes(h[..8].try_into().unwrap()) % SPREAD_SECS
}

/// What to send an address that has `have`: what is missing to TARGET, or nothing when that is
/// less than the chain's smallest output.
fn top_up(have: u64) -> Option<Vec<u64>> {
    let need = TARGET.saturating_sub(have);
    (need >= arcade_arc::params::DUST).then(|| two_coins(need))
}

/// The most this wallet can spend from one key on ARCADE (a send pays from one key).
fn spendable(node: &Node, p: &ChainParams) -> u64 {
    let mut keys = node.app.tag_keys(p.chain_no).unwrap_or_default();
    if keys.is_empty() {
        if let Ok((k, _)) = node.app.tag_key(p, 0) {
            keys.push(k);
        }
    }
    keys.iter()
        .filter_map(|k| arcade_ledger::address::p2pkh_script_for_address(p, &k.address))
        .map(|s| crate::claims::arc_coins(node, p, &s).iter().map(|u| u.value).sum::<u64>())
        .max()
        .unwrap_or(0)
}

/// Pay one open ask from this arcade's wallet, if it can (called every 15 seconds).
pub fn pay(node: &Node) {
    let Ok(p) = chain() else { return };
    // Only from its own copy, caught up: the balance it reads must be the chain's.
    if crate::arcsvc::lends().is_none() || !node.app.has_wallet().unwrap_or(false) {
        return;
    }
    resolve_welcomes(node, p);
    if pay_node_gift(node, p) {
        return; // one payment per pass
    }
    let now = arcade_app::now();
    let me = node.id_hex();
    let key = day_key("arc_faucet_paid", now);
    let paid = read_list(node, &key); // [[addr, asker], ...]
    if paid.len() >= DAILY_CAP {
        return;
    }
    let candidates: Vec<(String, String, u64)> = {
        let mut st = state().lock().unwrap();
        st.asks.retain(|_, r| r["at"].as_u64().is_some_and(|t| fresh(t, now)));
        st.done.retain(|_, t| now.saturating_sub(*t) < ASK_SECS);
        st.sent.retain(|_, l| {
            l.retain(|(_, at)| now.saturating_sub(*at) < ASK_SECS);
            !l.is_empty()
        });
        st.asks
            .iter()
            .filter_map(|(addr, r)| {
                let at = r["at"].as_u64()?;
                let asker = r["a"].as_str()?.to_string();
                // Done since this ask, or a payment announced lately.
                if st.done.get(addr).is_some_and(|t| *t >= at) {
                    return None;
                }
                if st.sent.get(addr).is_some_and(|l| l.iter().any(|(_, t)| *t + SKEW_SECS >= at && now.saturating_sub(*t) < SENT_HOLD)) {
                    return None;
                }
                (now >= at + delay(&me, addr)).then_some((addr.clone(), asker, at))
            })
            .collect()
    };
    let have_spare = spendable(node, p).saturating_sub(KEEP + FEE_MARGIN);
    for (addr, asker, _) in candidates {
        let by_asker = paid.iter().filter(|e| e[1] == asker.as_str()).count();
        if by_asker >= PER_ASKER || paid.iter().any(|e| e[0] == addr.as_str()) {
            continue;
        }
        let Some(have) = balance(node, p, &addr) else { continue };
        let Some(parts) = top_up(have) else {
            state().lock().unwrap().done.insert(addr, now);
            continue;
        };
        let amount: u64 = parts.iter().sum();
        if amount > have_spare {
            return; // nothing to spare: someone else pays
        }
        let r = match crate::coinsend::send_parts(node, Some(p.key), &addr, &parts, None, None, false, true) {
            Ok(r) => r,
            Err(e) => {
                let _ = node.app.event("faucet", &format!("could not top {addr} up with ARCADE: {e}"));
                return;
            }
        };
        let txid = r["broadcast"]["txid"].as_str().or(r["txid"].as_str()).unwrap_or("").to_string();
        let sent = arcade_mesh::welcome::sign(json!({"a": me, "addr": addr, "at": now, "k": "sent", "txid": txid}), &node.identity);
        node.mesh_outbox.lock().unwrap().push(frame(sent));
        {
            let mut st = state().lock().unwrap();
            st.done.insert(addr.clone(), now);
            st.sent.entry(addr.clone()).or_default().push((txid.clone(), now));
        }
        let mut list = paid;
        list.push(json!([addr, asker]));
        write_list(node, &key, &list);
        let _ = node.app.event("faucet", &format!("topped {addr} up with {} ARCADE in {txid}", amount as f64 / 1e8));
        return; // one per pass: the next pass sees the updated coins
    }
}

/// The node faucet, paying side: one open node-gift ask from this arcade's wallet, if it can. NODE_GIFT in two coins
/// to a node no payer has paid, at an address past no gift. Whether it paid.
/// Why a node-gift ask wasn't paid here, said once an hour per address (2026-11-21: a new arcade's ask sat unpaid on an
/// arcade with ARCADE to spare and nothing said why).
fn skipped(node: &Node, id: &str, addr: &str, why: &str) {
    static SAID: std::sync::Mutex<Option<std::collections::HashMap<String, u64>>> = std::sync::Mutex::new(None);
    let now = arcade_app::now();
    let mut m = SAID.lock().unwrap();
    let m = m.get_or_insert_with(Default::default);
    if m.get(addr).is_some_and(|t| now.saturating_sub(*t) < 3600) {
        return;
    }
    m.insert(addr.to_string(), now);
    let _ = node.app.event("faucet", &format!("node {}'s gift at {addr} not paid here: {why}", &id[..id.len().min(16)]));
}

/// The open node-gift asks as this arcade knows them: (node, address, asked at, paid as a notice said), without this
/// arcade's own, addresses found past their gift, and addresses with a payment notice lately (their coins decide once
/// they arrive). The chain checks are [`gift_open`]'s.
fn open_asks(node: &Node, now: u64) -> Vec<(String, String, u64, Option<(String, u64)>)> {
    let me = node.id_hex();
    let mut st = state().lock().unwrap();
    st.nodes.retain(|_, r| r["at"].as_u64().is_some_and(|t| fresh(t, now)));
    let gifted = st.gifted.clone();
    let sent = st.sent.clone();
    let asks: Vec<(String, Value)> = st.nodes.iter().map(|(id, r)| (id.clone(), r.clone())).collect();
    let paid = paid_nodes(node, &mut st).clone();
    asks.into_iter()
        .filter(|(id, _)| *id != me)
        .filter_map(|(id, r)| {
            let addr = r["addr"].as_str()?.to_string();
            let at = r["at"].as_u64()?;
            if gifted.contains(&addr) || sent.get(&addr).is_some_and(|l| l.iter().any(|(_, t)| now.saturating_sub(*t) < SENT_HOLD)) {
                return None;
            }
            Some((id.clone(), addr, at, paid.get(&id).cloned()))
        })
        .collect()
}

/// Whether node `id`'s gift at `addr` is still open on this arcade's copy of the chain, or why not. The same checks for
/// the automatic payer, a "Welcome to the arcade" press and the new-arcade notice, so the notice goes away for every
/// arcade the moment one of them would no longer pay it.
fn gift_open(node: &Node, p: &ChainParams, addr: &str, was_paid: Option<(String, u64)>, now: u64) -> Result<(), String> {
    // Paid before (by us or as a notice said): so it stays, unless the notice's payment never came (its address
    // received no gift on this copy after the hold; a notice is only its sender's word).
    if let Some((paid_addr, at)) = was_paid {
        let came = script_of(p, &paid_addr).ok().and_then(|sc| node.chain.history_for_script(p.chain_no, &sc).ok()).is_some_and(|h| h.iter().map(|x| x.2).sum::<u64>() > TARGET)
            || balance(node, p, &paid_addr).is_some_and(|b| b >= NODE_GIFT / 2);
        if came || now.saturating_sub(at) < SENT_HOLD {
            return Err(if came { "another arcade paid it (its coins arrived)" } else { "another arcade says it paid it; waiting for the coins" }.into());
        }
    }
    if held_before(node, p, addr) {
        state().lock().unwrap().gifted.insert(addr.to_string());
        return Err("the address holds or received ARCADE already".into());
    }
    Ok(())
}

/// One welcome sent at a time from this arcade (the automatic payer and a "Welcome to the arcade" press).
static SENDING: Mutex<()> = Mutex::new(());

/// Send node `id` its welcome at `addr`: `amount` in two coins, the signed payment notice to the mesh (it clears the
/// new-arcade notice on every arcade it reaches), noted as paid and in today's list. The txid.
fn send_gift(node: &Node, p: &ChainParams, id: &str, addr: &str, amount: u64, how: &str) -> Result<String, String> {
    let now = arcade_app::now();
    let me = node.id_hex();
    let r = crate::coinsend::send_parts(node, Some(p.key), addr, &two_coins(amount), None, None, false, true)
        .map_err(|e| format!("could not give node {} its ARCADE at {addr}: {e}", &id[..id.len().min(16)]))?;
    let txid = r["broadcast"]["txid"].as_str().or(r["txid"].as_str()).unwrap_or("").to_string();
    let sent = arcade_mesh::welcome::sign(json!({"a": me, "addr": addr, "at": now, "k": "sent", "txid": txid, "node": id}), &node.identity);
    node.mesh_outbox.lock().unwrap().push(frame(sent));
    {
        let mut st = state().lock().unwrap();
        st.gifted.insert(addr.to_string());
        st.sent.entry(addr.to_string()).or_default().push((txid.clone(), now));
    }
    note_paid_node(node, id, addr, now);
    note_welcome(node, id, addr, now);
    let key = day_key("arc_node_paid_day", now);
    let mut list = read_list(node, &key);
    list.push(json!([addr, id]));
    write_list(node, &key, &list);
    let _ = node.app.event("faucet", &format!("{how} node {} with {} ARCADE (two coins) at {addr} in {txid}", &id[..id.len().min(16)], amount as f64 / 1e8));
    Ok(txid)
}

/// The node faucet, paying side: one open node-gift ask from this arcade's wallet, if its operator leaves automatic
/// welcomes on (Options) and it can. The operator's amount (1 ARCADE unless set) in two coins to a node no payer has
/// paid, at an address past no gift, after people's head start. Whether it paid.
fn pay_node_gift(node: &Node, p: &ChainParams) -> bool {
    if !auto_on(node) {
        return false;
    }
    let now = arcade_app::now();
    let me = node.id_hex();
    let paid_today = read_list(node, &day_key("arc_node_paid_day", now)); // [[addr, node], ...]
    if paid_today.len() >= NODE_DAILY_CAP {
        return false;
    }
    let amount = welcome_amount(node);
    let candidates: Vec<_> = open_asks(node, now).into_iter().filter(|(_, addr, at, _)| now >= at + HEAD_START_SECS + delay(&me, addr)).collect();
    let have_spare = spendable(node, p).saturating_sub(KEEP + FEE_MARGIN);
    for (id, addr, _, was_paid) in candidates {
        if paid_today.iter().any(|e| e[0] == addr.as_str() || e[1] == id.as_str()) {
            skipped(node, &id, &addr, "paid today already");
            continue;
        }
        if let Err(why) = gift_open(node, p, &addr, was_paid, now) {
            skipped(node, &id, &addr, &why);
            continue;
        }
        if amount > have_spare {
            skipped(node, &id, &addr, &format!("this wallet has {} ARCADE to spare, less than the welcome", have_spare as f64 / 1e8));
            return false; // nothing to spare: someone else pays
        }
        let _one = SENDING.lock().unwrap();
        // A press may have welcomed it while this pass looked.
        if state().lock().unwrap().gifted.contains(&addr) {
            continue;
        }
        return match send_gift(node, p, &id, &addr, amount, "welcomed (automatically)") {
            Ok(_) => true,
            Err(e) => {
                let _ = node.app.event("faucet", &e);
                false
            }
        };
    }
    false
}

// ---------------------------------------------------------------- welcomes: the notice, the button, the high scores
//
// A new arcade asks the mesh for its node gift (the `node` record above). Every arcade lists the open asks as a
// "new arcade" notice with a "Welcome to the arcade" button; the first arcade whose payment lands welcomes it. The
// notice goes away on every arcade once one of them would no longer pay (a payment notice heard, or the coins seen).
// Automatic welcomes (on unless the operator turns them off in Options, with the amount the operator sets) wait
// HEAD_START_SECS first, so a person pressing the button can be first. The welcomers' high scores count, per wallet
// address, the welcomes whose payment came first on the chain: read from the chain alone (the earliest payment of a
// welcome's size to the new arcade's address, and the address its first input spends), so a payment notice can't
// claim another arcade's welcome.

/// People's head start: automatic welcomes wait this long after a new arcade's ask (plus their spread).
const HEAD_START_SECS: u64 = 180;
/// The welcome amounts an operator may set: 1 to 100 ARCADE.
const WELCOME_MIN: u64 = NODE_GIFT;
const WELCOME_MAX: u64 = 100 * NODE_GIFT;
/// Welcomes kept for the high scores, and how many are looked up on the chain per pass.
const MAX_WELCOMES: usize = 20_000;
const RESOLVE_PER_PASS: usize = 3;
/// A welcome not found on the chain this long after the ask is given up on.
const RESOLVE_SECS: u64 = 7 * 86_400;

fn welcome_cfg(node: &Node) -> Value {
    node.app.store.read(|r| r.get(arcade_app::META, b"arc_welcome")).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or(json!({}))
}

/// Whether this arcade welcomes new arcades by itself (on unless its operator turned it off).
pub fn auto_on(node: &Node) -> bool {
    welcome_cfg(node)["auto"].as_bool().unwrap_or(true)
}

/// What this arcade gives a new arcade (1 ARCADE unless its operator set more).
pub fn welcome_amount(node: &Node) -> u64 {
    welcome_cfg(node)["amount"].as_u64().filter(|a| (WELCOME_MIN..=WELCOME_MAX).contains(a)).unwrap_or(NODE_GIFT)
}

/// A welcome amount as the API gives it (satoshis: an integer, or a string of one), or why it can't be one.
fn amount_of(v: &Value) -> Result<u64, String> {
    let u = v.as_u64().or_else(|| v.as_str().and_then(|s| s.trim().parse().ok())).ok_or("amount: satoshis of ARCADE (100000000 is 1 ARCADE)")?;
    if !(WELCOME_MIN..=WELCOME_MAX).contains(&u) {
        return Err(format!("amount: {} to {} ARCADE ({WELCOME_MIN} to {WELCOME_MAX} satoshis)", WELCOME_MIN / 100_000_000, WELCOME_MAX / 100_000_000));
    }
    Ok(u)
}

/// `faucet.welcome_set {auto?, amount?}` (Admin): automatic welcomes on or off, and the amount (satoshis).
pub fn welcome_set(node: &Node, req: &Value) -> Result<Value, String> {
    let mut c = welcome_cfg(node);
    if let Some(on) = req["auto"].as_bool() {
        c["auto"] = json!(on);
    }
    if let Some(a) = req.get("amount").filter(|v| !v.is_null()) {
        c["amount"] = json!(amount_of(a)?);
    }
    node.app.store.write(|w| w.put(arcade_app::META, b"arc_welcome", &serde_json::to_vec(&c).unwrap())).map_err(|e| e.to_string())?;
    let (auto, amount) = (auto_on(node), welcome_amount(node));
    let _ = node.app.event("faucet", &format!("welcomes: automatic {}, {} ARCADE each", if auto { "on" } else { "off" }, amount as f64 / 1e8));
    Ok(json!({"auto": auto, "amount": amount}))
}

/// `faucet.welcome {node, amount?}` (Wallet): welcome a new arcade now, from this wallet ("Welcome to the arcade").
/// Refused once it is welcomed (by anyone), so a second press, here or on another arcade, sends nothing.
pub fn welcome(node: &Node, id: &str, amount: Option<&Value>) -> Result<Value, String> {
    let p = chain().map_err(|_| "this arcade doesn't run the ARCADE chain: welcomes are sent from an arcade that does".to_string())?;
    if crate::arcsvc::lends().is_none() {
        return Err("this arcade's ARCADE copy is still catching up: welcomes are sent once it has".into());
    }
    if !node.app.has_wallet().unwrap_or(false) {
        return Err("make a wallet first".into());
    }
    let amount = match amount.filter(|v| !v.is_null()) {
        Some(v) => amount_of(v)?,
        None => welcome_amount(node),
    };
    let now = arcade_app::now();
    let _one = SENDING.lock().unwrap();
    let Some((_, addr, _, was_paid)) = open_asks(node, now).into_iter().find(|(n, ..)| n == id) else {
        return Err("that arcade is welcomed already (or its ask is gone)".into());
    };
    gift_open(node, p, &addr, was_paid, now).map_err(|why| format!("not sent: {why}"))?;
    let spare = spendable(node, p).saturating_sub(KEEP + FEE_MARGIN);
    if amount > spare {
        return Err(format!("this wallet has {} ARCADE to spare (it keeps {} for itself)", spare as f64 / 1e8, KEEP as f64 / 1e8));
    }
    let txid = send_gift(node, p, id, &addr, amount, "welcomed")?;
    Ok(json!({"txid": txid, "node": id, "address": addr, "amount": amount}))
}

fn welcomes(node: &Node) -> serde_json::Map<String, Value> {
    node.app.store.read(|r| r.get(arcade_app::META, b"arc_welcomes")).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or_default()
}

fn save_welcomes(node: &Node, w: &serde_json::Map<String, Value>) {
    let _ = node.app.store.write(|x| x.put(arcade_app::META, b"arc_welcomes", &serde_json::to_vec(w).unwrap()));
}

/// Note a new arcade (node `id`, its wallet's `addr`) for the high scores, once.
fn note_welcome(node: &Node, id: &str, addr: &str, at: u64) {
    let mut w = welcomes(node);
    if w.get(id).is_some_and(|e| e["addr"] == addr) || w.len() >= MAX_WELCOMES {
        return;
    }
    w.insert(id.to_string(), json!({"addr": addr, "at": at, "winner": Value::Null}));
    save_welcomes(node, &w);
}

/// Who welcomed the arcade at `addr` first, on this copy of the chain: the earliest payment of a welcome's size
/// (1 ARCADE or more in one transaction; its block, then its place in the block), as (txid, height, the address its
/// first input spends, that address's @tag). None while no such payment is in a block.
fn first_welcome(node: &Node, p: &'static ChainParams, addr: &str) -> Option<(String, u32, String, Option<String>)> {
    let hist = node.chain.history_for_script(p.chain_no, &script_of(p, addr).ok()?).ok()?;
    let h = hist.iter().filter(|x| x.2 >= WELCOME_MIN).map(|x| x.0).min()?;
    let mut best: Option<(u32, arcade_chain::Hash256)> = None;
    for (_, t12, _, _) in hist.iter().filter(|x| x.0 == h && x.2 >= WELCOME_MIN) {
        for txid in node.chain.txids_by_t12(p, t12).ok()? {
            if let Some((bh, idx)) = node.chain.tx_location(p, &txid).ok().flatten() {
                if bh == h && best.as_ref().map_or(true, |(i, _)| idx < *i) {
                    best = Some((idx, txid));
                }
            }
        }
    }
    let txid = best?.1.to_hex();
    let tx = crate::explorer::handle(node, "explorer.tx", &json!({"chain": p.key, "txid": txid})).ok()?;
    let first = &tx["inputs"].as_array()?.first()?;
    let from = first["address"].as_str()?.to_string();
    let tag = first["tag"].as_str().map(String::from);
    Some((txid, h, from, tag))
}

/// Look up who welcomed the new arcades not looked up yet (a few per pass, from this arcade's own caught-up copy).
fn resolve_welcomes(node: &Node, p: &'static ChainParams) {
    // When each open welcome was last looked up (in memory: the least lately looked-up go first, so a few that are
    // never paid don't hold the others back; only an answer is written to the store).
    static LOOKED: Mutex<Option<HashMap<String, u64>>> = Mutex::new(None);
    let now = arcade_app::now();
    let mut w = welcomes(node);
    let open: Vec<(String, String)> = {
        let mut looked = LOOKED.lock().unwrap();
        let looked = looked.get_or_insert_with(HashMap::new);
        let mut open: Vec<(u64, String, String)> = w
            .iter()
            .filter(|(_, e)| e["winner"].is_null() && e["gave_up"].is_null())
            .filter_map(|(id, e)| Some((looked.get(id).copied().unwrap_or(0), id.clone(), e["addr"].as_str()?.to_string())))
            .filter(|(t, ..)| now.saturating_sub(*t) >= 60)
            .collect();
        open.sort();
        open.truncate(RESOLVE_PER_PASS);
        for (_, id, _) in &open {
            looked.insert(id.clone(), now);
        }
        open.into_iter().map(|(_, id, a)| (id, a)).collect()
    };
    let mut changed = false;
    for (id, addr) in open {
        let e = w.get_mut(&id).unwrap();
        match first_welcome(node, p, &addr) {
            Some((txid, height, from, tag)) => e["winner"] = json!({"address": from, "tag": tag, "txid": txid, "height": height}),
            None if now.saturating_sub(e["at"].as_u64().unwrap_or(0)) > RESOLVE_SECS => e["gave_up"] = json!(true),
            None => continue,
        }
        changed = true;
    }
    if changed {
        save_welcomes(node, &w);
    }
}

/// The welcomers' high scores: per wallet address, the new arcades it welcomed first (with its @tag where known),
/// most first; and the latest welcomes.
pub fn scores(node: &Node) -> Value {
    let w = welcomes(node);
    let mut per: HashMap<String, (u64, Option<String>, u64)> = HashMap::new();
    let mut recent: Vec<Value> = Vec::new();
    for (id, e) in &w {
        let Some(win) = e["winner"].as_object() else { continue };
        let Some(a) = win["address"].as_str() else { continue };
        let s = per.entry(a.to_string()).or_insert((0, None, 0));
        s.0 += 1;
        if let Some(t) = win["tag"].as_str() {
            s.1 = Some(t.to_string());
        }
        let h = win["height"].as_u64().unwrap_or(0);
        s.2 = s.2.max(h);
        recent.push(json!({"node": id, "name": crate::meshview::name_of_peer(id), "by": a, "tag": win["tag"], "txid": win["txid"], "height": h}));
    }
    let mut top: Vec<(String, (u64, Option<String>, u64))> = per.into_iter().collect();
    // Most welcomes first; a tie goes to the one that reached it first (the lower latest height).
    top.sort_by(|a, b| b.1 .0.cmp(&a.1 .0).then(a.1 .2.cmp(&b.1 .2)).then(a.0.cmp(&b.0)));
    recent.sort_by(|a, b| b["height"].as_u64().cmp(&a["height"].as_u64()));
    recent.truncate(10);
    json!({"top": top.iter().take(20).map(|(a, (n, t, _))| json!({"address": a, "tag": t, "welcomes": n})).collect::<Vec<_>>(),
           "recent": recent, "counted": w.values().filter(|e| e["winner"].is_object()).count()})
}

/// `faucet.newcomers` (Read): the new arcades waiting for their welcome (for the Home notice and its button), this
/// arcade's welcome settings, whether it can send one now, and the welcomers' high scores.
pub fn newcomers(node: &Node) -> Value {
    let now = arcade_app::now();
    let mut open = Vec::new();
    if let Ok(p) = chain() {
        for (id, addr, at, was_paid) in open_asks(node, now) {
            if gift_open(node, p, &addr, was_paid, now).is_ok() {
                open.push(json!({"node": id, "name": crate::meshview::name_of_peer(&id), "address": addr, "at": at,
                                 "auto_after": at + HEAD_START_SECS}));
            }
        }
    }
    open.sort_by(|a, b| b["at"].as_u64().cmp(&a["at"].as_u64()));
    let can = chain().is_ok() && crate::arcsvc::lends().is_some() && node.app.has_wallet().unwrap_or(false);
    json!({"open": open, "auto": auto_on(node), "amount": welcome_amount(node), "head_start": HEAD_START_SECS,
           "min": WELCOME_MIN, "max": WELCOME_MAX, "can_send": can, "scores": scores(node)})
}

/// For the Home tab: `addr`'s top-up as the welcome faucet's is shown ({status: "paid", txid, by} once a payment notice
/// was seen, {status: "asked", at} while asked), or None.
pub fn of_address(addr: &str) -> Option<Value> {
    let st = state().lock().unwrap();
    if let Some((txid, at)) = st.sent.get(addr).and_then(|l| l.last()) {
        return Some(json!({"status": "paid", "at": at, "txid": txid, "by": Value::Null}));
    }
    st.asks.get(addr).map(|r| json!({"status": "asked", "at": r["at"], "txid": Value::Null, "by": r["a"]}))
}

/// `faucet.arc_status`: the asks this arcade knows and what it paid today.
pub fn status(node: &Node) -> Value {
    let now = arcade_app::now();
    let st = state().lock().unwrap();
    let asks: Vec<Value> = st.asks.iter().map(|(a, r)| json!({"address": a, "asker": r["a"], "at": r["at"], "done": st.done.contains_key(a), "notices": st.sent.get(a).map(|l| l.len()).unwrap_or(0)})).collect();
    let paid = read_list(node, &day_key("arc_faucet_paid", now));
    let askers: HashSet<&str> = paid.iter().filter_map(|e| e[1].as_str()).collect();
    let node_asks: Vec<Value> = st.nodes.iter().map(|(id, r)| json!({"node": id, "address": r["addr"], "at": r["at"], "gifted": r["addr"].as_str().is_some_and(|a| st.gifted.contains(a))})).collect();
    let node_gifts_today = read_list(node, &day_key("arc_node_paid_day", now)).len();
    let own = json!({"asking": st.own_ask.as_ref().map(|r| r["addr"].clone()), "done": st.node_done});
    drop(st);
    let paid_known = paid_nodes(node, &mut state().lock().unwrap()).len();
    json!({"node_faucet": {"gift": NODE_GIFT, "daily_cap": NODE_DAILY_CAP, "asks": node_asks, "paid_today": node_gifts_today, "nodes_paid_known": paid_known, "this_arcade": own},
           "target": TARGET, "daily_cap": DAILY_CAP, "per_asker": PER_ASKER, "keep": KEEP,
           "running": chain().is_ok(), "caught_up": crate::arcsvc::lends().is_some(),
           "asks": asks, "paid_today": paid.len(), "askers_paid_today": askers.len()})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tops_up_to_the_target() {
        let sum = |v: Option<Vec<u64>>| v.map(|v| v.iter().sum::<u64>());
        assert_eq!(top_up(0), Some(vec![TARGET / 2, TARGET / 2]));
        assert_eq!(sum(top_up(400_000)), Some(TARGET - 400_000));
        assert_eq!(top_up(TARGET), None);
        assert_eq!(top_up(TARGET * 3), None);
        // Short by less than the smallest output: nothing to send.
        assert_eq!(top_up(TARGET - arcade_arc::params::DUST + 1), None);
        assert_eq!(top_up(TARGET - arcade_arc::params::DUST), Some(vec![arcade_arc::params::DUST]));
    }

    #[test]
    fn a_top_up_doesnt_use_up_the_node_gift() {
        assert!(!past_gift(0, 0), "new");
        assert!(!past_gift(TARGET, TARGET), "a top-up came before");
        assert!(!past_gift(TARGET, 1_000_000), "and was spent");
        assert!(past_gift(TARGET + NODE_GIFT, 3_000_000), "gifted, then spent");
        assert!(past_gift(0, NODE_GIFT), "the gift waits in the pool");
    }

    #[test]
    fn faucets_pay_in_two_coins() {
        assert_eq!(two_coins(NODE_GIFT), vec![50_000_000, 50_000_000]);
        assert_eq!(two_coins(TARGET), vec![2_500_000, 2_500_000]);
        assert_eq!(two_coins(3), vec![3], "half below dust: one coin");
        let odd = two_coins(4_000_001);
        assert_eq!((odd.iter().sum::<u64>(), odd.len()), (4_000_001, 2));
    }

    #[test]
    fn asks_age_out() {
        assert!(fresh(1000, 1000));
        assert!(fresh(1000 + SKEW_SECS, 1000), "a little ahead of our clock");
        assert!(!fresh(1000 + SKEW_SECS + 1, 1000));
        assert!(!fresh(1000, 1000 + ASK_SECS));
    }

    #[test]
    fn payers_wait_differently() {
        let d: HashSet<u64> = (0..20).map(|i| delay(&format!("payer{i}"), "aXeV2c4YTSXLHVHRP5EdfYqeRX9oU4et2e")).collect();
        assert!(d.iter().all(|x| *x < SPREAD_SECS));
        assert!(d.len() > 5, "spread over the window");
    }

    #[test]
    fn records_verify() {
        let k = ed25519_dalek::SigningKey::from_bytes(&[5; 32]);
        let rec = arcade_mesh::welcome::sign(json!({"a": hex::encode(k.verifying_key().to_bytes()), "addr": "aX", "at": 1, "k": "ask"}), &k);
        assert!(arcade_mesh::welcome::verify(&rec));
        let mut bad = rec.clone();
        bad["addr"] = json!("aY");
        assert!(!arcade_mesh::welcome::verify(&bad));
    }
    #[test]
    fn welcome_amounts_are_1_to_100_arcade() {
        assert_eq!(amount_of(&json!(NODE_GIFT)).unwrap(), NODE_GIFT);
        assert_eq!(amount_of(&json!("250000000")).unwrap(), 250_000_000);
        assert_eq!(amount_of(&json!(100 * NODE_GIFT)).unwrap(), 100 * NODE_GIFT);
        for bad in [json!(NODE_GIFT / 2), json!(0), json!(101 * NODE_GIFT), json!(-1), json!(1.5), json!("1 ARCADE")] {
            assert!(amount_of(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn welcome_settings_are_kept_and_checked() {
        let (_g, t) = crate::http::tests::node();
        assert!(auto_on(&t.node), "automatic welcomes are on unless the operator turns them off");
        assert_eq!(welcome_amount(&t.node), NODE_GIFT);
        let r = welcome_set(&t.node, &json!({"auto": false, "amount": 2 * NODE_GIFT})).unwrap();
        assert_eq!((r["auto"].clone(), r["amount"].clone()), (json!(false), json!(2 * NODE_GIFT)));
        assert!(!auto_on(&t.node));
        assert_eq!(welcome_amount(&t.node), 2 * NODE_GIFT);
        assert!(welcome_set(&t.node, &json!({"amount": 500 * NODE_GIFT})).is_err());
        assert_eq!(welcome_amount(&t.node), 2 * NODE_GIFT, "a refused amount changes nothing");
        // A pass with automatic welcomes off pays nothing, whatever is open.
        if let Ok(p) = chain() {
            assert!(!pay_node_gift(&t.node, p));
        }
    }

    /// SadFrog 2026-11-22: once one arcade has welcomed a new arcade, the notice must not stay up for everyone. Its
    /// payment notice (`sent` with the node) reaches the others over the mesh; from then on no arcade lists the new
    /// arcade as open, and every arcade has it as paid (so a press elsewhere is refused and no automatic payer pays).
    #[test]
    fn a_welcome_clears_the_notice_on_every_arcade() {
        let (_g, t) = crate::http::tests::node();
        let now = arcade_app::now();
        let newbie = ed25519_dalek::SigningKey::from_bytes(&[41; 32]);
        let payer = ed25519_dalek::SigningKey::from_bytes(&[42; 32]);
        let id = hex::encode(newbie.verifying_key().to_bytes());
        let addr = match crate::arcsvc::defined() {
            Some(p) => arcade_ledger::address::base58check(p.p2pkh_version, &[41; 20]),
            None => "aWelcomeTestAddress41".to_string(),
        };
        let ask = arcade_mesh::welcome::sign(json!({"a": id, "addr": addr, "at": now, "k": "node"}), &newbie);
        on_frame(&t.node, &json!({"recs": [ask]}));
        let open = |n: &Node| open_asks(n, arcade_app::now()).into_iter().any(|(n, ..)| n == id);
        assert!(open(&t.node), "a new arcade's ask shows as a notice");
        assert!(welcomes(&t.node).contains_key(&id), "and is noted for the high scores");
        // Another arcade welcomes it: its signed payment notice arrives.
        let sent = arcade_mesh::welcome::sign(
            json!({"a": hex::encode(payer.verifying_key().to_bytes()), "addr": addr, "at": now, "k": "sent", "txid": "ab".repeat(32), "node": id}), &payer);
        on_frame(&t.node, &json!({"recs": [sent.clone()]}));
        assert!(!open(&t.node), "the notice goes away once any arcade has welcomed it");
        assert!(paid_nodes(&t.node, &mut state().lock().unwrap()).contains_key(&id), "kept as paid (in the store too)");
        // The notice is passed on, so the arcades after this one clear it too; the same notice again is not.
        let passed = t.node.mesh_outbox.lock().unwrap().since(0).0.iter().filter(|f| f["recs"][0] == sent).count();
        assert_eq!(passed, 1);
        on_frame(&t.node, &json!({"recs": [sent]}));
        assert_eq!(t.node.mesh_outbox.lock().unwrap().since(0).0.iter().filter(|f| f["recs"][0]["k"] == "sent" && f["recs"][0]["node"] == id.as_str()).count(), 1);
        // The new arcade saying its ask again doesn't bring the notice back.
        let again = arcade_mesh::welcome::sign(json!({"a": id, "addr": addr, "at": now + 1, "k": "node"}), &newbie);
        on_frame(&t.node, &json!({"recs": [again]}));
        assert!(!open(&t.node));
        // A press on this arcade is refused (nothing is sent).
        assert!(welcome(&t.node, &id, None).is_err());
    }

    #[test]
    fn high_scores_count_first_welcomes_per_wallet() {
        let (_g, t) = crate::http::tests::node();
        let mut w = welcomes(&t.node);
        for (n, by, tag, h) in [("n1", "aA", Some("alice"), 10), ("n2", "aB", None, 11), ("n3", "aA", Some("alice"), 12)] {
            w.insert(format!("hs-{n}"), json!({"addr": format!("x{n}"), "at": 1, "winner": {"address": by, "tag": tag, "txid": "t", "height": h}}));
        }
        w.insert("hs-open".into(), json!({"addr": "xo", "at": 1, "winner": null}));
        save_welcomes(&t.node, &w);
        let s = scores(&t.node);
        let top = s["top"].as_array().unwrap();
        let a = top.iter().position(|x| x["address"] == "aA").unwrap();
        let b = top.iter().position(|x| x["address"] == "aB").unwrap();
        assert!(a < b, "more welcomes first");
        assert_eq!(top[a]["welcomes"], 2);
        assert_eq!(top[a]["tag"], "alice");
        assert!(s["recent"].as_array().unwrap().iter().all(|r| r["node"] != "hs-open"), "an open welcome scores nobody");
    }
}

