//! Build-only transactions for a key that signs somewhere else (ARCADE Git #23): a website's user signs in the ARCADE
//! app through its site pairing (crate::sitesign), so the website's server asks this arcade to build what the user
//! does, unsigned, and the app signs it.
//!
//! - `psbt.send {address, pubkey, chain?, to, amount}`: a payment.
//! - `psbt.feed_post {address, pubkey, chain?, text, channel?, file?}`: a public post, the record feed.post writes.
//! - `psbt.feed_act {address, pubkey, chain?, kind, target, text?, amount?}`: like, reply, tip...; a tip pays the
//!   post's author in the same transaction, as feed.act does.
//! - `psbt.finalize {psbt, chain?}`: the signed PSBT back, checked and finished: the raw transaction for
//!   tx.broadcast.
//! - `msg.verify {address, message, signature, prefix? | chain?}`: whether a Core signmessage signature checks.
//!
//! No keys here and nothing is signed or sent. Coins are tx.coins' (inscribed ones held back, crate::ordsafe), the
//! fee rate and dust tx.fee's, and the records byte for byte the ones this arcade's own wallet writes (and the
//! original website builds). `chain` defaults to the ledger home (ARCADE testnet on the test arcades).
use crate::node::Node;
use crate::psbt::Psbt;
use arcade_chain::params::ChainParams;
use arcade_chain::{Hash256, OutPoint, Tx};
use arcade_wallet::secp256k1::PublicKey;
use arcade_wallet::tx::{TxOutSpec, Utxo};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

type R = Result<Value, String>;

/// Most builds a minute on this arcade, and for one address.
const MOST_A_MINUTE: u32 = 600;
const MOST_A_MINUTE_EACH: u32 = 60;

static WINDOW: LazyLock<Mutex<(u64, u32, HashMap<String, u32>)>> = LazyLock::new(Default::default);

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Builds read coins and previous transactions: a few a second from anyone, as many as a website's users make.
fn limit(who: &str) -> Result<(), String> {
    let minute = now() / 60;
    let mut w = WINDOW.lock().unwrap();
    if w.0 != minute {
        *w = (minute, 0, HashMap::new());
    }
    let each = w.2.get(who).copied().unwrap_or(0);
    if w.1 >= MOST_A_MINUTE || each >= MOST_A_MINUTE_EACH {
        return Err("this arcade is building as many transactions as it takes this minute: try again in a minute".into());
    }
    w.1 += 1;
    if w.2.len() < 100_000 {
        w.2.insert(who.to_string(), each + 1);
    }
    Ok(())
}

pub fn dispatch(node: &Node, req: &Value) -> R {
    let cmd = req["cmd"].as_str().unwrap_or("");
    if cmd == "psbt.finalize" {
        limit("")?;
        return finalize(node, req);
    }
    let w = who(node, req)?;
    limit(&w.address)?;
    match cmd {
        "psbt.send" => send(node, &w, req),
        "psbt.feed_post" => feed_post(node, &w, req),
        "psbt.feed_act" => feed_act(node, &w, req),
        _ => Err(format!("unknown command {cmd}")),
    }
}

/// The key a build is for.
struct Who {
    p: &'static ChainParams,
    pk: PublicKey,
    address: String,
    script: Vec<u8>,
}

/// The chain a build or a finished PSBT is on: `chain`, else the ledger home. Legacy transactions without a time
/// field only (what crate::psbt reads and the app signs).
fn chain_of(node: &Node, req: &Value) -> Result<&'static ChainParams, String> {
    let p = match req["chain"].as_str().map(str::trim).filter(|k| !k.is_empty()) {
        Some(k) => crate::chains::find(k).ok_or_else(|| format!("unknown chain {k}"))?,
        None => node.ledger_home(),
    };
    if p.format.tx_time {
        return Err(format!("{} transactions carry a time field: they can't be built as a PSBT here", p.name));
    }
    Ok(p)
}

