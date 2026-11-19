//! Sending one ord inscription from this arcade's wallet (`ord.send`; Martin 2026-11-12: "start on ORD transfers
//! now. Do the website first and then arcade V1").
//!
//! The way ord-dogecoin's wallet sends one (and the website's /account/doginal/send): the inscribed coin is input 0
//! and goes WHOLE to the recipient as output 0, so the inscription's first sat is the recipient's first; clean coins
//! (crate::swaps::coins, which leaves every coin carrying an inscription out) pay the fee; change last. From the
//! wallet's key 0 on the chain, a chain kept here with its ord index. Refused unless this arcade's ord index says the
//! inscription is on that key's coin, at offset 0, alone on the coin, and nothing waiting spends the coin already.

use crate::node::Node;
use crate::swaps::{build, coins, p2pkh_of, pool_spender, wallet_key};
use arcade_chain::OutPoint;
use arcade_wallet::tx::{TxOutSpec, Utxo};
use serde_json::{json, Value};

/// Why an inscription at `offset` on a coin carrying `on` may not be sent on its own (None: it may).
pub fn refused(id: &str, offset: u64, on: &[String]) -> Option<String> {
    if offset != 0 {
        return Some(format!("that inscription sits {offset} sats into its coin, not at its start: sending it plainly could hand the wrong piece away, so it is not sent"));
    }
    if !on.iter().any(|x| x == id) {
        return Some("the ord index has not read that inscription on its coin yet: try again in a minute".into());
    }
    if on.len() > 1 {
        return Some(format!("that coin carries {} inscriptions: sending it would hand all of them over. Sending one of them on its own is not ready yet", on.len()));
    }
    None
}

/// `inscription` (an id, or a number) on `chain` (default: home) to `to`. Built and shown with its fee; sent when `go`.
pub fn send(node: &Node, chain: Option<&str>, inscription: &str, to: &str, go: bool) -> Result<Value, String> {
    let p = match chain.filter(|c| !c.is_empty()) {
        Some(k) => crate::chains::find(k).ok_or_else(|| format!("unknown chain {k}"))?,
        None => node.params,
    };
    let wanted = inscription.trim().trim_start_matches('#');
    if crate::borrow::is_borrowed(p.key) {
        return send_borrowed(node, p, wanted, to, go);
    }
    if !crate::chains::kept(node, p) {
        return Err(format!("{} is neither installed nor borrowed here: an inscription is sent from a copy of the chain this arcade keeps or borrows", p.name));
    }
    let (op, offset) = arcade_ledger::ord::location(&node.chain, p, wanted)
        .ok_or_else(|| format!("{}'s ord index here does not know that inscription (add the ord index in Chains & Coins, or wait for it to read that far)", p.name))?;
    let iid = arcade_ledger::ord::resolve(&node.chain, p.chain_no, wanted).map(|b| arcade_ledger::ord::iid_str(&b)).unwrap_or_else(|| wanted.to_string());
    let on = arcade_ledger::ord::on_coin(&node.chain, p, &op);
    if let Some(why) = refused(&iid, offset, &on) {
        return Err(why);
    }
    let (sk, pk) = wallet_key(node, p)?;
    let own = p2pkh_of(&pk);
    let coin = node.chain.coin(p, &op).map_err(|e| e.to_string())?.ok_or("that inscription's coin is already spent")?;
    if coin.script != own {
        return Err("that inscription is not this wallet's (its coin is not on this wallet's address)".into());
    }
    if let Some(by) = pool_spender(node, p, &op) {
        return Err(format!("a waiting transaction already spends that inscription's coin ({})", by.to_hex()));
    }
    let to = to.trim();
    let script = arcade_ledger::address::script_for_address(p, to).ok_or_else(|| format!("{to} is not a {} address", p.name))?;
    if script == own {
        return Err("that is this wallet's own address".into());
    }
    let first = vec![Utxo { outpoint: op, value: coin.value, script: own.clone() }];
    let fee_coins: Vec<Utxo> = coins(node, p, &pk).into_iter().filter(|u| u.outpoint != op).collect();
    let tx = build(node, p, &sk, first, fee_coins, vec![TxOutSpec { value: coin.value, script }], &own)?;
    if tx.inputs.first().map(|i| i.prevout) != Some(op) || tx.outputs.first().map(|o| o.value) != Some(coin.value) {
        return Err("the transaction built does not carry the inscription first: nothing sent".into());
    }
    let ins_value: u64 = coin.value + tx.inputs.iter().skip(1).filter_map(|i| node.chain.coin(p, &i.prevout).ok().flatten().map(|c| c.value)).sum::<u64>();
    let fee = ins_value.saturating_sub(tx.outputs.iter().map(|o| o.value).sum());
    let mut v = json!({"chain": p.key, "inscription": iid, "coin": format!("{}:{}", op.txid.to_hex(), op.vout), "value": coin.value,
                       "to": to, "fee": fee, "txid": tx.txid.to_hex(), "size": tx.raw.len()});
    if go {
        v["txid"] = json!(crate::swaps::send(node, p, &tx)?);
        v["sent"] = json!(true);
        let _ = node.app.event("wallet", &format!("sent inscription {iid} on {} to {to} ({})", p.name, v["txid"].as_str().unwrap_or("")));
    }
    Ok(v)
}

