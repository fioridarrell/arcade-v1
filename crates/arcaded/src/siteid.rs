//! Sign-in for a website server in front of the arcade: the two ways a website can let its users
//! in, side by side, so each site (and each user) picks.
//!
//! **ARCADE ID, no password** (built on Digi-ID, created by the DigiByte community: its QR codes,
//! callback and signed URI are used unchanged, so Digi-ID wallets (Coinomi is the one digi-id.io names) work; an
//! ARCADE wallet may answer with a key on any chain it holds and says which in `chain`):
//!   id.start       a code for the site to show: digiid://<host><path>?x=<nonce>
//!   id.callback    what the wallet posted to the site's callback URL, checked here
//!   id.poll        the site's page asks until the scan is in: the address (and chain) that signed
//! and, for a new device to get the wallet from one the user already has (no copy on any server):
//!   id.link_start  the new browser's one-time public key in, a code + a 6-digit number out
//!   id.link_send   the user's other device sends the words sealed to that key (the arcade can't open them)
//!   id.link_poll   the new browser collects them, once
//!
//! **Password** (the original arcades' way): the browser seals the 12 words under the password
//! and the site keeps that sealed record; any arcade may ask for it, only the password opens it.
//!   vault.put      keep an account's sealed record here, signed by the @tag's own address
//!   vault.get      the sealed record of @tag: kept here, else from the arcade that holds it
//!   vault.delete   drop it ("stop using a password"), signed the same way
//!
//! Everything kept for a scan or a link lives in memory for five minutes and works once.

use crate::node::Node;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

type R = Result<Value, String>;

/// How long a sign-in code or a device link lasts (seconds).
pub const CODE_SECONDS: u64 = 300;
/// Most codes and links waiting at once (each kind); the oldest go first.
const MAX_WAITING: usize = 10_000;
/// Largest sealed message a device link carries, and largest sealed record (bytes of JSON).
const MAX_SEALED: usize = 16_384;
/// Digi-ID wallets sign with DigiByte's prefix (BitID ones with Bitcoin's).
const DIGIID_MAGICS: [&str; 2] = ["DigiByte Signed Message:\n", "Bitcoin Signed Message:\n"];
/// ARCADE's own prefix, which any chain's answer may use (arcade/llm.py MAGIC).
const ARCADE_MAGIC: &str = arcade_wallet::keys::PEPECOIN_MAGIC;
/// Chains an ARCADE ID never answers with (kept out of everything published).
const NEVER: [&str; 2] = ["kot", "wjk"];

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut b);
    hex::encode(b)
}

/// A chain's own Core message prefix, where it is known.
fn chain_magic(chain: &str) -> Option<&'static str> {
    Some(match chain {
        "pep" | "peptest" | "pepregtest" | "arc" => ARCADE_MAGIC,
        "doge" => "Dogecoin Signed Message:\n",
        "dgb" | "dgbtest" => "DigiByte Signed Message:\n",
        "ltc" | "ltctest" | "ltcregtest" => "Litecoin Signed Message:\n",
        "btc" => "Bitcoin Signed Message:\n",
        _ => return None,
    })
}

#[derive(Clone)]
struct Scan {
    uri: String,
    at: u64,
    done: Option<Value>,
}

#[derive(Clone)]
struct Link {
    key: String,
    at: u64,
    sealed: Option<String>,
}

static SCANS: LazyLock<Mutex<HashMap<String, Scan>>> = LazyLock::new(Default::default);
static LINKS: LazyLock<Mutex<HashMap<String, Link>>> = LazyLock::new(Default::default);

/// Drop what has run out; past MAX_WAITING, the oldest too.
fn prune<T>(m: &mut HashMap<String, T>, at: impl Fn(&T) -> u64, t: u64) {
    m.retain(|_, v| at(v) + CODE_SECONDS > t);
    while m.len() >= MAX_WAITING {
        let Some(oldest) = m.iter().min_by_key(|(_, v)| at(v)).map(|(k, _)| k.clone()) else { break };
        m.remove(&oldest);
    }
}

/// A host as a Digi-ID URI carries it: a name or address, an optional port, nothing else.
fn clean_host(host: &str) -> Result<String, String> {
    let h = host.trim().trim_start_matches("https://").trim_start_matches("http://").trim_end_matches('/').to_lowercase();
    if h.is_empty() || h.len() > 253 || !h.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '[' | ']')) {
        return Err("host: the site's host name, such as example.com".into());
    }
    Ok(h)
}

// ---------------- ARCADE ID sign-in ----------------