fn who(node: &Node, req: &Value) -> Result<Who, String> {
    let p = chain_of(node, req)?;
    let pk = hex::decode(req["pubkey"].as_str().unwrap_or("").trim())
        .ok()
        .filter(|b| b.len() == 33)
        .and_then(|b| PublicKey::from_slice(&b).ok())
        .ok_or("pubkey: the account's compressed public key (33 bytes, hex), as the ARCADE app's pairing gives it")?;
    let address = arcade_wallet::keys::p2pkh_address(&pk, p.p2pkh_version);
    if let Some(a) = req["address"].as_str().map(str::trim).filter(|a| !a.is_empty()) {
        if a != address {
            return Err(format!("address {a} isn't pubkey's {} address ({address})", p.name));
        }
    }
    let script = arcade_wallet::keys::p2pkh_script_for(&pk);
    Ok(Who { p, pk, address, script })
}

/// Coins tx.coins lists for the address (confirmed and waiting; inscribed ones held back), largest first.
fn coins(node: &Node, w: &Who) -> Result<Vec<Utxo>, String> {
    let got = crate::sitesvc::coins(node, &json!({"addresses": [w.address], "chain": w.p.key}))?;
    let a = &got["addresses"][&w.address];
    let op_of = |c: &Value| -> Option<OutPoint> { Some(OutPoint { txid: Hash256::from_hex(c["txid"].as_str()?).ok()?, vout: c["vout"].as_u64()? as u32 }) };
    let own_arc = crate::arcsvc::own(w.p);
    let mut out = Vec::new();
    for (list, confirmed) in [(&a["confirmed"], true), (&a["unconfirmed"], false)] {
        for c in list.as_array().into_iter().flatten() {
            let Some(op) = op_of(c) else { continue };
            // The ARCADE chain's store copy follows a block a moment later: a coin its chain has spent stays out.
            if confirmed && own_arc && (crate::arcsvc::spender(&op).is_some() || !crate::arcsvc::unspent(&op)) {
                continue;
            }
            out.push(Utxo { outpoint: op, value: c["value"].as_u64().unwrap_or(0), script: w.script.clone() });
        }
    }
    out.sort_by(|a, b| b.value.cmp(&a.value));
    Ok(out)
}

/// The whole transaction a coin is an output of (a PSBT carries it so the signer sees the real amount): a waiting
/// one, one in a block here, or a lender's.
fn prev_tx(node: &Node, p: &'static ChainParams, txid: &Hash256, vout: u32) -> Result<Vec<u8>, String> {
    let borrowed = crate::borrow::is_borrowed(p.key);
    let pooled: Vec<Tx> = if borrowed { crate::borrow::pool_txs(p) } else { crate::sitesvc::pool(node, p).into_iter().map(|(_, t)| t).collect() };
    let raw = match pooled.into_iter().find(|t| t.txid == *txid) {
        Some(t) => Some(t.raw),
        None => node.chain.tx_raw(p, txid).ok().flatten().map(|(raw, _)| raw),
    };
    // The ARCADE chain's own copy: from the block itself, before its store copy has it.
    let raw = raw.or_else(|| crate::arcsvc::own(p).then(|| crate::arcsvc::coin_and_tx(&OutPoint { txid: *txid, vout }).and_then(|(_, _, t)| t)).flatten());
    let raw = match raw {
        Some(r) => r,
        None if borrowed => crate::borrow::raw_tx(node, p, txid)?.raw,
        None => return Err(format!("the transaction of coin {} isn't readable here", txid.to_hex())),
    };
    match Tx::parse_with(&raw, &p.format) {
        Ok(t) if t.txid == *txid => Ok(raw),
        _ => Err(format!("the transaction of coin {} didn't read back as itself", txid.to_hex())),
    }
}

fn shown_to(p: &ChainParams, script: &[u8]) -> String {
    arcade_ledger::address::address_of(script, p).unwrap_or_else(|| if script.first() == Some(&0x6a) { "data (OP_RETURN)".into() } else { format!("script {}", hex::encode(script)) })
}