/// Whether an entry of an explorer.ord_outpoints "on" list is the inscription asked for (an id, a txid meaning its
/// first inscription, or a number).
fn names(entry: &Value, wanted: &str) -> bool {
    let id = entry["id"].as_str().unwrap_or("");
    if !id.is_empty() && (id == wanted || (wanted.len() == 64 && id == format!("{wanted}i0"))) {
        return true;
    }
    wanted.bytes().all(|b| b.is_ascii_digit()) && entry["number"].as_u64().is_some_and(|n| n.to_string() == wanted)
}

/// Where an inscription asked for sits among this wallet's coins, from the answers (explorer.ord_outpoints, with "on")
/// of the arcades keeping the chain's ord index: (its coin, its id, its offset in the coin, every inscription on that
/// coin, how many arcades vouch). Refused when an arcade asked says otherwise (another coin or offset, other
/// inscriptions on the coin, the coin free), or when none names it.
pub(crate) fn locate(answers: &[(Value, String)], wanted: &str) -> Result<(OutPoint, String, u64, Vec<String>, usize), String> {
    let mut seen = std::collections::HashSet::new();
    let mut found: Option<(OutPoint, String, u64, Vec<String>)> = None;
    let mut vouch = 0;
    let mut told_on = false;
    for (v, by) in answers {
        if !seen.insert(by.as_str()) {
            continue;
        }
        let Some(on) = v["on"].as_object() else { continue };
        told_on = true;
        let mut here = None;
        for (coin, list) in on {
            let list = list.as_array().cloned().unwrap_or_default();
            if let Some(e) = list.iter().find(|e| names(e, wanted)) {
                let op = parse_op(coin).ok_or("an arcade named a coin that isn't one")?;
                let mut ids: Vec<String> = list.iter().filter_map(|e| e["id"].as_str().map(String::from)).collect();
                ids.sort();
                here = Some((op, e["id"].as_str().unwrap_or("").to_string(), e["offset"].as_u64().unwrap_or(u64::MAX), ids));
            }
        }
        match (&found, here) {
            (_, None) => {
                // This arcade doesn't name it: fine while it hasn't read that coin (neither held nor free), not when it
                // calls the coin found by another arcade free.
                if let Some((op, ..)) = &found {
                    if v["free"].as_array().is_some_and(|f| f.iter().any(|x| x.as_str() == Some(format!("{}:{}", op.txid.to_hex(), op.vout).as_str()))) {
                        return Err("the arcades keeping the ord index disagree about that inscription's coin: nothing is sent".into());
                    }
                }
            }
            (None, Some(h)) => {
                found = Some(h);
                vouch = 1;
            }
            (Some(f), Some(h)) if *f == h => vouch += 1,
            (Some(_), Some(_)) => return Err("the arcades keeping the ord index disagree about where that inscription is: nothing is sent".into()),
        }
    }
    if !told_on && !answers.is_empty() {
        return Err("the arcades keeping the ord index are of an older version that doesn't say which inscriptions sit on a coin: nothing is sent".into());
    }
    // A coin one arcade calls free while another names the inscription on it (answered before it was found).
    if let Some((op, ..)) = &found {
        let s = format!("{}:{}", op.txid.to_hex(), op.vout);
        if answers.iter().any(|(v, _)| v["free"].as_array().is_some_and(|f| f.iter().any(|x| x.as_str() == Some(s.as_str())))) {
            return Err("the arcades keeping the ord index disagree about that inscription's coin: nothing is sent".into());
        }
    }
    let (op, id, off, on) = found.ok_or("no arcade keeping the ord index finds that inscription on this wallet's coins (sent away already, or not read that far yet)")?;
    Ok((op, id, off, on, vouch))
}