/// id.start: a sign-in code for the site to show as a QR code (and a link, for a wallet on the
/// same device). `path`: where the site takes the wallet's answer (default /digiid/callback);
/// `http`: the site is served over plain http (a test address: Digi-ID's u=1).
pub fn start(host: &str, path: Option<&str>, http: bool) -> R {
    let host = clean_host(host)?;
    let path = path.unwrap_or("/digiid/callback").trim();
    if !path.starts_with('/') || path.len() > 200 || path.contains(['?', '#', ' ']) {
        return Err("path: the callback's path on the site, such as /digiid/callback".into());
    }
    let nonce = random_hex(16);
    let uri = format!("digiid://{host}{path}?x={nonce}{}", if http { "&u=1" } else { "" });
    let t = now();
    let mut m = SCANS.lock().unwrap();
    prune(&mut m, |s: &Scan| s.at, t);
    m.insert(nonce.clone(), Scan { uri: uri.clone(), at: t, done: None });
    Ok(json!({"uri": uri, "nonce": nonce, "expires_in": CODE_SECONDS}))
}

fn nonce_of(uri: &str) -> Option<&str> {
    let q = uri.split_once('?')?.1;
    q.split('&').find_map(|kv| kv.strip_prefix("x="))
}

/// Whether `signature` is `address`'s Core signmessage over `uri`: with a chain, that chain's
/// prefix or ARCADE's; without, Digi-ID's.
fn signed(address: &str, signature: &str, uri: &str, chain: Option<&str>) -> bool {
    let magics: Vec<&str> = match chain {
        Some(c) => chain_magic(c).into_iter().chain([ARCADE_MAGIC]).collect(),
        None => DIGIID_MAGICS.to_vec(),
    };
    magics.iter().any(|m| arcade_wallet::keys::verifymessage(address, signature, m, uri))
}

/// id.callback: the wallet's answer, as it posted it to the site ({address, uri, signature,
/// chain?}). Checked here: a code this arcade gave out, not used, not run out, signed by that
/// address. Then id.poll hands the address to the site's page.
pub fn callback(address: &str, uri: &str, signature: &str, chain: Option<&str>) -> R {
    let chain = chain.map(str::trim).filter(|c| !c.is_empty());
    if let Some(c) = chain {
        if NEVER.contains(&c) || (chain_magic(c).is_none() && arcade_chain::params::by_key(c).is_none()) {
            return Err(format!("chain: {c} isn't one ARCADE ID takes"));
        }
    }
    let nonce = nonce_of(uri).ok_or("uri: not a sign-in code from this arcade")?.to_string();
    let t = now();
    let mut m = SCANS.lock().unwrap();
    let s = m.get_mut(&nonce).ok_or("this code isn't known here (or it has run out): show a new one")?;
    if s.at + CODE_SECONDS <= t {
        m.remove(&nonce);
        return Err("this code has run out: show a new one".into());
    }
    if s.uri != uri {
        return Err("uri: not the code this arcade gave out".into());
    }
    if s.done.is_some() {
        return Err("this code was already used".into());
    }
    if !signed(address.trim(), signature.trim(), uri, chain) {
        return Err("the signature doesn't match that address".into());
    }
    s.done = Some(json!({"address": address.trim(), "chain": chain, "digiid": chain.is_none()}));
    Ok(json!({"ok": true}))
}

/// id.poll: waiting, done (once: the address that signed, and its chain; chain null = a
/// Digi-ID wallet's DigiByte key for this site), expired or unknown.
pub fn poll(nonce: &str) -> R {
    let t = now();
    let mut m = SCANS.lock().unwrap();
    let Some(s) = m.get(nonce.trim()).cloned() else { return Ok(json!({"state": "unknown"})) };
    if let Some(d) = s.done {
        m.remove(nonce.trim());
        let mut out = d;
        out["state"] = json!("done");
        return Ok(out);
    }
    if s.at + CODE_SECONDS <= t {
        m.remove(nonce.trim());
        return Ok(json!({"state": "expired"}));
    }
    Ok(json!({"state": "waiting", "expires_in": s.at + CODE_SECONDS - t}))
}

// ---------------- adding a device ----------------

/// The 6-digit number both devices show for a link: the new browser from id.link_start, the
/// other device from the code it scanned (the code and the key), so a page that swapped the code
/// shows a different number.
pub fn link_number(code: &str, key: &str) -> String {
    let h = Sha256::new().chain_update(b"ARCADE/link\0").chain_update(code.as_bytes()).chain_update(b"\0").chain_update(key.to_lowercase().as_bytes()).finalize();
    format!("{:06}", u32::from_be_bytes([h[0], h[1], h[2], h[3]]) % 1_000_000)
}

