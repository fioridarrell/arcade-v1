//! Inscriptions that are longer than a mempool chain.
//!
//! Each piece spends the change of the piece before it, and any unconfirmed transactions already
//! in the wallet count toward the same row of [`crate::chainlimit::LIMIT`]. This sends only what
//! fits, writes each signed transaction down before it is broadcast, and a background pass sends
//! the rest as earlier ones confirm. A piece already broadcast is never signed again, including
//! after a restart: the bytes and the id are kept in the arcade's database.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use arcade_chain::Tx;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

const JOURNAL: &str = "inscj:";
const BODY: &str = "inscb:";

static GATE: Mutex<()> = Mutex::new(());

fn locked<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let _g = GATE.lock().unwrap_or_else(|e| e.into_inner());
    f()
}

fn jkey(key: &str) -> Vec<u8> {
    format!("{JOURNAL}{key}").into_bytes()
}

fn bkey(key: &str) -> Vec<u8> {
    format!("{BODY}{key}").into_bytes()
}

fn jget(node: &Node, key: &str) -> Option<Value> {
    node.app
        .store
        .read(|r| r.get(arcade_app::META, &jkey(key)))
        .ok()
        .flatten()
        .and_then(|v| serde_json::from_slice(&v).ok())
}

fn jput(node: &Node, j: &Value) -> Result<(), String> {
    let key = j["key"].as_str().unwrap_or("");
    node.app
        .store
        .write(|w| w.put(arcade_app::META, &jkey(key), j.to_string().as_bytes()))
        .map_err(|e| {
            format!("the inscription's journal couldn't be written ({e}); nothing more was sent")
        })
}

fn journals(node: &Node) -> Vec<Value> {
    let mut all = node
        .app
        .store
        .read(|r| r.scan_prefix(arcade_app::META, JOURNAL.as_bytes()))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_slice::<Value>(&v).ok())
        .collect::<Vec<_>>();
    all.sort_by_key(|j| j["at"].as_u64().unwrap_or(0));
    all
}

fn put_body(node: &Node, key: &str, bytes: &[u8]) -> Result<(), String> {
    node.app
        .store
        .write(|w| w.put(arcade_app::META, &bkey(key), bytes))
        .map_err(|e| format!("the inscription's bytes couldn't be kept ({e}); nothing was sent"))
}

fn get_body(node: &Node, key: &str) -> Option<Vec<u8>> {
    node.app
        .store
        .read(|r| r.get(arcade_app::META, &bkey(key)))
        .ok()
        .flatten()
}

fn del_body(node: &Node, key: &str) {
    let _ = node
        .app
        .store
        .write(|w| w.del(arcade_app::META, &bkey(key)).map(|_| ()));
}

/// The chain an inscription is written to: the one named, or the chain a send would use now.
pub fn chain_for(node: &Node, chain: Option<&str>) -> Result<&'static ChainParams, String> {
    match chain.map(str::trim).filter(|c| !c.is_empty()) {
        None => Ok(crate::msgsvc::active_write_chain(node)),
        Some(k) => crate::msgsvc::write_chains(node)
            .into_iter()
            .find(|p| p.key.eq_ignore_ascii_case(k))
            .ok_or_else(|| {
                let have = crate::msgsvc::write_chains(node)
                    .iter()
                    .map(|p| p.key)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("can't inscribe on {k} here (this wallet can write to {have})")
            }),
    }
}

fn chain_by_key(node: &Node, key: &str) -> Result<&'static ChainParams, String> {
    if let Ok(p) = chain_for(node, Some(key)) {
        return Ok(p);
    }
    crate::arcsvc::defined()
        .filter(|p| p.key.eq_ignore_ascii_case(key))
        .or_else(|| node.chain_params(key))
        .or_else(|| arcade_chain::params::by_key(key))
        .ok_or_else(|| format!("{key} isn't a chain this arcade knows"))
}