fn parse_op(s: &str) -> Option<OutPoint> {
    let (t, v) = s.split_once(':')?;
    Some(OutPoint { txid: arcade_chain::Hash256::from_hex(t).ok()?, vout: v.parse().ok()? })
}

/// [`send`] on a chain only borrowed here (borrowed = installed, batch 6): this wallet's coins from a lender (each
/// checked against its block), where the inscription sits from up to three independent arcades keeping the ord index
/// (all that answer must agree), built and signed here (the key never leaves), sent through the lenders and tracked
/// until a block has it.
fn send_borrowed(node: &Node, p: &'static arcade_chain::params::ChainParams, wanted: &str, to: &str, go: bool) -> Result<Value, String> {
    if arcade_ledger::ord::ord_start(p).is_none() {
        return Err(format!("{} has no ord inscriptions", p.name));
    }
    let (sk, pk) = wallet_key(node, p)?;
    let own = p2pkh_of(&pk);
    let to = to.trim();
    let script = arcade_ledger::address::script_for_address(p, to).ok_or_else(|| format!("{to} is not a {} address", p.name))?;
    if script == own {
        return Err("that is this wallet's own address".into());
    }
    let held = crate::borrow::own_coins(node, p, &own);
    if held.is_empty() {
        return Err(format!("this wallet holds no coins on {} that a lender could show", p.name));
    }
    let list: Vec<String> = held.iter().take(500).map(|u| format!("{}:{}", u.outpoint.txid.to_hex(), u.outpoint.vout)).collect();
    let q = json!({"chain": p.key, "outpoints": list, "local": true});
    let answers = crate::shares::ask_explorer_each(node, p, "ord", "explorer.ord_outpoints", &q, 3, 6000);
    if answers.is_empty() {
        return Err(format!("no arcade keeping {}'s ord index answered: nothing is sent", p.name));
    }
    let (op, iid, offset, on, vouch) = locate(&answers, wanted)?;
    if let Some(why) = refused(&iid, offset, &on) {
        return Err(why);
    }
    let coin = held.iter().find(|u| u.outpoint == op).cloned().ok_or("that inscription is not on this wallet's coins")?;
    if let Some(by) = pool_spender(node, p, &op) {
        return Err(format!("a waiting transaction already spends that inscription's coin ({})", by.to_hex()));
    }
    let first = vec![Utxo { outpoint: op, value: coin.value, script: own.clone() }];
    let fee_coins: Vec<Utxo> = coins(node, p, &pk).into_iter().filter(|u| u.outpoint != op).collect();
    let tx = build(node, p, &sk, first, fee_coins.clone(), vec![TxOutSpec { value: coin.value, script }], &own)?;
    if tx.inputs.first().map(|i| i.prevout) != Some(op) || tx.outputs.first().map(|o| o.value) != Some(coin.value) {
        return Err("the transaction built does not carry the inscription first: nothing sent".into());
    }
    let ins_value: u64 = coin.value + tx.inputs.iter().skip(1).filter_map(|i| fee_coins.iter().find(|c| c.outpoint == i.prevout).map(|c| c.value)).sum::<u64>();
    let fee = ins_value.saturating_sub(tx.outputs.iter().map(|o| o.value).sum());
    let spent: Vec<OutPoint> = tx.inputs.iter().skip(1).map(|i| i.prevout).collect();
    // Where the inscription is rests on the arcades asked: one alone is said so in the preview.
    let located = (vouch < 2).then(|| {
        crate::shares::caution(crate::shares::Trust::Yellow, "Only one arcade keeping the ord index vouched where this inscription sits. Double-check before sending.")
    });
    let mut v = json!({"chain": p.key, "inscription": iid, "coin": format!("{}:{}", op.txid.to_hex(), op.vout), "value": coin.value,
                       "to": to, "fee": fee, "txid": tx.txid.to_hex(), "size": tx.raw.len(), "borrowed": true,
                       "caution": crate::shares::worse_caution(located, crate::ordsafe::caution(node, p, &spent))});
    if go {
        v["txid"] = json!(crate::swaps::send(node, p, &tx)?);
        v["sent"] = json!(true);
        let _ = node.app.event("wallet", &format!("sent inscription {iid} on {} (borrowed) to {to} ({})", p.name, v["txid"].as_str().unwrap_or("")));
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::{locate, refused};
    use serde_json::json;

    #[test]
    fn only_one_inscription_at_the_start_of_its_coin_goes() {
        let one = vec!["a".to_string()];
        assert_eq!(refused("a", 0, &one), None);
        assert!(refused("a", 546, &one).unwrap().contains("546 sats into"));
        assert!(refused("a", 0, &["a".into(), "b".into()]).unwrap().contains("2 inscriptions"));
        assert!(refused("a", 0, &[]).unwrap().contains("not read"));
    }

    fn coin(n: u8) -> String {
        format!("{}:0", hex::encode([n; 32]))
    }

    fn id(n: u8) -> String {
        format!("{}i0", hex::encode([n; 32]))
    }

    /// An arcade's explorer.ord_outpoints answer naming `list` on coin `c` (id, offset, number).
    fn said(c: &str, list: &[(String, u64, u64)], free: &[String]) -> serde_json::Value {
        json!({"held": [c], "free": free, "on": {c: list.iter().map(|(i, o, n)| json!({"id": i, "offset": o, "number": n})).collect::<Vec<_>>()}})
    }

    /// Borrowed = installed, batch 6: where an inscription sits comes from the arcades keeping the ord index, and they
    /// must agree; it is found by id, by its txid or by its number.
    #[test]
    fn a_borrowed_inscription_is_found_where_the_keepers_agree() {
        let one = said(&coin(1), &[(id(9), 0, 42)], &[]);
        let two = vec![(one.clone(), "a".to_string()), (one.clone(), "b".to_string())];
        let (op, iid, off, on, vouch) = locate(&two, &id(9)).unwrap();
        assert_eq!((format!("{}:{}", op.txid.to_hex(), op.vout), iid.clone(), off, on, vouch), (coin(1), id(9), 0, vec![id(9)], 2));
        assert_eq!(locate(&two, "42").unwrap().1, id(9), "by number");
        assert_eq!(locate(&two, &hex::encode([9u8; 32])).unwrap().1, id(9), "by txid");
        // The same arcade twice counts once.
        assert_eq!(locate(&[(one.clone(), "a".into()), (one.clone(), "a".into())], &id(9)).unwrap().4, 1);
        // Another coin, another offset, or more on the coin: refused.
        let elsewhere = said(&coin(2), &[(id(9), 0, 42)], &[]);
        assert!(locate(&[(one.clone(), "a".into()), (elsewhere, "b".into())], &id(9)).unwrap_err().contains("disagree"));
        let shifted = said(&coin(1), &[(id(9), 546, 42)], &[]);
        assert!(locate(&[(one.clone(), "a".into()), (shifted, "b".into())], &id(9)).unwrap_err().contains("disagree"));
        let more = said(&coin(1), &[(id(9), 0, 42), (id(8), 0, 41)], &[]);
        assert!(locate(&[(one.clone(), "a".into()), (more, "b".into())], &id(9)).unwrap_err().contains("disagree"));
        // One that calls the coin free, before or after: refused.
        let free = json!({"held": [], "free": [coin(1)], "on": {}});
        assert!(locate(&[(one.clone(), "a".into()), (free.clone(), "b".into())], &id(9)).unwrap_err().contains("disagree"));
        assert!(locate(&[(free, "b".into()), (one.clone(), "a".into())], &id(9)).unwrap_err().contains("disagree"));
        // One that hasn't read the coin (neither held nor free) leaves the other's word.
        let behind = json!({"held": [], "free": [], "on": {}});
        assert_eq!(locate(&[(one.clone(), "a".into()), (behind, "b".into())], &id(9)).unwrap().4, 1);
        // Nobody names it; an older keeper that gives no "on".
        assert!(locate(&two, &id(7)).unwrap_err().contains("no arcade"));
        assert!(locate(&[(json!({"held": [coin(1)], "free": []}), "a".into())], &id(9)).unwrap_err().contains("older version"));
    }
}
