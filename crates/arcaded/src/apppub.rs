//! `apps.publish` and `apps.withdraw` (2026-11-21, Martin): inscribe an app page (or its icon, or any file) signed with
//! this arcade's own wallet key, as `tag.claim` and `wallet.send` sign, so a publisher never has to export a key to a
//! file for the separate arcade-publish tool.
//!
//! The bytes on the chain are exactly what arcade-publish (v1publish, `--name x.html --type ... --json <card> file`)
//! writes, so the ledger and the App Store read both alike (docs/compat/INSC.md):
//!
//! - one stream: the manifest (total u32, sha256, type length u8, type, JSON length u16, JSON) then the content;
//! - cut EVENLY (not greedily) into as few pieces as hold it at 7,628 bytes each, every piece
//!   "INSC" 01 01 | id8 | countdown u16 | clen u16 | body, the first piece with the highest countdown;
//! - each piece wrapped in AnyData (version 0, type 200) and carried Class C when "arcd" + it fits 80 bytes, else
//!   Class B (the marker output, 1-of-3 bare multisig outputs holding our own key and two data keys, 0.01 each),
//!   change back to the sender, on every chain alike (the ARCADE chain too, as the tool does there);
//! - several pieces: a split first (one output per piece, to our own address), then each piece spending its own
//!   output. On the ARCADE chain the pieces go out at once, spending the split while it waits (its pool takes up to
//!   25 in a row); on a coin chain they wait for the split's first block, as the tool does.
//!
//! The inscription's id is the first piece's txid, served at /app/<chain>/<id>.
//!
//! A journal in the arcade's META (`apppub:<key>`) keyed by client_ref, or by the content's sha256, the chain and the
//! type and card, notes each transaction's txid and signed bytes BEFORE it is broadcast. Asked again, a publication
//! finishes what is unfinished: a transaction the chain already knows is not sent again, one it doesn't know goes out
//! again as the same bytes (the same coins, so it can be paid at most once), and only one the network refuses is
//! built afresh. Coins carrying an inscription are never spent (crate::ordsafe), nor the outputs of a split another
//! unfinished publication still needs.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use arcade_chain::{Hash256, OutPoint, Tx};
use arcade_wallet::secp256k1::{PublicKey, SECP256K1};
use arcade_wallet::tx::{TxOutSpec, Utxo};
use arcade_wallet::SecretKey;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::sync::Mutex;

/// The largest file published at once (2026-11-21): 4 MB, about 550 pieces. The tool has no ceiling of its own (the
/// format's is 65,536 pieces); this keeps one call's transactions inside what a pool holds.
pub const MAX_BYTES: usize = 4 * 1024 * 1024;
/// What one piece carries after the AnyData header: Class B's 255 packets of 30 bytes, less its 4 (the tool's
/// CHUNK_CAPACITY).
const CAPACITY: usize = arcade_wallet::tx::CLASS_B_MAX - 4;
/// "INSC" 01 01, id8, countdown u16, clen u16.
const HEADER: usize = 18;
/// Most OP_RETURN data ("arcd" + AnyData) carried Class C: the 83-byte datacarrier limit's 76 bytes of payload, as
/// the tool uses on every chain.
const CLASS_C_MAX: usize = 80;
/// The format's most pieces (the countdown is two bytes).
const MAX_PIECES: usize = 0x10000;
/// The JSON field's most bytes.
const MAX_JSON: usize = 65_535;
/// How long a publication on a coin chain waits in the background for its split's first block before it leaves
/// the rest to a call again: two hours.
const WAIT_SPLIT_SECS: u64 = 7200;

// ---------------- the format (ported from arcade-publish: arcade/inscriptions.py, inscribe.py, media.py) ----------------

/// Other names JavaScript goes by (media.JS_NAMES): all inscribed as application/javascript.
const JS_NAMES: &[&str] = &["application/x-javascript", "text/javascript", "application/ecmascript", "text/ecmascript", "application/x-ecmascript", "text/x-javascript"];

/// The one name for a content type (media.standard_type): JavaScript under any alias is application/javascript, an
/// untyped .js or .mjs file too; the base is trimmed and lowercased, parameters such as a charset kept as given.
pub fn standard_type(content_type: &str, name: &str) -> String {
    let (base, sep, rest) = match content_type.split_once(';') {
        Some((b, r)) => (b, ";", r),
        None => (content_type, "", ""),
    };
    let mut kind = base.trim().to_lowercase();
    if JS_NAMES.contains(&kind.as_str()) {
        kind = "application/javascript".into();
    }
    if (kind.is_empty() || kind == "application/octet-stream") && !name.is_empty() {
        if let Some(dot) = name.rfind('.') {
            if matches!(name[dot..].to_lowercase().as_str(), ".js" | ".mjs") {
                kind = "application/javascript".into();
            }
        }
    }
    if kind.is_empty() { String::new() } else { format!("{kind}{sep}{rest}") }
}

/// A type from a file's name when none was given (what Python's mimetypes says for the files apps are made of).
fn type_by_name(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "application/javascript",
        "json" => "application/json",
        "txt" => "text/plain",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "ico" => "image/vnd.microsoft.icon",
        "wasm" => "application/wasm",
        "mp3" => "audio/mpeg",
        "wav" => "audio/x-wav",
        "mp4" => "video/mp4",
        _ => "",
    }
}

/// The content type a publication is inscribed with (the tool's choice): the type given, standardized; else from
/// the name; else application/octet-stream.
pub fn content_type(given: &str, name: &str) -> String {
    let t = standard_type(given, name);
    if !t.is_empty() {
        return t;
    }
    let t = standard_type(type_by_name(name), "");
    if t.is_empty() { "application/octet-stream".into() } else { t }
}