/// id.link_start: the new browser's one-time public key (hex, 32 to 65 bytes) in; a code, the
/// number to compare and the QR text (arcadelink://<host>/<code>?k=<key>) out.
pub fn link_start(key: &str, host: &str) -> R {
    let key = key.trim().to_lowercase();
    let len = hex::decode(&key).map(|b| b.len()).unwrap_or(0);
    if !(32..=65).contains(&len) {
        return Err("key: the new browser's one-time public key, in hex".into());
    }
    let host = clean_host(host)?;
    let code = random_hex(16);
    let t = now();
    let mut m = LINKS.lock().unwrap();
    prune(&mut m, |l: &Link| l.at, t);
    m.insert(code.clone(), Link { key: key.clone(), at: t, sealed: None });
    Ok(json!({"code": code, "number": link_number(&code, &key), "qr": format!("arcadelink://{host}/{code}?k={key}"), "expires_in": CODE_SECONDS}))
}

/// id.link_send: the other device's sealed message for the new browser (sealed to its key; the
/// arcade only passes it on), once per code.
pub fn link_send(code: &str, sealed: &str) -> R {
    if sealed.is_empty() || sealed.len() > MAX_SEALED {
        return Err(format!("sealed: the sealed message, up to {MAX_SEALED} characters"));
    }
    let t = now();
    let mut m = LINKS.lock().unwrap();
    let l = m.get_mut(code.trim()).ok_or("this code isn't known here (or it has run out)")?;
    if l.at + CODE_SECONDS <= t {
        m.remove(code.trim());
        return Err("this code has run out".into());
    }
    if l.sealed.is_some() {
        return Err("this code was already used".into());
    }
    l.sealed = Some(sealed.to_string());
    Ok(json!({"ok": true}))
}

/// id.link_poll: waiting, done (once: the sealed message and the key it is sealed to), expired
/// or unknown.
pub fn link_poll(code: &str) -> R {
    let t = now();
    let mut m = LINKS.lock().unwrap();
    let Some(l) = m.get(code.trim()).cloned() else { return Ok(json!({"state": "unknown"})) };
    if let Some(s) = l.sealed {
        m.remove(code.trim());
        return Ok(json!({"state": "done", "sealed": s, "key": l.key}));
    }
    if l.at + CODE_SECONDS <= t {
        m.remove(code.trim());
        return Ok(json!({"state": "expired"}));
    }
    Ok(json!({"state": "waiting", "expires_in": l.at + CODE_SECONDS - t}))
}

// ---------------- password accounts (sealed records) ----------------

fn vault_key(tag: &str) -> Vec<u8> {
    format!("sitevault/{tag}").into_bytes()
}

fn clean_tag(tag: &str) -> Result<String, String> {
    let t = tag.trim().trim_start_matches('@').to_lowercase();
    if t.is_empty() || t.len() > 64 || !t.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err("tag: the account's @tag".into());
    }
    Ok(t)
}

/// A sealed record as the original arcades keep it: {kdf: "PBKDF2-SHA512", iterations, salt,
/// nonce, sealed}, hex. Only its shape is checked: nothing here can open it.
fn check_blob(blob: &Value) -> Result<(), String> {
    let hexy = |k: &str| blob[k].as_str().is_some_and(|s| !s.is_empty() && s.len() % 2 == 0 && s.chars().all(|c| c.is_ascii_hexdigit()));
    if blob["kdf"] != "PBKDF2-SHA512" || !blob["iterations"].as_u64().is_some_and(|n| (1000..=10_000_000).contains(&n)) || !hexy("salt") || !hexy("nonce") || !hexy("sealed") {
        return Err("blob: {kdf: \"PBKDF2-SHA512\", iterations, salt, nonce, sealed}, as the browser wallet seals it".into());
    }
    if blob.to_string().len() > MAX_SEALED {
        return Err("blob: too large".into());
    }
    Ok(())
}

fn blob_hash(blob: &Value) -> String {
    let canon = json!({"kdf": blob["kdf"], "iterations": blob["iterations"], "salt": blob["salt"], "nonce": blob["nonce"], "sealed": blob["sealed"]});
    hex::encode(Sha256::digest(canon.to_string().as_bytes()))
}

/// The words the @tag's address signs to keep a sealed record here.
pub fn put_words(tag: &str, address: &str, pubkey: &str, blob: &Value) -> String {
    format!("ARCADE vault @{tag} {address} {pubkey} {}", blob_hash(blob))
}

