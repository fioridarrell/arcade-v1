//! Messaging, this arcade's side: your messaging key (from your 12 words, as the original's
//! accounts), the scanner that tries it on every candidate the home chain's Messages index
//! keeps, and the reads behind the Messages tab. Opened messages are stored sealed with this
//! arcade's secret key (app.v2.mail); nothing private is written in the clear.

use crate::node::Node;
use arcade_msg::content::Body;
use arcade_msg::envelope as env;
use arcade_msg::MsgKeys;
use arcade_store::KeyBuf;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// Messaging keys, derived once per phrase.
pub fn keys(node: &Node) -> Result<MsgKeys, String> {
    static CACHE: OnceLock<Mutex<Option<(Vec<u8>, MsgKeys)>>> = OnceLock::new();
    let phrase = node.app.phrase().map_err(|e| e.to_string())?;
    let id = sha2_of(phrase.as_bytes());
    let mut c = CACHE.get_or_init(Default::default).lock().unwrap();
    if let Some((i, k)) = c.as_ref() {
        if *i == id {
            return Ok(k.clone());
        }
    }
    let seed = arcade_wallet::keys::seed_from_phrase(&phrase).map_err(|e| e.to_string())?;
    let k = MsgKeys::from_seed(&seed[..]);
    *c = Some((id, k.clone()));
    Ok(k)
}

fn sha2_of(b: &[u8]) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(b).to_vec()
}

fn cursor_key(fp: &str) -> Vec<u8> {
    format!("mail_cursor:{fp}").into_bytes()
}

fn mail_key(txid: &str) -> Vec<u8> {
    let mut k = b"m".to_vec();
    k.extend_from_slice(&hex::decode(txid).unwrap_or_default());
    k
}

fn file_key(txid: &str) -> Vec<u8> {
    let mut k = b"f".to_vec();
    k.extend_from_slice(&hex::decode(txid).unwrap_or_default());
    k
}

/// Keep one opened (or sent) message. Returns false when it was already kept.
pub fn keep(node: &Node, row: &Value, file: Option<&[u8]>) -> Result<bool, String> {
    let txid = row["id"].as_str().unwrap_or_default();
    if node.app.get_sealed(arcade_app::MAIL, &mail_key(txid)).map_err(|e| e.to_string())?.is_some() {
        return Ok(false);
    }
    if let Some(f) = file {
        node.app.put_sealed(arcade_app::MAIL, &file_key(txid), f).map_err(|e| e.to_string())?;
    }
    node.app.put_sealed(arcade_app::MAIL, &mail_key(txid), row.to_string().as_bytes()).map_err(|e| e.to_string())?;
    Ok(true)
}

/// Every kept message (not files), any order.
pub fn all(node: &Node) -> Vec<Value> {
    node.app
        .scan_sealed(arcade_app::MAIL, b"m")
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_slice(&v).ok())
        .collect()
}

/// A message row from what was opened.
fn row_of(me: &[u8; 32], opened: &env::Opened, cand: &Value, height: u32, at: u64) -> (Value, Option<Vec<u8>>) {
    let body = Body::parse(&opened.message);
    let file = body.file.as_ref().map(|f| f.bytes.clone());
    let mut extra = body.extra.clone();
    let mine = opened.sender == *me;
    // Our own copy of something we sent: V1.0 notes the recipient in the body.
    let (dir, peer, peer_address) = if mine {
        match extra.get("to").and_then(|t| t.as_str()) {
            Some(to) => ("out", to.to_string(), extra.get("to_address").and_then(|a| a.as_str()).unwrap_or("").to_string()),
            None => ("note", hex::encode(me), String::new()),
        }
    } else {
        ("in", hex::encode(opened.sender), cand["from"].as_str().unwrap_or("").to_string())
    };
    extra.remove("to");
    extra.remove("to_address");
    let kind = match opened.kind {
        env::API => "api",
        _ if body.text.starts_with("\u{0}arcade-group/1\n") => "group_invite",
        _ if body.text.starts_with("\u{0}arcade-group-left/1\n") => "group_left",
        _ => "dm",
    };
    let row = json!({
        "id": cand["txid"], "txids": [cand["txid"]], "dir": dir, "peer": peer, "peer_address": peer_address,
        "from_address": cand["from"], "at": at, "height": height, "kind": kind,
        "text": body.text, "file": body.file.as_ref().map(|f| json!({"name": f.name, "type": f.mime, "size": f.bytes.len()})),
        "extra": Value::Object(extra), "unread": dir == "in",
    });
    (row, file)
}

