//! Refereed results (the original arcade's referee.py, client side).
//!
//! A referee is one arcade, with a key, that runs a game's judge (an inscribed script) over a
//! player's run and signs what it found. A V1.0 arcade runs no judges: it asks the referee a pool
//! or page names, and it checks signed results itself.
//!
//! - **An attestation** is a referee's ECDSA (secp256k1, DER) signature over the SHA-256 of the
//!   canonical JSON `{"address","content","judge","referee","result","v":1}`: this result, by this
//!   judge, for this address, over this content (the sha256 of the inscription that carries it).
//!   The inscription carries it in its JSON record as `attested`.
//! - **`/r/verified/<judge>`** lists inscriptions carrying a result of that judge signed by a
//!   referee the caller trusts (`?referee=<key>`, else this arcade's referee setting), checked
//!   here: the signature, the content it names, and that the inscription's creator is the address
//!   it was given to. Newest first, `after` the last number seen.
//! - **The referee setting** (`referee.set`): the address of the referee arcade this one asks when
//!   a page names none, and whose key `/r/verified` trusts by default. Empty until the operator
//!   sets it: a V1.0 arcade is no referee of its own.

use crate::node::Node;
use arcade_store::KeyBuf;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Mutex;

pub const ATTESTATION_VERSION: u64 = 1;

/// The bytes a referee signs (referee.py attestation_message).
pub fn attestation_message(referee: &str, judge: &str, result: &Value, address: &str, content: &str) -> String {
    arcade_mesh::canon::canonical(&json!({"v": ATTESTATION_VERSION, "referee": referee.to_lowercase(), "judge": judge.to_lowercase(),
                                          "result": result, "address": address, "content": content.to_lowercase()}))
}

/// Whether `att` is a referee's signature over exactly its fields, and, when given, over this
/// content, for this address, by this referee (referee.py verify_attestation).
pub fn attestation_ok(att: &Value, content: Option<&str>, address: Option<&str>, referee: Option<&str>) -> bool {
    let (Some(pub_hex), Some(judge), Some(addr), Some(cont), Some(sig)) =
        (att["referee"].as_str(), att["judge"].as_str(), att["address"].as_str(), att["content"].as_str(), att["sig"].as_str())
    else {
        return false;
    };
    let pub_hex = pub_hex.to_lowercase();
    if referee.is_some_and(|r| r.to_lowercase() != pub_hex)
        || content.is_some_and(|c| c.to_lowercase() != cont.to_lowercase())
        || address.is_some_and(|a| a != addr)
        || att.get("result").is_none()
    {
        return false;
    }
    let msg = attestation_message(&pub_hex, judge, &att["result"], addr, cont);
    let digest: [u8; 32] = {
        use sha2::{Digest, Sha256};
        Sha256::digest(msg.as_bytes()).into()
    };
    use arcade_wallet::secp256k1::{ecdsa::Signature, Message, PublicKey, SECP256K1};
    let (Ok(pk_bytes), Ok(der)) = (hex::decode(&pub_hex), hex::decode(sig)) else { return false };
    let (Ok(pk), Ok(mut s)) = (PublicKey::from_slice(&pk_bytes), Signature::from_der(&der)) else { return false };
    s.normalize_s();
    SECP256K1.verify_ecdsa(&Message::from_digest(digest), &s, &pk).is_ok()
}

// ---------------- the referee setting ----------------

pub fn setting(node: &Node) -> Value {
    node.app.store.read(|r| r.get(arcade_app::META, b"referee")).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok()).unwrap_or(json!({}))
}

/// referee.set: the referee arcade's address (https://...), its key read from its /r/referee.
/// An empty address clears it.
pub fn set(node: &Node, url: &str) -> Result<Value, String> {
    let url = url.trim().trim_end_matches('/');
    let v = if url.is_empty() {
        json!({})
    } else {
        if !(url.starts_with("https://") || url.starts_with("http://127.0.0.1:")) || url.contains(['?', '#', ' ']) {
            return Err("a referee is named by its address, like https://example.org".into());
        }
        let said: Value = ureq::get(&format!("{url}/r/referee")).timeout(std::time::Duration::from_secs(15)).call().map_err(|e| format!("{url} did not answer: {e}"))?.into_json().map_err(|_| "that is no referee".to_string())?;
        let key = said["pubkey"].as_str().filter(|k| k.len() == 66 && hex::decode(k).is_ok()).ok_or("that arcade has no referee key")?;
        json!({"url": url, "pubkey": key.to_lowercase(), "engine": said["engine"]})
    };
    node.app.store.write(|w| w.put(arcade_app::META, b"referee", v.to_string().as_bytes())).map_err(|e| e.to_string())?;
    Ok(v)
}

// ---------------- /r/verified ----------------

#[derive(Clone)]
struct Found {
    judge: String,
    referee: String,
    number: u64,
    row: Value,
}

#[derive(Default)]
struct Scan {
    chain: u16,
    next: u64,
    found: Vec<Found>,
}

static SCAN: Mutex<Option<Scan>> = Mutex::new(None);