/// The words the @tag's address signs to drop it.
pub fn delete_words(tag: &str, address: &str, blob: &Value) -> String {
    format!("ARCADE vault delete @{tag} {address} {}", blob_hash(blob))
}

/// The address @tag names on the home chain.
fn owner(node: &Node, tag: &str) -> Result<String, String> {
    arcade_ledger::index::tag_row(&node.chain, node.ledger_home().chain_no, tag)
        .map_err(|e| e.to_string())?
        .map(|r| r.address)
        .ok_or_else(|| format!("@{tag} is not claimed"))
}

/// vault.put: keep @tag's sealed record here ({tag, address, pubkey, blob, signature}):
/// `address` must be the one @tag names on chain and `signature` its Core signmessage
/// (ARCADE's prefix) over put_words. `pubkey`: the account's login key, as the original
/// arcades keep it. Replaces an older record of the same @tag.
pub fn vault_put(node: &Node, req: &Value) -> R {
    let tag = clean_tag(req["tag"].as_str().unwrap_or(""))?;
    let address = req["address"].as_str().unwrap_or("").trim().to_string();
    let pubkey = req["pubkey"].as_str().unwrap_or("").trim().to_lowercase();
    let blob = &req["blob"];
    check_blob(blob)?;
    if hex::decode(&pubkey).map(|b| b.len()).unwrap_or(0) != 32 && hex::decode(&pubkey).map(|b| b.len()).unwrap_or(0) != 33 {
        return Err("pubkey: the account's login public key, in hex".into());
    }
    if owner(node, &tag)? != address {
        return Err(format!("address: not the one @{tag} names on chain"));
    }
    let words = put_words(&tag, &address, &pubkey, blob);
    if !arcade_wallet::keys::verifymessage(&address, req["signature"].as_str().unwrap_or(""), ARCADE_MAGIC, &words) {
        return Err(format!("signature: sign \"{words}\" with @{tag}'s address"));
    }
    let rec = json!({"tag": tag, "address": address, "pubkey": pubkey, "blob": blob, "at": now()});
    node.app.store.write(|w| w.put(arcade_app::META, &vault_key(&tag), rec.to_string().as_bytes())).map_err(|e| e.to_string())?;
    Ok(json!({"ok": true, "tag": tag}))
}

fn kept(node: &Node, tag: &str) -> Option<Value> {
    node.app.store.read(|r| r.get(arcade_app::META, &vault_key(tag))).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok())
}

/// vault.get: @tag's sealed record, {tag, address, pubkey, blob, from?}: kept here, else (unless
/// `here`) from the arcade that holds it. The answer has the shape of the original arcades'
/// /signin/<tag>, so a site can serve it there and other arcades find accounts made on it.
pub fn vault_get(node: &Node, tag: &str, here: bool) -> R {
    let tag = clean_tag(tag)?;
    if let Some(mut v) = kept(node, &tag) {
        // Still this @tag's? A record whose @tag moved on is not handed out.
        if owner(node, &tag).ok().as_deref() == v["address"].as_str() {
            v.as_object_mut().map(|o| o.remove("at"));
            return Ok(v);
        }
    }
    if here {
        return Err(format!("no sealed record of @{tag} here"));
    }
    let (from, mut v) = crate::accountsvc::find_vault(node, &tag, None)?;
    v["from"] = json!(from);
    Ok(v)
}