/// One scanning pass over new candidates. Returns how many messages were opened.
pub fn scan(node: &Node) -> Result<usize, String> {
    if !node.app.has_wallet().map_err(|e| e.to_string())? {
        return Ok(0);
    }
    let k = keys(node)?;
    let me = k.public_bytes();
    let c = node.params.chain_no;
    let ck = cursor_key(&k.fingerprint());
    let mut cursor: Option<(u32, u32)> = node
        .app
        .store
        .read(|r| r.get(arcade_app::META, &ck))
        .ok()
        .flatten()
        .filter(|v| v.len() == 8)
        .map(|v| (u32::from_be_bytes(v[..4].try_into().unwrap()), u32::from_be_bytes(v[4..].try_into().unwrap())));
    let mut opened = 0;
    loop {
        let batch = arcade_ledger::messages::candidates(&node.chain, c, cursor, 500).map_err(|e| e.to_string())?;
        if batch.is_empty() {
            break;
        }
        for (h, p, cand) in &batch {
            let payload = hex::decode(cand["payload"].as_str().unwrap_or("")).unwrap_or_default();
            let at = block_time(node, *h);
            match cand["kind"].as_u64().unwrap_or(0) as u8 {
                env::SINGLE | env::API => {
                    if let Ok(o) = env::open_single(&k.secret, &payload) {
                        let (row, file) = row_of(&me, &o, cand, *h, at);
                        if keep(node, &row, file.as_deref())? {
                            opened += 1;
                        }
                    }
                }
                env::CHUNK => {
                    if let Some((id, cd, piece)) = env::chunk_parts(&payload) {
                        let from = cand["from"].as_str().unwrap_or("");
                        let base = KeyBuf::new().bytes(from.as_bytes()).u8(0).bytes(&id).done();
                        let mut pk = base.clone();
                        pk.extend_from_slice(&cd.to_be_bytes());
                        let rec = json!({"txid": cand["txid"], "piece": hex::encode(&piece)});
                        node.app.store.write(|w| w.put(arcade_app::MAIL_PARTS, &pk, rec.to_string().as_bytes())).map_err(|e| e.to_string())?;
                        let rows = node.app.store.read(|r| r.scan_prefix(arcade_app::MAIL_PARTS, &base)).map_err(|e| e.to_string())?;
                        let cds: Vec<u16> = rows.iter().map(|(k, _)| u16::from_be_bytes([k[k.len() - 2], k[k.len() - 1]])).collect();
                        if cds.len() == cds.iter().copied().max().unwrap_or(0) as usize + 1 {
                            let vals: Vec<Value> = rows.iter().filter_map(|(_, v)| serde_json::from_slice(v).ok()).collect();
                            let mut parts: Vec<(u16, Vec<u8>)> = cds.iter().zip(&vals).map(|(cd, v)| (*cd, hex::decode(v["piece"].as_str().unwrap_or("")).unwrap_or_default())).collect();
                            if let Ok(o) = env::open_chunks(&k.secret, id, &mut parts) {
                                // Known by the first chunk's txid (countdown highest).
                                let mut order: Vec<(u16, String)> = cds.iter().zip(&vals).map(|(cd, v)| (*cd, v["txid"].as_str().unwrap_or("").to_string())).collect();
                                order.sort_by(|a, b| b.0.cmp(&a.0));
                                let mut first = cand.clone();
                                first["txid"] = json!(order[0].1);
                                let (mut row, file) = row_of(&me, &o, &first, *h, at);
                                row["txids"] = json!(order.iter().map(|x| x.1.clone()).collect::<Vec<_>>());
                                if keep(node, &row, file.as_deref())? {
                                    opened += 1;
                                }
                            }
                            node.app.store.write(|w| {
                                for (k, _) in &rows {
                                    w.del(arcade_app::MAIL_PARTS, k)?;
                                }
                                Ok(())
                            }).map_err(|e| e.to_string())?;
                        }
                    }
                }
                _ => {}
            }
            cursor = Some((*h, *p));
        }
        let (h, p) = cursor.unwrap();
        let mut v = h.to_be_bytes().to_vec();
        v.extend_from_slice(&p.to_be_bytes());
        node.app.store.write(|w| w.put(arcade_app::META, &ck, &v)).map_err(|e| e.to_string())?;
    }
    if opened > 0 {
        let _ = node.app.event("messages", &format!("{opened} new message{}", if opened == 1 { "" } else { "s" }));
    }
    Ok(opened)
}