/// The unsigned PSBT for `chosen` coins and `outs`, with what it does.
fn answer(node: &Node, w: &Who, chosen: &[Utxo], outs: &[TxOutSpec], what: &str) -> R {
    let mut with_prev = Vec::new();
    for c in chosen {
        with_prev.push((c.outpoint, prev_tx(node, w.p, &c.outpoint.txid, c.outpoint.vout)?));
    }
    let outputs: Vec<(u64, Vec<u8>)> = outs.iter().map(|o| (o.value, o.script.clone())).collect();
    let ps = Psbt::unsigned(&with_prev, &outputs)?;
    let ins: u64 = chosen.iter().map(|c| c.value).sum();
    let out_total: u64 = outs.iter().map(|o| o.value).sum();
    use base64::Engine;
    Ok(json!({
        "psbt": base64::engine::general_purpose::STANDARD.encode(ps.serialize()),
        "chain": w.p.key,
        "unit": crate::explorer::unit(w.p),
        "address": w.address,
        "what": what,
        "summary": {
            "inputs": chosen.iter().map(|c| json!({"coin": format!("{}:{}", c.outpoint.txid.to_hex(), c.outpoint.vout), "value": c.value, "from": w.address})).collect::<Vec<_>>(),
            "outputs": outs.iter().enumerate().map(|(n, o)| json!({"index": n, "value": o.value, "to": shown_to(w.p, &o.script), "mine": o.script == w.script})).collect::<Vec<_>>(),
            "fee": ins.saturating_sub(out_total),
        },
    }))
}

fn fees(p: &ChainParams) -> (u64, u64) {
    let fp = crate::coinsend::fee_policy(p);
    (fp.fallback_per_kb, fp.dust)
}

fn built<T>(r: Result<T, arcade_wallet::tx::BuildError>) -> Result<T, String> {
    r.map_err(|e| format!("couldn't build it: {e}"))
}

/// Where a payment to `to` (an address of the chain, or a @tag) goes.
fn pay_to(node: &Node, p: &'static ChainParams, to: &str) -> Result<Vec<u8>, String> {
    let to = to.trim();
    let address = if to.starts_with('@') {
        let (on, holder) = crate::hometag::owner_or_err(node, to)?;
        crate::hometag::pay_address(node, p, on, to.trim_start_matches('@'), &holder)?
    } else {
        to.to_string()
    };
    arcade_ledger::address::script_for_address(p, &address).ok_or_else(|| format!("{address} isn't a {} address", p.name))
}

/// psbt.send: `amount` (smallest units) to `to`, change back.
fn send(node: &Node, w: &Who, req: &Value) -> R {
    let (fee, dust) = fees(w.p);
    let amount = req["amount"].as_u64().ok_or("amount: in the smallest units")?;
    if amount < dust {
        return Err(format!("an amount under {dust} is dust on {}", w.p.name));
    }
    let script = pay_to(node, w.p, req["to"].as_str().ok_or("to: an address or a @tag")?)?;
    let coins = coins(node, w)?;
    let (chosen, outs) = built(arcade_wallet::tx::plan_spend(w.p, vec![TxOutSpec { value: amount, script }], &w.pk, &coins, fee, dust))?;
    answer(node, w, &chosen, &outs, "a payment")
}

/// Most bytes of an OP_RETURN record (Class C) on `p`, as this arcade's own wallet writes them (crate::msgsvc).
fn class_c_max(p: &ChainParams) -> usize {
    if arcade_chain::params::ARCADE_CHAINS.iter().any(|(n, _)| *n == p.chain_no) {
        arcade_wallet::tx::ARCADE_CLASS_C_MAX
    } else {
        80
    }
}

/// One record's transaction from the key, plus any `extra` outputs (a tip's payment).
fn record(node: &Node, w: &Who, payload: &[u8], extra: Vec<TxOutSpec>, what: &str) -> R {
    let (fee, dust) = fees(w.p);
    let coins = coins(node, w)?;
    let (chosen, outs) = built(arcade_wallet::tx::plan_arcade(w.p, &arcade_wallet::tx::any_data(payload), extra, &w.pk, &coins, fee, dust, class_c_max(w.p)))?;
    answer(node, w, &chosen, &outs, what)
}