/// The JSON beside the file, as the tool's `--json` takes it: a string is used as written (trimmed), an object is
/// written compactly; either must parse, and none is "".
pub fn card_text(card: &Value) -> Result<String, String> {
    let text = match card {
        Value::Null => return Ok(String::new()),
        Value::String(s) => s.trim().to_string(),
        other => other.to_string(),
    };
    if text.is_empty() {
        return Ok(text);
    }
    serde_json::from_str::<Value>(&text).map_err(|e| format!("card is not valid JSON ({e}): an inscription can't be corrected later"))?;
    if text.len() > MAX_JSON {
        return Err(format!("card is limited to {MAX_JSON} bytes of JSON"));
    }
    Ok(text)
}

/// The inscription's pieces, in sending order (inscriptions.plan): the manifest stream cut evenly.
pub fn chunks(content: &[u8], ctype: &str, json: &str, id: [u8; 8]) -> Result<Vec<Vec<u8>>, String> {
    if content.is_empty() {
        return Err("there is nothing to inscribe".into());
    }
    if ctype.len() > 255 {
        return Err("the content type is at most 255 bytes".into());
    }
    if json.len() > MAX_JSON {
        return Err(format!("the JSON is at most {MAX_JSON} bytes"));
    }
    let mut stream = (content.len() as u32).to_be_bytes().to_vec();
    stream.extend_from_slice(&Sha256::digest(content));
    stream.push(ctype.len() as u8);
    stream.extend_from_slice(ctype.as_bytes());
    stream.extend_from_slice(&(json.len() as u16).to_be_bytes());
    stream.extend_from_slice(json.as_bytes());
    stream.extend_from_slice(content);
    let room = CAPACITY - HEADER;
    let count = stream.len().div_ceil(room);
    if count > MAX_PIECES {
        return Err(format!("that needs {count} pieces and the format carries at most {MAX_PIECES}"));
    }
    let even = stream.len().div_ceil(count);
    let pieces: Vec<&[u8]> = stream.chunks(even).collect();
    let n = pieces.len();
    Ok(pieces
        .into_iter()
        .enumerate()
        .map(|(i, body)| {
            let mut d = b"INSC".to_vec();
            d.extend_from_slice(&[1, 1]);
            d.extend_from_slice(&id);
            d.extend_from_slice(&((n - 1 - i) as u16).to_be_bytes());
            d.extend_from_slice(&(body.len() as u16).to_be_bytes());
            d.extend_from_slice(body);
            d
        })
        .collect())
}

/// The outputs carrying one piece from `pk`'s P2PKH address (publisher.payload_outputs).
pub fn piece_outputs(p: &ChainParams, pk: &PublicKey, piece: &[u8]) -> Result<Vec<TxOutSpec>, String> {
    arcade_wallet::tx::arcade_outputs(p, &arcade_wallet::tx::any_data(piece), pk, CLASS_C_MAX).map_err(|e| e.to_string())
}

// ---------------- the chain, the key, the coins ----------------

fn is_arcade(p: &ChainParams) -> bool {
    crate::arcsvc::defined().is_some_and(|d| d.chain_no == p.chain_no)
}

/// The chain asked for (kept here or borrowed), else the ARCADE chain when it is installed or borrowed, else the
/// home chain.
fn chain_for(node: &Node, chain: Option<&str>) -> Result<&'static ChainParams, String> {
    match chain.map(str::trim).filter(|c| !c.is_empty()) {
        Some(k) => {
            if let Some(p) = crate::arcsvc::usable_chain().filter(|p| p.key.eq_ignore_ascii_case(k)) {
                return Ok(p);
            }
            crate::sitesvc::chain_or_borrowed(node, &json!({"chain": k})).map(|(p, _)| p)
        }
        None => Ok(crate::arcsvc::usable_chain().unwrap_or(node.params)),
    }
}

/// The key that signs on `p`: the one holding the @tag `"as"` names (the wallet's own, or a legacy wallet imported for
/// that chain), else the wallet's tagged key there, else its first. (address, tag, key)
fn signer(node: &Node, p: &'static ChainParams) -> Result<(String, Option<String>, SecretKey), String> {
    if is_arcade(p) {
        return crate::arcledger::sender(node, p);
    }
    let tag_of = |a: &str| crate::hometag::of_address_on(node, p, a);
    let keys = node.app.tag_keys(p.chain_no).map_err(|e| e.to_string())?;
    if let Some(want) = crate::arcledger::sign_as() {
        for k in &keys {
            if k.tag.as_deref() == Some(want.as_str()) || tag_of(&k.address).as_deref() == Some(want.as_str()) {
                let (k, sk) = node.app.tag_key(p, k.index).map_err(|e| e.to_string())?;
                return Ok((k.address, Some(want), sk));
            }
        }
        for (address, _, sk, kind) in crate::importsvc::wallets_on(node, p.key) {
            if kind == "legacy" && tag_of(&address).as_deref() == Some(want.as_str()) {
                return Ok((address, Some(want), sk));
            }
        }
        return Err(format!("no key here holds @{want} on {} (import it first: wallet.import with chain {})", p.name, p.key));
    }
    for k in &keys {
        if let Some(t) = k.tag.clone().or_else(|| tag_of(&k.address)) {
            let (k, sk) = node.app.tag_key(p, k.index).map_err(|e| e.to_string())?;
            return Ok((k.address, Some(t), sk));
        }
    }
    let (k, sk) = node.app.tag_key(p, 0).map_err(|e| e.to_string())?;
    Ok((k.address, None, sk))
}

/// The fee rate and dust limit on `p`, as `tx.fee` names them (what the tool is told).
fn fees(p: &ChainParams) -> (u64, u64) {
    let f = crate::coinsend::fee_policy(p);
    (f.fallback_per_kb.max(f.floor_per_kb), f.dust)
}

/// The wallet's spendable P2PKH coins of `pk` on `p`: confirmed, and on the ARCADE chain its change still waiting
/// too; none spent by a waiting transaction, none carrying an inscription, none of a split an unfinished
/// publication still needs.
fn wallet_coins(node: &Node, p: &'static ChainParams, pk: &PublicKey) -> Vec<Utxo> {
    let script = arcade_wallet::keys::p2pkh_script_for(pk);
    let coins = if is_arcade(p) {
        let mut c = crate::claims::arc_coins(node, p, &script);
        c.extend(crate::arcsvc::waiting_outputs(&script).into_iter().map(|(op, value)| Utxo { outpoint: op, value, script: script.clone() }));
        crate::ordsafe::keep_free(node, p, c)
    } else {
        crate::swaps::coins(node, p, pk)
    };
    let held = reserved(node);
    coins.into_iter().filter(|u| u.script == script && !held.contains(&u.outpoint.txid.to_hex())).collect()
}