fn block_time(node: &Node, h: u32) -> u64 {
    let c = node.params.chain_no;
    node.chain.hash_at(c, h).ok().flatten().and_then(|hash| node.chain.header(c, &hash).ok().flatten()).map(|r| r.header.time as u64).unwrap_or_else(arcade_app::now)
}

/// Background: scan for new messages every 15 seconds.
pub fn run(node: std::sync::Arc<Node>) {
    loop {
        if let Err(e) = scan(&node) {
            let _ = node.app.event("messages", &format!("scan stopped: {e}"));
            std::thread::sleep(Duration::from_secs(300));
        }
        std::thread::sleep(Duration::from_secs(15));
    }
}

// ---------------- people ----------------

/// Published profiles on the home chain by messaging key (hex).
fn profiles_by_key(node: &Node) -> BTreeMap<String, Value> {
    let c = node.params.chain_no;
    let pre = KeyBuf::new().u16(c).done();
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    for (_, v) in node.chain.store.read(|r| r.scan_prefix(arcade_ledger::index::PROFILE, &pre)).unwrap_or_default() {
        let Ok(v) = serde_json::from_slice::<Value>(&v) else { continue };
        if let Some(k) = v["pubkey"].as_str() {
            out.insert(k.to_string(), v);
        }
    }
    out
}

fn contacts(node: &Node) -> BTreeMap<String, Value> {
    node.app
        .store
        .read(|r| r.scan_prefix(arcade_app::CONTACTS, b""))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| Some((String::from_utf8(k).ok()?, serde_json::from_slice(&v).ok()?)))
        .collect()
}

pub fn mutes(node: &Node) -> BTreeMap<String, u64> {
    node.app
        .store
        .read(|r| r.scan_prefix(arcade_app::MUTES, b""))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(k, v)| Some((String::from_utf8(k).ok()?, u64::from_be_bytes(v.get(..8)?.try_into().ok()?))))
        .collect()
}

/// Who a messaging key is: contact name, published name, @tag, address.
fn person(node: &Node, key: &str, fallback_address: &str, profiles: &BTreeMap<String, Value>, contacts: &BTreeMap<String, Value>) -> Value {
    let c = node.params.chain_no;
    let prof = profiles.get(key);
    let address = prof.and_then(|p| p["address"].as_str()).filter(|_| prof.is_some_and(|p| p["stated"] == true)).unwrap_or(fallback_address).to_string();
    let tag = if address.is_empty() { None } else { arcade_ledger::index::tag_of(&node.chain, c, &address).ok().flatten() };
    let contact = contacts.get(key);
    let fp = hex::decode(key).ok().and_then(|b| <[u8; 32]>::try_from(b).ok()).map(|b| arcade_msg::keys::fingerprint(&b)).unwrap_or_default();
    json!({
        "key": key, "fingerprint": fp, "address": address, "tag": tag,
        "name": contact.and_then(|c| c["name"].as_str()).or(prof.and_then(|p| p["name"].as_str())).unwrap_or(""),
        "contact": contact.is_some(), "pfp": prof.map(|p| p["pfp"].clone()),
    })
}