fn file_of(req: &Value) -> Result<Option<arcade_msg::content::File>, String> {
    let Some(f) = req["file"].as_object() else { return Ok(None) };
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(f.get("data").and_then(|d| d.as_str()).unwrap_or("")).map_err(|e| format!("file data: {e}"))?;
    Ok(Some(arcade_msg::content::File {
        name: f.get("name").and_then(|x| x.as_str()).unwrap_or("file").into(),
        mime: f.get("type").and_then(|x| x.as_str()).unwrap_or("application/octet-stream").into(),
        bytes,
    }))
}

/// psbt.feed_post: the post as feed.post writes it, in one transaction (a post too long for one is refused).
fn feed_post(node: &Node, w: &Who, req: &Value) -> R {
    if crate::hometag::of_address(node, &w.address).is_none() {
        return Err("claim a @tag first: public posts show your @tag".into());
    }
    let post = crate::feedsvc::make_post(req["text"].as_str().unwrap_or(""), req["channel"].as_str().unwrap_or(""), file_of(req)?)?;
    let payloads = post.payloads();
    if payloads.len() != 1 {
        return Err(format!("that is too long for one transaction (a post's record is at most {} bytes): say less, or send a smaller file", arcade_msg::envelope::MAX_PAYLOAD));
    }
    record(node, w, &payloads[0], vec![], "a post")
}

/// psbt.feed_act: a feed act as feed.act writes it; a tip pays `amount` to the post's author in the same
/// transaction (their address on this chain: the same key's on a chain that derives keys as the ledger home does,
/// else the one they proved).
fn feed_act(node: &Node, w: &Who, req: &Value) -> R {
    let kind = req["kind"].as_str().ok_or("kind: like, unlike, dislike, undislike, reply, share, edit, delete or tip")?;
    let act = crate::feedsvc::act_kind(kind)?;
    let target = req["target"].as_str().unwrap_or("").trim().to_lowercase();
    if hex::decode(&target).map(|b| b.len()) != Ok(32) {
        return Err("target is a post's txid".into());
    }
    let mut extra = Vec::new();
    if act == arcade_msg::public::Act::Tip {
        let (_, dust) = fees(w.p);
        let amount = req["amount"].as_u64().unwrap_or(0);
        if amount == 0 {
            return Err("a tip needs an amount".into());
        }
        if amount < dust {
            return Err("a tip that small is dust".into());
        }
        let author = crate::feedsvc::tip_author(node, &target)?;
        let home = node.ledger_home();
        let tag = crate::hometag::of_address(node, &author).unwrap_or_default();
        let at = crate::hometag::pay_address(node, w.p, home, &tag, &author)?;
        let script = arcade_ledger::address::script_for_address(w.p, &at).ok_or_else(|| format!("the author's address {at} doesn't read"))?;
        extra.push(TxOutSpec { value: amount, script });
    }
    let fa = arcade_msg::public::FeedAct { act, target, text: req["text"].as_str().unwrap_or("").to_string() };
    record(node, w, &fa.encode(), extra, kind)
}

/// psbt.finalize: a signed PSBT in, every P2PKH signature checked and any partial one finished; the raw
/// transaction out, for tx.broadcast.
fn finalize(node: &Node, req: &Value) -> R {
    let p = chain_of(node, req)?;
    let mut ps = Psbt::parse(&crate::psbt::decode_text(req["psbt"].as_str().ok_or("psbt: the signed PSBT (base64 or hex)")?)?)?;
    let raw = ps.finalize_p2pkh()?;
    let tx = Tx::parse_with(&raw, &p.format).map_err(|e| format!("not a {} transaction: {e}", p.name))?;
    let ins: Option<u64> = (0..ps.inputs.len()).map(|i| ps.input(i).coin.map(|c| c.0)).sum();
    let outs: u64 = tx.outputs.iter().map(|o| o.value).sum();
    Ok(json!({"hex": hex::encode(&raw), "txid": tx.txid.to_hex(), "chain": p.key, "size": raw.len(), "fee": ins.and_then(|i| i.checked_sub(outs))}))
}