fn journal_key(sha: &str, chain: &str, ctype: &str, json: &str) -> String {
    let mix = hex::encode(&Sha256::digest(format!("{ctype}\n{json}").as_bytes())[..8]);
    format!("{sha}:{chain}:{mix}")
}

/// What inscribing `len` bytes costs on `p`: transactions, fee at that chain's rate, value kept in
/// the data outputs, and the chain's ticker. The same bytes on another chain are a different quote.
pub fn quote(p: &ChainParams, len: usize, ctype: &str, json_len: usize) -> Value {
    let stream = 4 + 32 + 1 + ctype.len() + 2 + json_len + len;
    let room = arcade_wallet::tx::CLASS_B_MAX - 4 - 18;
    let txs = stream.div_ceil(room).max(1);
    let packets = stream.div_ceil(30) + txs;
    let outs = packets.div_ceil(2) + txs;
    let bytes = txs * (10 + 148 + 34) + packets.div_ceil(2) * 114 + txs * 34;
    let fp = crate::coinsend::fee_policy(p);
    let rate = fp.fallback_per_kb.max(fp.floor_per_kb);
    let fee = (bytes as u64 * rate).div_ceil(1000);
    json!({
        "bytes": len,
        "transactions": txs,
        "fee": fee,
        "locked": outs as u64 * arcade_wallet::tx::CLASS_B_OUTPUT,
        "chain": p.key,
        "unit": crate::explorer::unit(p),
        "fee_per_kb": rate,
    })
}

/// Inscribe `content`, sending only as many pieces as the mempool chain has room for. The same
/// bytes, type, card and chain asked again finish the same inscription and never pay twice.
pub fn inscribe(
    node: &Node,
    content: &[u8],
    ctype: &str,
    json_text: Option<&str>,
    chain: Option<&str>,
) -> Result<Value, String> {
    if content.is_empty() {
        return Err("nothing to inscribe".into());
    }
    let ctype = if ctype.is_empty() {
        "text/plain"
    } else {
        ctype
    };
    let json_text = json_text.unwrap_or("");
    if !json_text.is_empty() {
        serde_json::from_str::<Value>(json_text)
            .map_err(|e| format!("the JSON does not parse: {e}"))?;
    }
    let p = chain_for(node, chain)?;
    locked(|| begin(node, p, content, ctype, json_text))
}

fn begin(
    node: &Node,
    p: &'static ChainParams,
    content: &[u8],
    ctype: &str,
    json_text: &str,
) -> Result<Value, String> {
    let sha = hex::encode(Sha256::digest(content));
    let key = journal_key(&sha, p.key, ctype, json_text);
    if let Some(mut j) = jget(node, &key) {
        if j["sha256"] != sha.as_str()
            || j["chain"] != p.key
            || j["content_type"] != ctype
            || j["json"] != json_text
        {
            return Err("this inscription's journal is for a different file".into());
        }
        if get_body(node, &key).is_none() && j["done"] != true {
            put_body(node, &key, content)?;
        }
        return advance(node, &mut j);
    }
    let id8: [u8; 8] = arcade_mesh::random::bytes();
    let chunks = pieces(content, ctype, json_text, id8)?;
    let j = json!({
        "key": key,
        "chain": p.key,
        "sha256": sha,
        "content_type": ctype,
        "json": json_text,
        "id8": hex::encode(id8),
        "total": chunks.len(),
        "sent": [],
        "staged": Value::Null,
        "fee": 0,
        "waiting": 0,
        "done": false,
        "at": arcade_app::now(),
    });
    put_body(node, &key, content)?;
    jput(node, &j)?;
    let mut j = j;
    advance(node, &mut j)
}

fn pieces(
    content: &[u8],
    ctype: &str,
    json_text: &str,
    id8: [u8; 8],
) -> Result<Vec<Vec<u8>>, String> {
    arcade_wallet::tx::insc_chunks(
        content,
        ctype,
        (!json_text.is_empty()).then_some(json_text),
        id8,
    )
}