/// Read inscriptions numbered from where the last look stopped; keep the ones carrying a good
/// attestation. Each inscription is checked once (inscriptions do not change).
fn catch_up(node: &Node) -> Vec<Found> {
    let c = node.params.chain_no;
    let mut g = SCAN.lock().unwrap();
    let scan = g.get_or_insert_with(Scan::default);
    if scan.chain != c {
        *scan = Scan { chain: c, ..Scan::default() };
    }
    let mut misses = 0;
    let mut n = scan.next;
    // Up to 20000 a call, so a first look on a big chain is spread over a few requests.
    for _ in 0..20_000 {
        let Some(txid) = node.chain.store(c).read(|r| r.get(arcade_ledger::engine::BY_NUMBER, &KeyBuf::new().u16(c).u64(n).done())).ok().flatten() else {
            misses += 1;
            if misses > 3 {
                break;
            }
            n += 1;
            continue;
        };
        misses = 0;
        let txid = String::from_utf8_lossy(&txid).into_owned();
        n += 1;
        scan.next = n;
        let Some(row) = arcade_ledger::content::inscription(&node.chain, c, &txid) else { continue };
        let raw = row["json"].as_str().unwrap_or("");
        if !raw.contains("\"attested\"") {
            continue;
        }
        let Ok(said) = serde_json::from_str::<Value>(raw) else { continue };
        let att = &said["attested"];
        let (Some(judge), Some(creator), Some(sha)) = (att["judge"].as_str(), row["creator"].as_str(), row["sha256"].as_str()) else { continue };
        if !attestation_ok(att, Some(sha), Some(creator), None) {
            continue;
        }
        let mut rest = said.clone();
        if let Value::Object(m) = &mut rest {
            m.remove("attested");
        }
        scan.found.push(Found {
            judge: judge.to_lowercase(),
            referee: att["referee"].as_str().unwrap_or("").to_lowercase(),
            number: row["number"].as_u64().unwrap_or(0),
            row: json!({"id": row["txid"], "number": row["number"], "creator": creator, "block": row["block_height"], "result": att["result"], "json": rest}),
        });
    }
    scan.found.clone()
}

/// The judge an id names: an inscription that is JavaScript.
fn judge_row(node: &Node, key: &str) -> Result<Value, String> {
    let row = arcade_ledger::content::inscription(&node.chain, node.params.chain_no, key.trim_start_matches('#')).ok_or("there is no such judge")?;
    if !row["content_type"].as_str().unwrap_or("").contains("javascript") {
        return Err(format!("#{} is not JavaScript, so it cannot judge", row["number"]));
    }
    Ok(row)
}

/// GET /r/verified/<judge>?referee=&after=&limit= in the original's shape.
pub fn verified(node: &Node, judge: &str, referee: Option<&str>, after: i64, limit: u64) -> (u16, Value) {
    let row = match judge_row(node, judge) {
        Ok(r) => r,
        Err(e) => return (404, json!({"error": e})),
    };
    let judge_id = row["txid"].as_str().unwrap_or("").to_lowercase();
    let trusted = match referee.filter(|r| !r.is_empty()).map(str::to_lowercase).or_else(|| setting(node)["pubkey"].as_str().map(String::from)) {
        Some(k) => k,
        None => return (400, json!({"error": "name the referee to trust (?referee=<its key>): this arcade is no referee, and its operator has named none"})),
    };
    let mut rows: Vec<Found> = catch_up(node).into_iter().filter(|f| f.judge == judge_id && f.referee == trusted && (after < 0 || (f.number as i64) < after)).collect();
    rows.sort_by(|a, b| b.number.cmp(&a.number));
    rows.truncate(limit.clamp(1, 200) as usize);
    let mut tags: HashMap<String, Value> = HashMap::new();
    let out: Vec<Value> = rows
        .into_iter()
        .map(|f| {
            let mut r = f.row;
            let a = r["creator"].as_str().unwrap_or("").to_string();
            let tag = tags.entry(a.clone()).or_insert_with(|| json!(arcade_ledger::index::tag_of(&node.chain, node.params.chain_no, &a).ok().flatten())).clone();
            r["creatortag"] = tag;
            r
        })
        .collect();
    let next = out.last().map(|r| r["number"].clone()).unwrap_or(Value::Null);
    (200, json!({"judge": judge_id, "referee": trusted, "results": out, "next": next}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attestations_made_by_the_original_referee_check_here() {
        let v: Value = serde_json::from_str(include_str!("../tests/data/attestations.json")).unwrap();
        for c in v["cases"].as_array().unwrap() {
            let att = &c["att"];
            let (content, address) = (c["content"].as_str().unwrap(), c["address"].as_str().unwrap());
            assert_eq!(attestation_message(att["referee"].as_str().unwrap(), att["judge"].as_str().unwrap(), &att["result"], address, content), c["message"].as_str().unwrap());
            assert!(attestation_ok(att, Some(content), Some(address), Some(att["referee"].as_str().unwrap())));
            // Another address, other content, another referee, a changed result: no.
            assert!(!attestation_ok(att, Some(content), Some("nOther"), None));
            assert!(!attestation_ok(att, Some(&"00".repeat(32)), None, None));
            assert!(!attestation_ok(att, None, None, Some(&format!("03{}", "11".repeat(32)))));
            let mut bad = att.clone();
            bad["result"] = json!("something else");
            assert!(!attestation_ok(&bad, None, None, None));
        }
    }
}