/// Out to the network, whichever way `p` is kept here.
fn broadcast(node: &Node, p: &'static ChainParams, tx: &Tx) -> Result<(), String> {
    if is_arcade(p) {
        crate::ownwait::send_arc(node, tx).map(|_| ())
    } else {
        crate::swaps::send(node, p, tx).map(|_| ())
    }
}

/// Whether the chain knows a transaction we signed (waiting or in a block).
fn known(node: &Node, p: &'static ChainParams, raw_hex: &str) -> bool {
    let Some(tx) = hex::decode(raw_hex).ok().and_then(|r| Tx::parse_with(&r, &p.format).ok()) else { return false };
    crate::ownwait::where_is(node, p, &tx) != crate::ownwait::Where::Neither
}

fn depth(node: &Node, p: &'static ChainParams, txid: &str) -> u32 {
    Hash256::from_hex(txid).map(|h| crate::swaps::depth(node, p, &h)).unwrap_or(0)
}

/// What `tx` paid the network: its inputs (`ins`) less its outputs.
fn fee_of(tx: &Tx, ins: &[Utxo]) -> u64 {
    ins.iter().map(|u| u.value).sum::<u64>().saturating_sub(tx.outputs.iter().map(|o| o.value).sum())
}

fn sign(p: &ChainParams, sk: &SecretKey, ins: &[Utxo], outs: &[TxOutSpec]) -> Result<Tx, String> {
    let tx = arcade_wallet::tx::sign_p2pkh(p, ins, outs, sk, arcade_app::now() as u32);
    if !(0..ins.len()).all(|i| arcade_wallet::tx::verify_p2pkh_input(p, &tx, ins, i)) {
        return Err("a transaction signed here didn't verify; nothing was sent".into());
    }
    Ok(tx)
}

// ---------------- the journal ----------------

const JOURNAL: &str = "apppub:";

fn jget(node: &Node, key: &str) -> Option<Value> {
    node.app.store.read(|r| r.get(arcade_app::META, format!("{JOURNAL}{key}").as_bytes())).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok())
}

fn jput(node: &Node, j: &Value) -> Result<(), String> {
    let key = format!("{JOURNAL}{}", j["key"].as_str().unwrap_or(""));
    node.app.store.write(|w| w.put(arcade_app::META, key.as_bytes(), j.to_string().as_bytes())).map_err(|e| format!("the publication's journal couldn't be written ({e}); nothing was sent"))
}

/// Splits that unfinished publications still need (their outputs pay their pieces): never spent for anything else.
fn reserved(node: &Node) -> BTreeSet<String> {
    node.app.store.read(|r| r.scan_prefix(arcade_app::META, JOURNAL.as_bytes())).unwrap_or_default().into_iter()
        .filter_map(|(_, v)| serde_json::from_slice::<Value>(&v).ok())
        .filter(|j| j["done"] != true)
        .filter_map(|j| j["split"].as_str().filter(|s| !s.is_empty()).map(String::from))
        .collect()
}

/// The journal's key: the client_ref, else the content, chain, type and card (the same file with another card is
/// another publication; the tool keys by the file alone).
fn journal_key(client_ref: Option<&str>, sha: &str, chain: &str, ctype: &str, json: &str) -> String {
    match client_ref {
        Some(r) => format!("ref:{r}"),
        None => format!("{sha}:{chain}:{}", hex::encode(&Sha256::digest(format!("{ctype}\n{json}").as_bytes())[..8])),
    }
}

/// Publications running now (journal keys): a second call for one waits its turn by being told to ask again.
static BUSY: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

struct Busy(String);
impl Drop for Busy {
    fn drop(&mut self) {
        BUSY.lock().unwrap().remove(&self.0);
    }
}
fn busy(key: &str) -> Option<Busy> {
    BUSY.lock().unwrap().insert(key.to_string()).then(|| Busy(key.to_string()))
}

// ---------------- apps.publish ----------------

/// One publication asked for: what to inscribe, where, and as whom.
#[derive(Clone)]
pub struct Ask {
    pub content: Vec<u8>,
    pub name: String,
    pub ctype: String,
    pub json: String,
    pub chain: Option<String>,
    pub client_ref: Option<String>,
    pub send: bool,
}