/// Who `name` is (@tag or address) and their published messaging key.
pub fn who(node: &Node, name: &str) -> Result<Value, String> {
    let c = node.params.chain_no;
    let n = name.trim();
    let address = if let Some(t) = n.strip_prefix('@') {
        arcade_ledger::index::tag_row(&node.chain, c, &t.to_lowercase()).map_err(|e| e.to_string())?.ok_or_else(|| format!("{n} is not claimed"))?.address
    } else {
        n.to_string()
    };
    let prof = arcade_ledger::index::profile(&node.chain, c, &address).map_err(|e| e.to_string())?;
    let Some(prof) = prof.filter(|p| p["stated"] == true || p["address"] == address.as_str()) else {
        return Err(format!("{n} has not published a messaging key yet, so messages can't be sealed to them"));
    };
    let key = prof["pubkey"].as_str().unwrap_or("").to_string();
    Ok(person(node, &key, &address, &profiles_by_key(node), &contacts(node)))
}

// ---------------- reads for the Messages tab ----------------

pub fn me(node: &Node) -> Result<Value, String> {
    let k = keys(node)?;
    let key = hex::encode(k.public_bytes());
    let profs = profiles_by_key(node);
    let announced = profs.get(&key).is_some_and(|p| p["stated"] == true);
    Ok(json!({"key": key, "fingerprint": k.fingerprint(), "announced": announced, "profile": profs.get(&key)}))
}

/// Conversations: one per person (or group), newest first. Muted people are left out; people
/// you haven't added or written to are "requests" unless `requests` is asked for.
pub fn conversations(node: &Node, requests: bool) -> Result<Value, String> {
    let me_key = hex::encode(keys(node)?.public_bytes());
    let (profs, cons, muted) = (profiles_by_key(node), contacts(node), mutes(node));
    let mut by: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for m in all(node).into_iter().filter(|m| m["kind"] == "dm" || m["kind"] == "api") {
        by.entry(m["peer"].as_str().unwrap_or("").to_string()).or_default().push(m);
    }
    let mut out: Vec<Value> = Vec::new();
    for (peer, mut ms) in by {
        ms.sort_by_key(|m| m["at"].as_u64().unwrap_or(0));
        let last = ms.last().cloned().unwrap_or(Value::Null);
        let addr = ms.iter().rev().find_map(|m| m["peer_address"].as_str().filter(|a| !a.is_empty())).unwrap_or("").to_string();
        let who = person(node, &peer, &addr, &profs, &cons);
        let is_muted = muted.contains_key(who["address"].as_str().unwrap_or(""));
        let wrote = ms.iter().any(|m| m["dir"] == "out");
        let request = !who["contact"].as_bool().unwrap_or(false) && !wrote && peer != me_key;
        if is_muted || request != requests {
            continue;
        }
        out.push(json!({"peer": peer, "who": who, "last": {"text": last["text"], "at": last["at"], "dir": last["dir"], "file": last["file"]},
                        "unread": ms.iter().filter(|m| m["unread"] == true).count(), "count": ms.len()}));
    }
    out.sort_by_key(|c| std::cmp::Reverse(c["last"]["at"].as_u64().unwrap_or(0)));
    Ok(json!(out))
}

pub fn thread(node: &Node, peer: &str) -> Result<Value, String> {
    let mut ms: Vec<Value> = all(node).into_iter().filter(|m| m["peer"] == peer && (m["kind"] == "dm" || m["kind"] == "api")).collect();
    ms.sort_by_key(|m| (m["at"].as_u64().unwrap_or(0), m["height"].as_u64().unwrap_or(0)));
    let addr = ms.iter().rev().find_map(|m| m["peer_address"].as_str().filter(|a| !a.is_empty())).unwrap_or("").to_string();
    Ok(json!({"who": person(node, peer, &addr, &profiles_by_key(node), &contacts(node)), "messages": ms}))
}