fn sent_idxs(j: &Value) -> Vec<usize> {
    j["sent"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x["n"].as_u64().map(|n| n as usize))
                .collect()
        })
        .unwrap_or_default()
}

fn staged_idx(j: &Value) -> Option<usize> {
    if j["staged"].is_null() {
        None
    } else {
        j["staged"]["n"].as_u64().map(|n| n as usize)
    }
}

fn commit_staged(j: &mut Value) {
    let s = j["staged"].clone();
    if !s.is_object() {
        return;
    }
    let mut sent = j["sent"].as_array().cloned().unwrap_or_default();
    if sent.iter().any(|x| x["n"] == s["n"]) {
        j["staged"] = Value::Null;
        return;
    }
    j["fee"] = json!(j["fee"].as_u64().unwrap_or(0) + s["fee"].as_u64().unwrap_or(0));
    sent.push(s);
    j["sent"] = json!(sent);
    j["staged"] = Value::Null;
}

fn known(node: &Node, p: &'static ChainParams, tx: &Tx) -> bool {
    crate::ownwait::where_is(node, p, tx) != crate::ownwait::Where::Neither
}

fn already(err: &str) -> bool {
    err.to_ascii_lowercase().contains("already")
}

fn id8_of(j: &Value) -> Result<[u8; 8], String> {
    let raw = hex::decode(j["id8"].as_str().unwrap_or(""))
        .map_err(|_| "the journal's inscription id is unreadable".to_string())?;
    raw.as_slice()
        .try_into()
        .map_err(|_| "the journal's inscription id is unreadable".to_string())
}