/// `apps.publish` from a socket request: `file` (a path on this computer; never over the web API) or `content_b64`.
pub fn publish_req(node: &Node, req: &Value) -> Result<Value, String> {
    use base64::Engine;
    let name = req["name"].as_str().unwrap_or("").trim().to_string();
    let file = req["file"].as_str().filter(|f| !f.trim().is_empty());
    let b64 = req["content_b64"].as_str().filter(|f| !f.trim().is_empty());
    let content = match (file, b64) {
        (Some(_), Some(_)) => return Err("give file or content_b64, not both".into()),
        (None, None) => return Err("file or content_b64 required".into()),
        (Some(_), None) if req["_web"] == true => return Err("file (a path on this computer) is not allowed over the web API: send content_b64".into()),
        (Some(f), None) => {
            let meta = std::fs::metadata(f).map_err(|e| format!("cannot read {f}: {e}"))?;
            if meta.len() as usize > MAX_BYTES {
                return Err(format!("{f} is {} bytes; at most {MAX_BYTES} are published at once", meta.len()));
            }
            std::fs::read(f).map_err(|e| format!("cannot read {f}: {e}"))?
        }
        (None, Some(b)) => base64::engine::general_purpose::STANDARD.decode(b.trim()).map_err(|e| format!("content_b64 is not base64: {e}"))?,
    };
    let name = if name.is_empty() { file.and_then(|f| std::path::Path::new(f).file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default() } else { name };
    if name.is_empty() {
        return Err("name required (the file's name, e.g. index.html)".into());
    }
    let ask = Ask {
        ctype: content_type(req["type"].as_str().unwrap_or(""), &name),
        json: card_text(&req["card"])?,
        content,
        name,
        chain: req["chain"].as_str().map(String::from),
        client_ref: crate::receipts::client_ref(req)?,
        send: req["send"] == true,
    };
    publish(node, &ask)
}

/// Plan, or (with send) sign and send, one publication. See the module notes.
pub fn publish(node: &Node, ask: &Ask) -> Result<Value, String> {
    if ask.content.is_empty() {
        return Err("there is nothing to inscribe".into());
    }
    if ask.content.len() > MAX_BYTES {
        return Err(format!("that is {} bytes; at most {MAX_BYTES} are published at once", ask.content.len()));
    }
    let p = chain_for(node, ask.chain.as_deref())?;
    let (address, tag, sk) = signer(node, p)?;
    let pk = PublicKey::from_secret_key(SECP256K1, &sk);
    let sha = hex::encode(Sha256::digest(&ask.content));
    let key = journal_key(ask.client_ref.as_deref(), &sha, p.key, &ask.ctype, &ask.json);
    let json_sha = hex::encode(Sha256::digest(ask.json.as_bytes()));
    let mut j = match jget(node, &key) {
        Some(j) => {
            if j["sha256"] != sha.as_str() || j["chain"] != p.key || j["content_type"] != ask.ctype.as_str() || j["json_sha256"] != json_sha.as_str() {
                return Err(format!("client_ref {} was used for another publication (another file, type, card or chain): use a new client_ref", ask.client_ref.as_deref().unwrap_or("")));
            }
            if j["address"] != address.as_str() {
                return Err(format!("this publication was begun from {} and the key signing now is {address}: ask with the same \"as\" as before", j["address"].as_str().unwrap_or("")));
            }
            j
        }
        None => json!({"key": key, "sha256": sha, "chain": p.key, "content_type": ask.ctype, "json_sha256": json_sha, "name": ask.name,
                       "address": address, "tag": tag, "id8": hex::encode(arcade_mesh::random::bytes::<8>()), "split": "", "piece": 0,
                       "pieces": {}, "sending": {}, "txids": [], "fee": 0, "done": false, "at": arcade_app::now()}),
    };
    let id: [u8; 8] = hex::decode(j["id8"].as_str().unwrap_or("")).ok().and_then(|b| b.try_into().ok()).ok_or("the journal's inscription id is unreadable")?;
    let pieces = chunks(&ask.content, &ask.ctype, &ask.json, id)?;
    let outs: Vec<Vec<TxOutSpec>> = pieces.iter().map(|c| piece_outputs(p, &pk, c)).collect::<Result<_, _>>()?;
    let (rate, dust) = fees(p);
    let fee1 = |o: &[TxOutSpec]| {
        let lens: Vec<usize> = o.iter().map(|x| x.script.len()).chain(std::iter::once(25)).collect();
        arcade_wallet::tx::fee_for_size(p, arcade_wallet::tx::estimate_size(1, &lens) as u64, rate)
    };
    let locked: u64 = outs.iter().flatten().map(|o| o.value).sum();
    let piece_fees: u64 = outs.iter().map(|o| fee1(o)).sum();
    // What each piece's own coin holds: its outputs and fee, and enough change back that it is no dust (on the
    // Dogecoin family, none under 0.01, which peers charge for).
    let margin = if crate::swaps::soft_dust(p) { dust.max(crate::swaps::SOFT_DUST) } else { dust };
    let piece_value = outs.iter().map(|o| o.iter().map(|x| x.value).sum::<u64>() + fee1(o)).max().unwrap_or(0) + margin;
    let base = json!({"chain": p.key, "address": address, "tag": tag, "name": ask.name, "content_type": ask.ctype, "bytes": ask.content.len(),
                      "sha256": sha, "chunks": pieces.len(), "transactions": pieces.len() + usize::from(pieces.len() > 1)});

    if j["done"] == true {
        let mut v = base;
        v["id"] = j["txids"][0].clone();
        v["txids"] = j["txids"].clone();
        v["fee"] = j["fee"].clone();
        v["resumed"] = json!(true);
        v["state"] = json!("sent");
        v["note"] = json!("already published: nothing was sent again");
        return Ok(v);
    }

    if !ask.send {
        return Ok(plan_answer(node, p, &pk, &j, base, &outs, piece_value, piece_fees, locked, rate, dust));
    }
    let _guard = busy(&key).ok_or("this publication is being sent right now: ask again in a moment")?;
    // Read again under the guard: another call may have finished part of it meanwhile.
    if let Some(now) = jget(node, &key) {
        j = now;
    }
    jput(node, &j)?;
    let resumed = j["split"].as_str().is_some_and(|s| !s.is_empty()) || j["sending"].as_object().is_some_and(|m| !m.is_empty());
    let mut v = run(node, p, &sk, &pk, &mut j, &pieces, &outs, piece_value, rate, dust, ask)?;
    let mut out = base;
    for (k, x) in v.as_object_mut().map(std::mem::take).unwrap_or_default() {
        out[k] = x;
    }
    out["resumed"] = json!(resumed);
    Ok(out)
}

/// The plan without sending: transactions, fee, value kept in data outputs, and whether the wallet holds enough.
#[allow(clippy::too_many_arguments)]
fn plan_answer(node: &Node, p: &'static ChainParams, pk: &PublicKey, j: &Value, mut v: Value, outs: &[Vec<TxOutSpec>], piece_value: u64, piece_fees: u64, locked: u64, rate: u64, dust: u64) -> Value {
    let coins = wallet_coins(node, p, pk);
    let balance: u64 = coins.iter().map(|c| c.value).sum();
    let done = j["pieces"].as_object().map(|m| m.len()).unwrap_or(0);
    let split_done = j["split"].as_str().is_some_and(|s| !s.is_empty());
    // The first transaction: the single one, or the split (one output per piece), paid from the wallet's coins.
    let (first_outs, rest_fee) = if outs.len() == 1 {
        (outs[0].clone(), 0)
    } else {
        (vec![TxOutSpec { value: piece_value, script: arcade_wallet::keys::p2pkh_script_for(pk) }; outs.len()], piece_fees)
    };
    let need_first = !(outs.len() > 1 && split_done);
    let (fee, enough, short) = if !need_first {
        (rest_fee, true, Value::Null)
    } else {
        match arcade_wallet::tx::plan_spend(p, first_outs.clone(), pk, &coins, rate, dust) {
            Ok((ins, o)) => {
                let f = ins.iter().map(|u| u.value).sum::<u64>().saturating_sub(o.iter().map(|x| x.value).sum());
                (f + rest_fee, true, Value::Null)
            }
            Err(e) => {
                let lens: Vec<usize> = first_outs.iter().map(|x| x.script.len()).chain(std::iter::once(25)).collect();
                let f = arcade_wallet::tx::fee_for_size(p, arcade_wallet::tx::estimate_size(1, &lens) as u64, rate);
                let need = first_outs.iter().map(|x| x.value).sum::<u64>() + f;
                (f + rest_fee, false, json!(format!("not enough coins on {} at {}: {} here, about {} needed ({e})", p.name, v["address"].as_str().unwrap_or(""),
                    arcade_ledger::amount::trimmed8(balance), arcade_ledger::amount::trimmed8(need))))
            }
        }
    };
    v["fee"] = json!(fee);
    v["locked"] = json!(locked);
    v["cost"] = json!(fee + locked);
    v["piece_value"] = json!(if outs.len() > 1 { piece_value } else { 0 });
    v["balance"] = json!(balance);
    v["enough"] = json!(enough);
    v["short"] = short;
    v["already_sent"] = json!(done + usize::from(split_done));
    v["sent"] = json!(false);
    v["state"] = json!("plan");
    v["note"] = json!("nothing was sent: send: true signs and broadcasts; locked is the 0.01 in each data output, spendable again by this key");
    v
}

/// A transaction noted under `slot` before it was broadcast: Some(txid) when the chain has it, or took the same
/// bytes again now.
fn prior(node: &Node, p: &'static ChainParams, j: &Value, slot: &str) -> Option<String> {
    let s = &j["sending"][slot];
    let (txid, raw) = (s["txid"].as_str()?, s["raw"].as_str()?);
    if known(node, p, raw) {
        return Some(txid.to_string());
    }
    // Not known: the same bytes again (the same coins, so they are taken at most once).
    let tx = Tx::parse_with(&hex::decode(raw).ok()?, &p.format).ok()?;
    broadcast(node, p, &tx).ok().map(|_| txid.to_string())
}

/// Note `tx` under `slot` (its txid and bytes), then broadcast it. Its fee is added to the journal's.
fn send_noted(node: &Node, p: &'static ChainParams, j: &mut Value, slot: &str, tx: &Tx, fee: u64) -> Result<String, String> {
    let txid = tx.txid.to_hex();
    j["sending"][slot] = json!({"txid": txid, "raw": hex::encode(&tx.raw), "fee": fee});
    jput(node, j)?;
    broadcast(node, p, tx)?;
    j["fee"] = json!(j["fee"].as_u64().unwrap_or(0) + fee);
    jput(node, j)?;
    Ok(txid)
}

#[allow(clippy::too_many_arguments)]
fn run(node: &Node, p: &'static ChainParams, sk: &SecretKey, pk: &PublicKey, j: &mut Value, pieces: &[Vec<u8>], outs: &[Vec<TxOutSpec>],
       piece_value: u64, rate: u64, dust: u64, ask: &Ask) -> Result<Value, String> {
    let from = j["address"].as_str().unwrap_or("").to_string();
    let what = |e: arcade_wallet::tx::BuildError| match e {
        arcade_wallet::tx::BuildError::InsufficientFunds { have, need } => format!(
            "not enough coins on {} at {}: {} here, {} needed (fee and 0.01 in each data output); nothing was sent",
            p.name, from, arcade_ledger::amount::trimmed8(have), arcade_ledger::amount::trimmed8(need)),
        e => format!("could not build the transaction: {e}"),
    };
    let finish = |node: &Node, j: &mut Value, txids: Vec<String>| -> Result<Value, String> {
        j["txids"] = json!(txids);
        j["done"] = json!(true);
        jput(node, j)?;
        let _ = node.app.event("apps", &format!("published {} ({} bytes) on {} in {} piece{}: {}", ask.name, ask.content.len(), p.name, txids.len(), if txids.len() == 1 { "" } else { "s" }, txids[0]));
        Ok(json!({"id": txids[0], "txids": txids, "fee": j["fee"], "state": "sent", "sent": true}))
    };
    if pieces.len() == 1 {
        let txid = match prior(node, p, j, "single") {
            Some(t) => t,
            None => {
                let coins = wallet_coins(node, p, pk);
                let (ins, o) = arcade_wallet::tx::plan_spend(p, outs[0].clone(), pk, &coins, rate, dust).map_err(what)?;
                let tx = sign(p, sk, &ins, &o)?;
                send_noted(node, p, j, "single", &tx, fee_of(&tx, &ins))?
            }
        };
        return finish(node, j, vec![txid]);
    }
    let script = arcade_wallet::keys::p2pkh_script_for(pk);
    // The split: one output per piece, each worth what its piece needs.
    let split = match j["split"].as_str().filter(|s| !s.is_empty()).map(String::from).filter(|_| prior(node, p, j, "split").is_some()) {
        Some(s) => s,
        None => {
            let coins = wallet_coins(node, p, pk);
            let o = vec![TxOutSpec { value: piece_value, script: script.clone() }; pieces.len()];
            let (ins, o) = arcade_wallet::tx::plan_spend(p, o, pk, &coins, rate, dust).map_err(what)?;
            let tx = sign(p, sk, &ins, &o)?;
            j["piece"] = json!(piece_value);
            j["pieces"] = json!({});
            let txid = send_noted(node, p, j, "split", &tx, fee_of(&tx, &ins))?;
            j["split"] = json!(txid);
            jput(node, j)?;
            txid
        }
    };
    let piece_value = j["piece"].as_u64().unwrap_or(piece_value);
    // A coin chain: the pieces once the split is in a block (peers drop a child whose parent they haven't seen); the
    // rest is sent in the background meanwhile, and a call again finishes it too.
    if !is_arcade(p) && depth(node, p, &split) < 1 {
        wait_in_background(node, ask, crate::arcledger::sign_as());
        return Ok(json!({"id": Value::Null, "txid": split, "split": split, "txids": [], "fee": j["fee"], "state": "waiting", "sent": true,
                         "note": "the split is sent; its pieces go out once it is in a block (in the background, or ask again with the same client_ref)"}));
    }
    let split_h = Hash256::from_hex(&split).map_err(|_| "the journal's split is unreadable")?;
    let mut txids = Vec::new();
    for (n, o) in outs.iter().enumerate() {
        if let Some(t) = j["pieces"][n.to_string()].as_str() {
            txids.push(t.to_string());
            continue;
        }
        let slot = format!("piece {n} of {split}");
        let txid = match prior(node, p, j, &slot) {
            Some(t) => t,
            None => {
                let coin = Utxo { outpoint: OutPoint { txid: split_h, vout: n as u32 }, value: piece_value, script: script.clone() };
                let (ins, o2) = arcade_wallet::tx::plan_spend(p, o.clone(), pk, std::slice::from_ref(&coin), rate, dust).map_err(what)?;
                let tx = sign(p, sk, &ins, &o2)?;
                match send_noted(node, p, j, &slot, &tx, fee_of(&tx, &ins)) {
                    Ok(t) => t,
                    Err(e) => {
                        // Refused: its coin is spent by another sender (the tool's case of 2026-10-19). Paid from
                        // the wallet's other coins instead, never the split's.
                        let coins: Vec<Utxo> = wallet_coins(node, p, pk).into_iter().filter(|u| u.outpoint.txid != split_h).collect();
                        let (ins, o2) = arcade_wallet::tx::plan_spend(p, o.clone(), pk, &coins, rate, dust)
                            .map_err(|b| format!("piece {} of {} could not be sent ({e}), nor paid otherwise ({}); ask again with the same client_ref to finish", n + 1, outs.len(), what(b)))?;
                        let tx = sign(p, sk, &ins, &o2)?;
                        send_noted(node, p, j, &format!("{slot} again"), &tx, fee_of(&tx, &ins))
                            .map_err(|e2| format!("piece {} of {} could not be sent ({e2}); {} of them are out: ask again with the same client_ref to finish", n + 1, outs.len(), txids.len()))?
                    }
                }
            }
        };
        j["pieces"][n.to_string()] = json!(txid);
        jput(node, j)?;
        txids.push(txid);
    }
    finish(node, j, txids)
}

/// A publication whose split waits for a block (a coin chain): once it has one, the pieces go out from here.
/// Publications waiting so (journal keys), one thread each.
static WAITING: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

fn wait_in_background(node: &Node, ask: &Ask, as_tag: Option<String>) {
    let Ok(p) = chain_for(node, ask.chain.as_deref()) else { return };
    let sha = hex::encode(Sha256::digest(&ask.content));
    let key = journal_key(ask.client_ref.as_deref(), &sha, p.key, &ask.ctype, &ask.json);
    if !WAITING.lock().unwrap().insert(key.clone()) {
        return;
    }
    let n = node.self_arc();
    let ask = Ask { send: true, ..ask.clone() };
    std::thread::spawn(move || {
        let until = arcade_app::now() + WAIT_SPLIT_SECS;
        while arcade_app::now() < until && !crate::STOP_ASKED.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_secs(15));
            let Some(j) = jget(&n, &key) else { break };
            if j["done"] == true {
                break;
            }
            if depth(&n, p, j["split"].as_str().unwrap_or("")) < 1 {
                continue;
            }
            match crate::arcledger::with_sign_as(as_tag.clone(), || publish(&n, &ask)) {
                Ok(v) if v["state"] == "sent" => break,
                Ok(_) => continue,
                Err(e) if e.contains("being sent right now") => continue,
                Err(e) => {
                    let _ = n.app.event("apps", &format!("publishing {} stopped: {e}", ask.name));
                    break;
                }
            }
        }
        WAITING.lock().unwrap().remove(&key);
    });
}