pub fn mark_read(node: &Node, peer: &str) -> Result<Value, String> {
    let mut n = 0;
    for mut m in all(node).into_iter().filter(|m| m["peer"] == peer && m["unread"] == true) {
        m["unread"] = json!(false);
        let id = m["id"].as_str().unwrap_or("").to_string();
        node.app.put_sealed(arcade_app::MAIL, &mail_key(&id), m.to_string().as_bytes()).map_err(|e| e.to_string())?;
        n += 1;
    }
    Ok(json!({"read": n}))
}

pub fn unread(node: &Node) -> Value {
    let muted = mutes(node);
    let cons = contacts(node);
    let ms = all(node);
    let wrote: std::collections::BTreeSet<String> = ms.iter().filter(|m| m["dir"] == "out").filter_map(|m| m["peer"].as_str().map(String::from)).collect();
    let (mut n, mut req) = (0, 0);
    for m in ms.iter().filter(|m| m["unread"] == true && (m["kind"] == "dm" || m["kind"] == "api")) {
        if muted.contains_key(m["peer_address"].as_str().unwrap_or("")) {
            continue;
        }
        let peer = m["peer"].as_str().unwrap_or("");
        if cons.contains_key(peer) || wrote.contains(peer) {
            n += 1;
        } else {
            req += 1;
        }
    }
    json!({"unread": n, "requests": req})
}

pub fn file(node: &Node, id: &str) -> Result<Value, String> {
    let m = node.app.get_sealed(arcade_app::MAIL, &mail_key(id)).map_err(|e| e.to_string())?.ok_or("no such message")?;
    let m: Value = serde_json::from_slice(&m).map_err(|e| e.to_string())?;
    let bytes = node.app.get_sealed(arcade_app::MAIL, &file_key(id)).map_err(|e| e.to_string())?.ok_or("this message has no file")?;
    use base64::Engine;
    Ok(json!({"name": m["file"]["name"], "type": m["file"]["type"], "data": base64::engine::general_purpose::STANDARD.encode(bytes)}))
}

// ---------------- contacts and mutes ----------------

pub fn contacts_list(node: &Node) -> Value {
    let profs = profiles_by_key(node);
    let cons = contacts(node);
    json!(cons.iter().map(|(k, c)| {
        let mut p = person(node, k, c["address"].as_str().unwrap_or(""), &profs, &cons);
        p["notes"] = c["notes"].clone();
        p
    }).collect::<Vec<_>>())
}

pub fn contact_save(node: &Node, key: &str, name: &str, address: &str, notes: &str) -> Result<Value, String> {
    if hex::decode(key).map(|b| b.len()) != Ok(32) {
        return Err("a contact is a 32-byte messaging key (hex)".into());
    }
    let mut m = Map::new();
    m.insert("name".into(), json!(name.trim()));
    m.insert("address".into(), json!(address.trim()));
    m.insert("notes".into(), json!(notes));
    m.insert("added".into(), json!(arcade_app::now()));
    node.app.store.write(|w| w.put(arcade_app::CONTACTS, key.as_bytes(), Value::Object(m).to_string().as_bytes())).map_err(|e| e.to_string())?;
    Ok(json!({"saved": key}))
}

pub fn contact_delete(node: &Node, key: &str) -> Result<Value, String> {
    node.app.store.write(|w| w.del(arcade_app::CONTACTS, key.as_bytes())).map_err(|e| e.to_string())?;
    Ok(json!({"deleted": key}))
}

pub fn mute(node: &Node, address: &str, on: bool) -> Result<Value, String> {
    node.app
        .store
        .write(|w| if on { w.put(arcade_app::MUTES, address.as_bytes(), &arcade_app::now().to_be_bytes()) } else { w.del(arcade_app::MUTES, address.as_bytes()).map(|_| ()) })
        .map_err(|e| e.to_string())?;
    Ok(json!({"address": address, "muted": on}))
}