fn advance(node: &Node, j: &mut Value) -> Result<Value, String> {
    if j["done"] == true {
        let mut v = view(node, j);
        v["note"] = json!("already inscribed: nothing was sent again");
        v["resumed"] = json!(true);
        return Ok(v);
    }
    let key = j["key"].as_str().unwrap_or("").to_string();
    let p = chain_by_key(node, j["chain"].as_str().unwrap_or(""))?;
    let content = get_body(node, &key)
        .ok_or("the inscription's bytes are no longer kept; publish the file again")?;
    let ctype = j["content_type"]
        .as_str()
        .unwrap_or("text/plain")
        .to_string();
    let json_text = j["json"].as_str().unwrap_or("").to_string();
    let id8 = id8_of(j)?;
    // The id is part of every piece. Rebuilding with the stored id yields the same payloads, so a
    // piece already signed is the same transaction and is not built again.
    let chunks = pieces(&content, &ctype, &json_text, id8)?;
    if chunks.len() != j["total"].as_u64().unwrap_or(0) as usize {
        return Err("the inscription's pieces don't match its journal".into());
    }
    let before = sent_idxs(j).len();
    let resumed = before > 0 || staged_idx(j).is_some();
    for _ in 0..chunks.len() + 2 {
        match crate::chainlimit::next_piece(chunks.len(), &sent_idxs(j), staged_idx(j)) {
            crate::chainlimit::Next::Done => {
                j["done"] = json!(true);
                j["waiting"] = json!(0);
                jput(node, j)?;
                del_body(node, &key);
                let _ = node.app.event(
                    "wallet",
                    &format!(
                        "inscribed {} bytes of {ctype} on {} in {} transaction{}",
                        content.len(),
                        p.name,
                        chunks.len(),
                        if chunks.len() == 1 { "" } else { "s" }
                    ),
                );
                break;
            }
            crate::chainlimit::Next::Broadcast(_) => {
                let raw_hex = j["staged"]["raw"].as_str().unwrap_or("").to_string();
                let raw = hex::decode(&raw_hex)
                    .map_err(|_| "a saved transaction isn't hex".to_string())?;
                let tx = Tx::parse_with(&raw, &p.format)
                    .map_err(|e| format!("a saved transaction doesn't parse ({e})"))?;
                if known(node, p, &tx) {
                    commit_staged(j);
                    jput(node, j)?;
                    continue;
                }
                let row = crate::msgsvc::projected_row(node, p, &tx);
                if !crate::chainlimit::fits(row) {
                    j["waiting"] = json!(row.saturating_sub(1));
                    jput(node, j)?;
                    break;
                }
                match crate::msgsvc::relay_tx(node, p, &tx) {
                    Ok(fee) => {
                        if fee > 0 {
                            j["staged"]["fee"] = json!(fee);
                        }
                        commit_staged(j);
                        jput(node, j)?;
                    }
                    Err(e) if already(&e) || known(node, p, &tx) => {
                        commit_staged(j);
                        jput(node, j)?;
                    }
                    Err(e) if crate::chainlimit::is_refusal(&e) => {
                        j["waiting"] = json!(crate::chainlimit::LIMIT);
                        jput(node, j)?;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
            crate::chainlimit::Next::Build(n) => {
                let staged = crate::msgsvc::stage_on(node, p, &chunks[n])?;
                if !crate::chainlimit::fits(staged.row) {
                    j["waiting"] = json!(staged.row.saturating_sub(1));
                    jput(node, j)?;
                    break;
                }
                j["staged"] = json!({"n": n, "txid": staged.tx.txid.to_hex(), "raw": hex::encode(&staged.tx.raw), "fee": staged.fee});
                jput(node, j)?;
                match crate::msgsvc::relay_tx(node, p, &staged.tx) {
                    Ok(fee) => {
                        if fee > 0 {
                            j["staged"]["fee"] = json!(fee);
                        }
                        commit_staged(j);
                        jput(node, j)?;
                    }
                    Err(e) if already(&e) => {
                        commit_staged(j);
                        jput(node, j)?;
                    }
                    Err(e) if crate::chainlimit::is_refusal(&e) => {
                        j["waiting"] = json!(crate::chainlimit::LIMIT);
                        jput(node, j)?;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }
    let sent_now = sent_idxs(j).len();
    if sent_now > before && j["done"] != true {
        let v = view(node, j);
        let _ = node
            .app
            .event("wallet", &v["note"].as_str().unwrap_or("").to_string());
    }
    let mut v = view(node, j);
    v["resumed"] = json!(resumed);
    Ok(v)
}

fn view(node: &Node, j: &Value) -> Value {
    let mut ordered = j["sent"].as_array().cloned().unwrap_or_default();
    ordered.sort_by_key(|x| x["n"].as_u64().unwrap_or(0));
    let txids: Vec<String> = ordered
        .iter()
        .filter_map(|x| x["txid"].as_str().map(str::to_string))
        .collect();
    let sent = ordered.len();
    let total = j["total"].as_u64().unwrap_or(0) as usize;
    let waiting = j["waiting"].as_u64().unwrap_or(0) as usize;
    let done = j["done"] == true;
    let chain = j["chain"].as_str().unwrap_or("");
    let unit = chain_by_key(node, chain)
        .ok()
        .map(|p| crate::explorer::unit(p).to_string())
        .unwrap_or_default();
    let note = if done {
        "Inscribed. It shows once its transactions are in blocks.".to_string()
    } else {
        crate::chainlimit::note(sent, total, waiting)
    };
    json!({
        "id": j["key"],
        "chain": chain,
        "unit": unit,
        "txid": txids.first().cloned().unwrap_or_default(),
        "txids": txids,
        "fee": j["fee"].as_u64().unwrap_or(0),
        "sent": sent,
        "total": total,
        "transactions": total,
        "state": if done { "sent" } else { "waiting" },
        "waiting": waiting,
        "note": note,
    })
}

fn find_journal(node: &Node, id: &str) -> Option<Value> {
    journals(node).into_iter().find(|j| {
        j["key"] == id
            || j["sent"]
                .as_array()
                .is_some_and(|a| a.iter().any(|t| t["txid"] == id))
            || j["staged"]["txid"] == id
    })
}

/// One inscription (`id` is its journal key, or any transaction it has sent), or every unfinished one.
pub fn status(node: &Node, id: Option<&str>) -> Result<Value, String> {
    match id.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(
            json!({"jobs": journals(node).iter().filter(|j| j["done"] != true).map(|j| view(node, j)).collect::<Vec<_>>()}),
        ),
        Some(id) => find_journal(node, id)
            .map(|j| view(node, &j))
            .ok_or_else(|| "no such inscription is being sent from here".into()),
    }
}

/// Send whatever still fits of one inscription, or of every unfinished one.
pub fn resume_req(node: &Node, req: &Value) -> Result<Value, String> {
    locked(
        || match req["id"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => resume_id(node, id),
            None => {
                let keys: Vec<String> = journals(node)
                    .into_iter()
                    .filter(|j| j["done"] != true)
                    .filter_map(|j| j["key"].as_str().map(str::to_string))
                    .collect();
                let mut jobs = Vec::new();
                for k in keys {
                    jobs.push(resume_id(node, &k)?);
                }
                Ok(json!({"jobs": jobs}))
            }
        },
    )
}

fn resume_id(node: &Node, id: &str) -> Result<Value, String> {
    let mut j = find_journal(node, id).ok_or("no such inscription is being sent from here")?;
    advance(node, &mut j)
}

/// Every few seconds, for as long as the arcade runs: each inscription not finished goes as far as
/// the mempool chain allows. A restart picks the journal up and does not sign those pieces again.
pub fn run(node: Arc<Node>) {
    loop {
        if crate::STOP_ASKED.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_secs(15));
        if crate::STOP_ASKED.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        if node.app.has_wallet().ok() != Some(true) {
            continue;
        }
        let _ = locked(|| {
            let keys: Vec<String> = journals(&node)
                .into_iter()
                .filter(|j| j["done"] != true)
                .filter_map(|j| j["key"].as_str().map(str::to_string))
                .collect();
            for k in keys {
                let _ = resume_id(&node, &k);
            }
            Ok(())
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The quote follows the chain: ARCADE testnet's fee rate is not Pepecoin testnet's, and the
    /// ticker is the chain's. The transaction count is the file's, so only the fee and the unit move.
    #[test]
    fn the_quote_changes_when_the_chain_changes() {
        let pep = arcade_chain::params::by_key("peptest").expect("peptest");
        let arc = arcade_arc::store::chain_params(
            &arcade_arc::params::TESTNET,
            &arcade_chain::Hash256::ZERO,
        );
        let len = 357_000;
        let json_len = 120;
        let a = quote(pep, len, "text/html", json_len);
        let b = quote(arc, len, "text/html", json_len);
        assert_eq!(
            a["transactions"], b["transactions"],
            "the file is the same number of pieces"
        );
        assert_eq!(
            a["locked"], b["locked"],
            "the data outputs hold the same coins"
        );
        assert!(
            a["fee"].as_u64() > b["fee"].as_u64(),
            "Pepecoin testnet's rate is higher: {a} vs {b}"
        );
        assert_eq!(a["fee_per_kb"], 1_000_000);
        assert_eq!(b["fee_per_kb"], 2 * arcade_arc::params::MIN_FEE_PER_KB);
        assert_eq!(a["unit"], "PEPtest");
        assert_eq!(b["unit"], "ARCADEtest");
        assert_eq!(a["chain"], "peptest");
        assert_eq!(b["chain"], "arctest");
        assert_ne!(
            journal_key("ab", "peptest", "text/html", "{}"),
            journal_key("ab", "arctest", "text/html", "{}")
        );
        assert_ne!(
            journal_key("ab", "arctest", "text/html", "{}"),
            journal_key("ab", "arctest", "text/html", "{\"app\":{}}")
        );
    }
}