// ---------------- apps.withdraw ----------------

/// The page a withdrawal card is inscribed beside: a few words, so the version that opens says what happened.
fn withdrawn_page(name: &str, reason: &str) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    format!("<!doctype html><meta charset=\"utf-8\"><title>{} (withdrawn)</title><p>Withdrawn by its publisher.{}</p>\n", esc(name),
            if reason.is_empty() { String::new() } else { format!(" {}", esc(reason)) })
}

/// The withdrawal card for an app whose newest card is `card` (its own fields, so the store files it under the same
/// app), as `arcade-publish --json` takes it too.
pub fn withdrawal_card(card: &arcade_ledger::cards::Card, reason: &str, replaced_by: &str, versions: &[String]) -> Value {
    let mut a = json!({"name": card.name, "withdrawn": true});
    if !card.family.is_empty() {
        a["family"] = json!(card.family);
    }
    if !card.category.is_empty() {
        a["category"] = json!(card.category);
    }
    if !reason.trim().is_empty() {
        a["reason"] = json!(reason.trim());
    }
    if !replaced_by.trim().is_empty() {
        a["replaced_by"] = json!(replaced_by.trim());
    }
    if !versions.is_empty() {
        a["versions"] = json!(versions);
    }
    json!({"app": a})
}

/// `apps.withdraw`: a new version of one of this wallet's apps whose card says it is withdrawn (the whole app, or the
/// versions listed), published as `apps.publish` publishes. Only the app's publisher may: its creator must be the key
/// that signs here.
pub fn withdraw_req(node: &Node, req: &Value) -> Result<Value, String> {
    let p = chain_for(node, req["chain"].as_str())?;
    let (address, _, _) = signer(node, p)?;
    let txid = req["txid"].as_str().map(|t| t.trim().to_lowercase()).filter(|t| !t.is_empty());
    let family = req["family"].as_str().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    if txid.is_none() && family.is_none() {
        return Err("txid (any version of the app) or family required".into());
    }
    let found = crate::appsvc::lookup(node, p, txid.as_deref(), family.as_deref());
    if found.is_empty() {
        return Err(format!("no such app on {} here", p.name));
    }
    let Some((card, txids)) = found.into_iter().find(|(c, _, _)| *c == address).map(|(_, card, t)| (card, t)) else {
        return Err(format!("only its publisher can withdraw it: this wallet's key on {} ({address}) did not publish it", p.name));
    };
    let versions: Vec<String> = req["versions"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(|s| s.trim().to_lowercase())).collect()).unwrap_or_default();
    if let Some(bad) = versions.iter().find(|v| !txids.contains(*v)) {
        return Err(format!("{bad} is not a version of {} here", card.name));
    }
    let reason = req["reason"].as_str().unwrap_or("");
    let replaced_by = req["replaced_by"].as_str().unwrap_or("");
    let w = withdrawal_card(&card, reason, replaced_by, &versions);
    let ask = Ask {
        content: withdrawn_page(&card.name, reason).into_bytes(),
        name: "withdrawn.html".into(),
        ctype: "text/html;charset=utf-8".into(),
        json: w.to_string(),
        chain: Some(p.key.to_string()),
        client_ref: crate::receipts::client_ref(req)?,
        send: req["send"] == true,
    };
    let mut v = publish(node, &ask)?;
    v["withdrawal_card"] = w;
    v["app"] = json!(card.name);
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(n: usize) -> Vec<u8> {
        (0..n).map(|i| ((i * 7 + 3) % 251) as u8).collect()
    }
    const CARD: &str = r#"{"app":{"name":"Test App","family":"test-app","version":"1.0"}}"#;
    const ID: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];

    /// The pieces arcade-publish makes of the same file, card and id (its inscribe.plan, run 2026-11-21 with
    /// scratch vectors.py): count, first lengths and the sha256 of every piece joined. Byte for byte.
    #[test]
    fn pieces_match_the_python_tool() {
        let ct = content_type("text/html;charset=utf-8", "x.html");
        assert_eq!(ct, "text/html;charset=utf-8");
        let want: &[(usize, usize, &[usize], &str)] = &[
            (10, 1, &[153], "a17d2e37f3209ed1ade7b0133d2152bd3808ff0ab9e781794ca8b00a0abfbc3e"),
            (7000, 1, &[7143], "37a65085eb1123fc5e944fb073bd7ae56350e6e151f89e437da38d653a6819f4"),
            (7505, 2, &[3833, 3833], "dbc155b498df4e17d977a5df4c590a008e36fdf1655fc7fa9fcc388be482b68f"),
            (7506, 2, &[3834, 3833], "c5da045c459f4b89a9fa91802f5cc021d56f3364a151929d0b5e8d294ff10d3f"),
            (7589, 2, &[3875, 3875], "d33fc2f4003a72c23de7a7e4abc0144562d932356bb5a1be88e8e071bc3f2c0e"),
            (7590, 2, &[3876, 3875], "a6850e4d3ebd8f30860754558443c6ca4d6ddcac5be2d7bc5a230ed6455230b2"),
            (20000, 3, &[6727, 6727, 6725], "9a5a3c9f571402333166af7a5e8038d3866300c6ebf402352cde4ad6c728c155"),
            (100000, 14, &[7170, 7170, 7170, 7170], "e502a26a6f3896337fc7da89cc4bcbee95985224a1af0bb2a3bb67cd8d5520c7"),
        ];
        for (n, count, lens, sha) in want {
            let c = chunks(&content(*n), &ct, CARD, ID).unwrap();
            assert_eq!(c.len(), *count, "{n} bytes");
            assert_eq!(&c.iter().take(lens.len()).map(|x| x.len()).collect::<Vec<_>>(), lens, "{n} bytes");
            assert_eq!(hex::encode(Sha256::digest(c.concat())), *sha, "{n} bytes");
            assert!(c.iter().all(|x| x.len() <= CAPACITY), "every piece fits one Class B transaction");
        }
    }

    /// The boundary: the largest file one piece holds, and one byte more is two even pieces.
    #[test]
    fn chunking_boundaries() {
        let ct = "text/html;charset=utf-8";
        let manifest = 4 + 32 + 1 + ct.len() + 2 + CARD.len();
        let one = CAPACITY - HEADER - manifest;
        assert_eq!(chunks(&content(one), ct, CARD, ID).unwrap().len(), 1);
        let two = chunks(&content(one + 1), ct, CARD, ID).unwrap();
        assert_eq!(two.len(), 2);
        assert!(two[0].len().abs_diff(two[1].len()) <= 1, "split evenly, not greedily");
        // Countdowns: the first piece the highest, the last 0; the card in the first piece's manifest.
        let c = chunks(&content(30_000), ct, CARD, ID).unwrap();
        for (i, x) in c.iter().enumerate() {
            assert_eq!(&x[..6], b"INSC\x01\x01");
            assert_eq!(&x[6..14], &ID);
            assert_eq!(u16::from_be_bytes([x[14], x[15]]) as usize, c.len() - 1 - i);
            assert_eq!(u16::from_be_bytes([x[16], x[17]]) as usize, x.len() - HEADER);
        }
        let first = &c[0][HEADER..];
        assert_eq!(&first[37..37 + ct.len()], ct.as_bytes());
        let at = 37 + ct.len();
        let jl = u16::from_be_bytes([first[at], first[at + 1]]) as usize;
        assert_eq!(&first[at + 2..at + 2 + jl], CARD.as_bytes(), "the card as given");
        assert!(chunks(b"", ct, CARD, ID).is_err());
    }

    /// The outputs of one piece equal the tool's with its free byte searched from 0 (encode_class_b rng=b"\0"):
    /// secret key 1 on arctest, the marker first, then 120 multisig outputs.
    #[test]
    fn class_b_outputs_match_the_python_tool() {
        let p = arcade_arc::store::chain_params(&arcade_arc::params::TESTNET, &Hash256::ZERO);
        let sk = SecretKey::from_slice(&[[0u8; 31].as_slice(), &[1]].concat()).unwrap();
        let pk = PublicKey::from_secret_key(SECP256K1, &sk);
        assert_eq!(arcade_wallet::keys::p2pkh_address(&pk, p.p2pkh_version), "aajLHAdXJ3KV2HmBFbkn5G1HMypr6qWRjg");
        let c = chunks(&content(7000), "text/html;charset=utf-8", CARD, ID).unwrap();
        let o = piece_outputs(p, &pk, &c[0]).unwrap();
        assert_eq!(o.len(), 121);
        let all: Vec<u8> = o.iter().flat_map(|x| x.script.clone()).collect();
        assert_eq!(hex::encode(Sha256::digest(&all)), "60024713a5d7bf118b818ceb27fbe472bb88803991143a66278c1bbd74cffb81");
        assert!(o.iter().all(|x| x.value == 1_000_000), "0.01 in each, as the tool pays");
    }

    /// Built, signed and read back by arcaded's own reader (arcade_ledger::extract) and the ledger's manifest
    /// parser: the same content, type, card and sender.
    #[test]
    fn a_published_page_reads_back() {
        let p = arcade_arc::store::chain_params(&arcade_arc::params::TESTNET, &Hash256::ZERO);
        let sk = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let pk = PublicKey::from_secret_key(SECP256K1, &sk);
        let script = arcade_wallet::keys::p2pkh_script_for(&pk);
        let me = arcade_wallet::keys::p2pkh_address(&pk, p.p2pkh_version);
        let page = content(16_000);
        let ct = content_type("", "index.html");
        assert_eq!(ct, "text/html");
        let pieces = chunks(&page, &ct, CARD, ID).unwrap();
        assert_eq!(pieces.len(), 3);
        let mut stream: Vec<(u16, Vec<u8>)> = Vec::new();
        for (n, c) in pieces.iter().enumerate() {
            let coin = Utxo { outpoint: OutPoint { txid: Hash256([n as u8 + 1; 32]), vout: 0 }, value: 5 * 100_000_000, script: script.clone() };
            let (ins, o) = arcade_wallet::tx::plan_spend(p, piece_outputs(p, &pk, c).unwrap(), &pk, std::slice::from_ref(&coin), 200_000, 100_000).unwrap();
            let tx = sign(p, &sk, &ins, &o).unwrap();
            let prev = vec![arcade_ledger::extract::Prevout { value: coin.value, script: script.clone() }];
            let got = arcade_ledger::extract::extract(&tx, &prev, p).unwrap().unwrap();
            assert_eq!(got.sender, me);
            assert_eq!(&got.payload[..4], &[0, 0, 0, 200], "AnyData");
            let d = &got.payload[4..];
            let clen = u16::from_be_bytes([d[16], d[17]]) as usize;
            stream.push((u16::from_be_bytes([d[14], d[15]]), d[18..18 + clen].to_vec()));
        }
        stream.sort_by(|a, b| b.0.cmp(&a.0));
        let joined: Vec<u8> = stream.into_iter().flat_map(|x| x.1).collect();
        let m = arcade_ledger::engine::manifest(&joined).unwrap();
        assert_eq!(m.ctype, ct);
        assert_eq!(m.json, CARD);
        assert_eq!(&m.content[..m.total as usize], &page[..]);
        assert_eq!(arcade_ledger::cards::parse(&m.json).unwrap().family, "test-app");
    }

    #[test]
    fn types_and_cards_as_the_tool_takes_them() {
        assert_eq!(standard_type("Text/JavaScript", ""), "application/javascript");
        assert_eq!(standard_type(" TEXT/HTML ;charset=utf-8", ""), "text/html;charset=utf-8");
        assert_eq!(content_type("", "game.js"), "application/javascript");
        assert_eq!(content_type("application/octet-stream", "x.mjs"), "application/javascript");
        assert_eq!(content_type("", "icon.png"), "image/png");
        assert_eq!(content_type("", "blob"), "application/octet-stream");
        assert_eq!(card_text(&json!("  {\"app\": {\"name\": \"A\"}} ")).unwrap(), "{\"app\": {\"name\": \"A\"}}", "a string as written");
        assert_eq!(card_text(&json!({"app": {"name": "A"}})).unwrap(), "{\"app\":{\"name\":\"A\"}}");
        assert_eq!(card_text(&Value::Null).unwrap(), "");
        assert!(card_text(&json!("{not json")).is_err());
    }

    #[test]
    fn journal_keys() {
        assert_eq!(journal_key(Some("r1"), "ab", "arctest", "t", "j"), "ref:r1");
        let a = journal_key(None, "ab", "arctest", "text/html", "{}");
        assert_ne!(a, journal_key(None, "ab", "arctest", "text/html", "{\"app\":{}}"), "another card is another publication");
        assert_ne!(a, journal_key(None, "ab", "peptest", "text/html", "{}"));
        assert_eq!(a, journal_key(None, "ab", "arctest", "text/html", "{}"));
    }

    #[test]
    fn a_withdrawal_card_files_under_the_same_app() {
        let card = arcade_ledger::cards::parse(r#"{"app":{"name":"Test Pond","family":"test-pond","category":"social","version":"2"}}"#).unwrap();
        let w = withdrawal_card(&card, "moved", "arctest:ab", &[]);
        let back = arcade_ledger::cards::parse(&w.to_string()).unwrap();
        assert_eq!(back.key(), card.key());
        assert_eq!(back.family, "test-pond");
        let wd = back.withdraw.unwrap();
        assert_eq!((wd.reason.as_str(), wd.replaced_by.as_str(), wd.versions.len()), ("moved", "arctest:ab", 0));
        assert!(withdrawn_page("<b>", "x & y").contains("&lt;b&gt;") && withdrawn_page("a", "x & y").contains("x &amp; y"));
    }
}