/// vault.delete: drop @tag's sealed record here ({tag, signature}: the @tag's address signs
/// delete_words over the record kept). After it, the account opens only with ARCADE ID or its 12
/// words on this site.
pub fn vault_delete(node: &Node, tag: &str, signature: &str) -> R {
    let tag = clean_tag(tag)?;
    let v = kept(node, &tag).ok_or_else(|| format!("no sealed record of @{tag} here"))?;
    let address = owner(node, &tag)?;
    let words = delete_words(&tag, &address, &v["blob"]);
    if !arcade_wallet::keys::verifymessage(&address, signature, ARCADE_MAGIC, &words) {
        return Err(format!("signature: sign \"{words}\" with @{tag}'s address"));
    }
    node.app.store.write(|w| w.del(arcade_app::META, &vault_key(&tag))).map_err(|e| e.to_string())?;
    Ok(json!({"ok": true}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcade_wallet::keys::signmessage;
    use arcade_wallet::secp256k1::{PublicKey, SecretKey, SECP256K1};

    fn p2pkh(sk: &SecretKey, version: u8) -> String {
        let pk = PublicKey::from_secret_key(SECP256K1, sk).serialize();
        arcade_ledger::address::base58check(version, &arcade_chain::hash::hash160(&pk))
    }

    #[test]
    fn a_digiid_scan_signs_in_once() {
        let sk = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let addr = p2pkh(&sk, 0x1e);
        let s = start("Example.com", None, false).unwrap();
        let (uri, nonce) = (s["uri"].as_str().unwrap().to_string(), s["nonce"].as_str().unwrap().to_string());
        assert!(uri.starts_with("digiid://example.com/digiid/callback?x="));
        assert_eq!(poll(&nonce).unwrap()["state"], "waiting");
        // Signed with another prefix or over another code: refused.
        assert!(callback(&addr, &uri, &signmessage(&sk, ARCADE_MAGIC, &uri), None).is_err());
        assert!(callback(&addr, &format!("{uri}0"), &signmessage(&sk, DIGIID_MAGICS[0], &format!("{uri}0")), None).is_err());
        // A Digi-ID wallet's answer: in, once.
        let sig = signmessage(&sk, DIGIID_MAGICS[0], &uri);
        assert!(callback(&addr, &uri, &sig, None).is_ok());
        assert!(callback(&addr, &uri, &sig, None).is_err());
        let done = poll(&nonce).unwrap();
        assert_eq!(done["state"], "done");
        assert_eq!(done["address"], addr.as_str());
        assert_eq!(done["digiid"], true);
        assert_eq!(poll(&nonce).unwrap()["state"], "unknown");
    }

    #[test]
    fn an_arcade_wallet_answers_on_its_own_chain() {
        let sk = SecretKey::from_slice(&[9u8; 32]).unwrap();
        let addr = p2pkh(&sk, 0x1e);
        let s = start("example.com", Some("/id/back"), true).unwrap();
        let uri = s["uri"].as_str().unwrap().to_string();
        assert!(uri.ends_with("&u=1") && uri.contains("/id/back?x="));
        assert!(callback(&addr, &uri, &signmessage(&sk, "Dogecoin Signed Message:\n", &uri), Some("kot")).is_err());
        assert!(callback(&addr, &uri, &signmessage(&sk, "Dogecoin Signed Message:\n", &uri), Some("doge")).is_ok());
        let done = poll(s["nonce"].as_str().unwrap()).unwrap();
        assert_eq!(done["chain"], "doge");
        assert_eq!(done["digiid"], false);
    }

    #[test]
    fn a_device_link_passes_one_sealed_message() {
        let key = "02".to_string() + &"ab".repeat(32);
        let l = link_start(&key, "example.com").unwrap();
        let code = l["code"].as_str().unwrap();
        // The other device works the number out from what it scanned.
        assert_eq!(l["number"], link_number(code, &key).as_str());
        assert_eq!(l["number"].as_str().unwrap().len(), 6);
        assert_eq!(l["qr"], format!("arcadelink://example.com/{code}?k={key}").as_str());
        assert_ne!(link_number(code, &("03".to_string() + &"ab".repeat(32))), link_number(code, &key));
        assert_eq!(link_poll(code).unwrap()["state"], "waiting");
        assert!(link_send(code, "c2VhbGVk").is_ok());
        assert!(link_send(code, "again").is_err());
        let got = link_poll(code).unwrap();
        assert_eq!(got["sealed"], "c2VhbGVk");
        assert_eq!(got["key"], key.as_str());
        assert_eq!(link_poll(code).unwrap()["state"], "unknown");
        assert!(link_start("nothex", "example.com").is_err());
    }

    #[test]
    fn a_sealed_record_has_the_original_shape() {
        let good = json!({"kdf": "PBKDF2-SHA512", "iterations": 600000, "salt": "00ff", "nonce": "0a0b", "sealed": "beef"});
        assert!(check_blob(&good).is_ok());
        assert!(check_blob(&json!({"kdf": "PBKDF2-SHA512", "iterations": 5, "salt": "00", "nonce": "00", "sealed": "00"})).is_err());
        assert!(check_blob(&json!({"kdf": "scrypt", "iterations": 600000, "salt": "00", "nonce": "00", "sealed": "00"})).is_err());
        // The words to sign name the record exactly, whatever order its fields came in.
        let reordered = json!({"sealed": "beef", "nonce": "0a0b", "salt": "00ff", "iterations": 600000, "kdf": "PBKDF2-SHA512"});
        assert_eq!(put_words("a", "b", "c", &good), put_words("a", "b", "c", &reordered));
        assert_ne!(delete_words("a", "b", &good), delete_words("a", "b", &json!({"sealed": "bee0"})));
        assert!(clean_tag("@Apple").is_ok_and(|t| t == "apple"));
        assert!(clean_tag("a b").is_err());
    }
}