/// msg.verify: whether `signature` (Core signmessage, base64) is `address`'s over `message`, with `prefix`, else
/// `chain`'s prefix, else ARCADE's.
pub fn verify(req: &Value) -> R {
    let address = req["address"].as_str().ok_or("address required")?.trim();
    let message = req["message"].as_str().ok_or("message required")?;
    let signature = req["signature"].as_str().ok_or("signature required (base64)")?;
    let prefix: String = match (req["prefix"].as_str(), req["chain"].as_str().map(str::trim).filter(|c| !c.is_empty())) {
        (Some(x), _) if x.len() <= 100 => x.to_string(),
        (Some(_), _) => return Err("prefix: at most 100 characters".into()),
        (None, Some(c)) => {
            let p = crate::chains::find(c).ok_or_else(|| format!("unknown chain {c}"))?;
            crate::siteid::chain_magic(p.key).ok_or_else(|| format!("{}'s message prefix isn't known here: give prefix", p.name))?.to_string()
        }
        (None, None) => crate::siteid::ARCADE_MAGIC.to_string(),
    };
    Ok(json!({"valid": arcade_wallet::keys::verifymessage(address, signature, &prefix, message), "address": address, "prefix": prefix}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcade_wallet::secp256k1::{SecretKey, SECP256K1};

    fn key(b: u8) -> (SecretKey, PublicKey) {
        let k = SecretKey::from_slice(&[b; 32]).unwrap();
        (k, PublicKey::from_secret_key(SECP256K1, &k))
    }

    fn p() -> &'static ChainParams {
        // The ARCADE testnet as its store makes it (its genesis doesn't matter here).
        arcade_arc::store::chain_params(&arcade_arc::params::TESTNET, &Hash256::default())
    }

    /// A previous transaction paying `value` to `script` (output 0).
    fn funding(script: &[u8], value: u64, n: u8) -> Vec<u8> {
        let shape = arcade_wallet::legacy::Shape {
            version: 1,
            time: None,
            inputs: vec![arcade_wallet::legacy::In { prevout: OutPoint { txid: Hash256([n; 32]), vout: 0 }, sequence: u32::MAX }],
            outputs: vec![(value, script.to_vec())],
            lock_time: 0,
        };
        arcade_wallet::legacy::serialize(&shape, &[vec![0x51]])
    }

    /// The website's (arcade-site actions.py, Python) OP_RETURN scripts for the same records, computed with its own
    /// code: group.plan(GroupPost(channel, "", text)) and feedlib.build(kind, target, text), AnyData-wrapped, Class C.
    const SITE_POST_MAIN: &str = "6a2061726364000000c86172636d0104046d61696e0068656c6c6f20617263616465";
    const SITE_POST_TXTOK: &str = "6a1f61726364000000c86172636d0104057478746f6b006120636c697020e29880";
    const SITE_LIKE: &str = "6a2f61726364000000c86172636d010701abababababababababababababababababababababababababababababababcd";
    const SITE_REPLY: &str = "6a3761726364000000c86172636d010703abababababababababababababababababababababababababababababababcd6e696365206f6e65";
    const SITE_TIP: &str = "6a2f61726364000000c86172636d010707abababababababababababababababababababababababababababababababcd";

    #[test]
    fn records_are_the_websites_byte_for_byte() {
        let (_, pk) = key(3);
        let target = format!("{}cd", "ab".repeat(31));
        let script = |payload: Vec<u8>, max: usize| hex::encode(&arcade_wallet::tx::arcade_outputs(p(), &arcade_wallet::tx::any_data(&payload), &pk, max).unwrap()[0].script);
        let post = |ch: &str, text: &str| crate::feedsvc::make_post(text, ch, None).unwrap().payloads();
        for max in [80, arcade_wallet::tx::ARCADE_CLASS_C_MAX] {
            let main = post("", "hello arcade");
            assert_eq!(main.len(), 1);
            assert_eq!(script(main[0].clone(), max), SITE_POST_MAIN);
            assert_eq!(script(post("TXTOK ", "a clip ☀")[0].clone(), max), SITE_POST_TXTOK);
            let act = |k: &str, text: &str| arcade_msg::public::FeedAct { act: crate::feedsvc::act_kind(k).unwrap(), target: target.clone(), text: text.into() }.encode();
            assert_eq!(script(act("like", ""), max), SITE_LIKE);
            assert_eq!(script(act("reply", "nice one"), max), SITE_REPLY);
            assert_eq!(script(act("tip", "ignored"), max), SITE_TIP);
        }
        // Longer than the coin chains' 80 bytes: Class B there, one OP_RETURN on ARCADE.
        let long = post("", &"x".repeat(300))[0].clone();
        assert_eq!(arcade_wallet::tx::arcade_outputs(p(), &arcade_wallet::tx::any_data(&long), &pk, arcade_wallet::tx::ARCADE_CLASS_C_MAX).unwrap().len(), 1);
        assert!(arcade_wallet::tx::arcade_outputs(p(), &arcade_wallet::tx::any_data(&long), &pk, 80).unwrap().len() > 2);
    }

    #[test]
    fn a_plan_is_what_the_wallet_would_sign_and_finalizes_to_a_valid_transaction() {
        let (sk, pk) = key(5);
        let mine = arcade_wallet::keys::p2pkh_script_for(&pk);
        let prev = funding(&mine, 50_000_000, 1);
        let ptx = Tx::parse(&prev).unwrap();
        let coins = vec![Utxo { outpoint: OutPoint { txid: ptx.txid, vout: 0 }, value: 50_000_000, script: mine.clone() }];
        let fa = arcade_msg::public::FeedAct { act: arcade_msg::public::Act::Like, target: "11".repeat(32), text: String::new() };
        let payload = arcade_wallet::tx::any_data(&fa.encode());
        let fee = 2 * arcade_arc::params::MIN_FEE_PER_KB;
        let (chosen, outs) = arcade_wallet::tx::plan_arcade(p(), &payload, vec![], &pk, &coins, fee, arcade_arc::params::DUST, arcade_wallet::tx::ARCADE_CLASS_C_MAX).unwrap();
        // The same transaction the wallet's own build signs.
        let own = arcade_wallet::tx::build_arcade_with(p(), &payload, vec![], &sk, &coins, fee, arcade_arc::params::DUST, 0, arcade_wallet::tx::ARCADE_CLASS_C_MAX).unwrap();
        assert_eq!(own.outputs.iter().map(|o| (o.value, o.script_pubkey.clone())).collect::<Vec<_>>(), outs.iter().map(|o| (o.value, o.script.clone())).collect::<Vec<_>>());
        let outputs: Vec<(u64, Vec<u8>)> = outs.iter().map(|o| (o.value, o.script.clone())).collect();
        let ps = Psbt::unsigned(&[(chosen[0].outpoint, prev.clone())], &outputs).unwrap();
        let b = ps.serialize();
        let back = Psbt::parse(&b).unwrap();
        assert_eq!(back.serialize(), b);
        assert_eq!(back.input(0).coin, Some((50_000_000, mine.clone())));
        // Signed as the app signs (finalize true): the transaction is the wallet's own, byte for byte.
        let mut signed = back.clone();
        signed.sign_input(0, &sk, &pk.serialize(), arcade_wallet::legacy::ALL, true).unwrap();
        let raw = signed.clone().finalize_p2pkh().unwrap();
        assert_eq!(raw, own.raw, "the same signature (RFC 6979) and the same bytes");
        let tx = Tx::parse_with(&raw, &p().format).unwrap();
        assert!(arcade_wallet::tx::verify_p2pkh_input(p(), &tx, &coins, 0));
        // Signed without finalize: a partial signature, finished here.
        let mut partial = back.clone();
        partial.sign_input(0, &sk, &pk.serialize(), arcade_wallet::legacy::ALL, false).unwrap();
        assert!(partial.extract().is_none());
        assert_eq!(partial.finalize_p2pkh().unwrap(), raw);
        // Not signed, or signed by another key, or a signature that doesn't check: refused.
        assert!(back.clone().finalize_p2pkh().is_err());
        let (other, opk) = key(6);
        let mut wrong = back.clone();
        let mut k = vec![0x02];
        k.extend_from_slice(&opk.serialize());
        let digest = arcade_wallet::legacy::sighash(&arcade_wallet::legacy::Shape { version: 1, time: None, inputs: vec![arcade_wallet::legacy::In { prevout: chosen[0].outpoint, sequence: u32::MAX }], outputs: outputs.clone(), lock_time: 0 }, 0, &mine, 1).unwrap();
        wrong.inputs[0].push((k, arcade_wallet::legacy::sign(&other, &digest, 1)));
        assert!(wrong.finalize_p2pkh().is_err());
        let mut bad = signed.clone();
        let ss = bad.inputs[0].iter_mut().find(|(k, _)| k == &vec![0x07]).unwrap();
        ss.1[10] ^= 1;
        assert!(bad.finalize_p2pkh().is_err());
    }

    #[test]
    fn coin_selection_pays_outputs_fee_and_change_and_says_when_it_cant() {
        let (_, pk) = key(7);
        let mine = arcade_wallet::keys::p2pkh_script_for(&pk);
        let coin = |v: u64, n: u8| Utxo { outpoint: OutPoint { txid: Hash256([n; 32]), vout: 0 }, value: v, script: mine.clone() };
        let to = arcade_ledger::address::p2pkh_script(&[9u8; 20]);
        let (chosen, outs) = arcade_wallet::tx::plan_spend(p(), vec![TxOutSpec { value: 1_000_000, script: to.clone() }], &pk, &[coin(400_000, 1), coin(900_000, 2), coin(5_000_000, 3)], 2_000, 1_000).unwrap();
        assert_eq!(chosen.len(), 1);
        assert_eq!(chosen[0].value, 5_000_000);
        assert_eq!(outs[0].script, to);
        assert_eq!(outs.last().unwrap().script, mine);
        let fee = 5_000_000 - outs.iter().map(|o| o.value).sum::<u64>();
        assert!(fee > 0 && fee < 2_000);
        // Another key's coins are never picked.
        let theirs = Utxo { script: arcade_ledger::address::p2pkh_script(&[8u8; 20]), ..coin(9_000_000, 4) };
        assert!(arcade_wallet::tx::plan_spend(p(), vec![TxOutSpec { value: 1_000_000, script: to }], &pk, &[theirs], 2_000, 1_000).is_err());
    }

    #[test]
    fn messages_verify_with_the_prefix_asked() {
        let (sk, pk) = key(9);
        let address = arcade_wallet::keys::p2pkh_address(&pk, p().p2pkh_version);
        let sig = arcade_wallet::keys::signmessage(&sk, crate::siteid::ARCADE_MAGIC, "sign in to txtok 42");
        let v = |r: Value| verify(&r).unwrap()["valid"] == true;
        assert!(v(json!({"address": address, "message": "sign in to txtok 42", "signature": sig})), "ARCADE's prefix by default");
        assert!(v(json!({"address": address, "message": "sign in to txtok 42", "signature": sig, "prefix": "Pepecoin Signed Message:\n"})));
        assert!(!v(json!({"address": address, "message": "sign in to txtok 43", "signature": sig})));
        assert!(!v(json!({"address": address, "message": "sign in to txtok 42", "signature": sig, "prefix": "Dogecoin Signed Message:\n"})));
        assert!(!v(json!({"address": arcade_wallet::keys::p2pkh_address(&key(10).1, 0x53), "message": "sign in to txtok 42", "signature": sig})), "another address");
        assert!(!v(json!({"address": address, "message": "sign in to txtok 42", "signature": "not base64"})));
        let dsig = arcade_wallet::keys::signmessage(&sk, "Dogecoin Signed Message:\n", "hi");
        assert!(v(json!({"address": arcade_wallet::keys::p2pkh_address(&pk, 0x1e), "message": "hi", "signature": dsig, "prefix": "Dogecoin Signed Message:\n"})));
        assert!(verify(&json!({"address": address, "message": "x"})).is_err());
        assert_eq!(crate::siteid::chain_magic("arctest"), Some(crate::siteid::ARCADE_MAGIC));
        assert_eq!(crate::siteid::chain_magic("doge"), Some("Dogecoin Signed Message:\n"));
    }
}
