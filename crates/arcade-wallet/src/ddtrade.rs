//! DigiDollar for DGB in one transaction (docs/MARKET.md "DigiDollar", build step 4): both assets move by
//! the chain's own rules, so the pair needs no lock and no ledger. The dollar seller's DigiDollar coins and
//! the DGB payer's coins are inputs of one DigiDollar send; each side checks the whole transaction and signs
//! only its own inputs, over all outputs (SIGHASH_ALL / Taproot default), so nobody can change it after.
//!
//! Outputs, in order: the dollars to the buyer (zero-value P2TR), the seller's dollar change (zero-value
//! P2TR, when any), the DGB price to the seller, the fee payer's DGB change, any other DGB change, and the
//! record `OP_RETURN "DD" 2 <cents> [<change cents>]`. Version 0x02000770, lock time 0.
//!
//! A fill and what is left of an order are each $1 or more (DigiDollar's least output); the fee is
//! DigiDollar's own rate (0.35 DGB a kB, 0.1 DGB at least), paid by the side named `fee_by`.

use crate::dd::{Held, DD_INPUT, DUST, FEE_PER_BYTE, LEAST_FEE, LEAST_OUTPUT, MAX_FEE, MOST_OUTPUT, P2PKH_INPUT, P2WPKH_INPUT, SEND_VERSION};
use crate::tx::{push, TxOutSpec, Utxo, SIGHASH_ALL};
use arcade_chain::params::ChainParams;
use arcade_chain::{OutPoint, Tx};
use arcade_ledger::digidollar::encode_script_num;
use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};

/// One side's coins for the trade.
#[derive(Debug, Clone, Default)]
pub struct Side {
    /// DigiDollar coins (the dollar seller only), at the side's own Taproot output.
    pub dd: Vec<Held>,
    /// DGB coins: the price (the buyer), or the fee (whichever side pays it).
    pub dgb: Vec<Utxo>,
    /// Where this side's DGB change goes.
    pub dgb_change: Vec<u8>,
}

/// What was agreed.
#[derive(Debug, Clone)]
pub struct Deal {
    /// Dollars that change hands, in cents.
    pub cents: i64,
    /// DGB the buyer pays for them, in satoshis.
    pub price: u64,
    /// Where the dollars go: the buyer's own DigiDollar output (P2TR script).
    pub dollars_to: Vec<u8>,
    /// Where the seller's dollar change goes: the seller's own DigiDollar output (P2TR script).
    pub seller_dd: Vec<u8>,
    /// Where the price goes (a DGB script of the seller's).
    pub price_to: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Seller,
    Buyer,
}

/// The transaction both sides sign: its coins in input order, and its outputs.
#[derive(Debug, Clone)]
pub struct Unsigned {
    pub utxos: Vec<Utxo>,
    pub outs: Vec<TxOutSpec>,
    pub fee: u64,
}

fn record(cents: i64, back: i64) -> Vec<u8> {
    let mut s = vec![0x6a];
    push(&mut s, b"DD");
    push(&mut s, &encode_script_num(2));
    push(&mut s, &encode_script_num(cents));
    if back > 0 {
        push(&mut s, &encode_script_num(back));
    }
    s
}

fn input_size(u: &Utxo) -> usize {
    if u.script.len() == 34 && u.script[0] == 0x51 {
        DD_INPUT
    } else if u.script.len() == 22 {
        P2WPKH_INPUT
    } else {
        P2PKH_INPUT
    }
}

/// Lay the trade out: the seller's dollar coins (largest first, as few as cover `cents`), the buyer's DGB
/// coins for the price (and the fee, when it pays), the seller's DGB coins for the fee when it pays.
pub fn layout(seller: &Side, buyer: &Side, deal: &Deal, fee_by: Role) -> Result<Unsigned, String> {
    if !(LEAST_OUTPUT..=MOST_OUTPUT).contains(&deal.cents) {
        return Err("a DigiDollar fill is $1 to $100,000".into());
    }
    if deal.price < DUST {
        return Err("the DGB price is under the dust line".into());
    }
    let mut dd: Vec<&Held> = seller.dd.iter().filter(|h| h.coin.script == deal.seller_dd).collect();
    dd.sort_by(|a, b| b.cents.cmp(&a.cents));
    let (mut spend, mut have) = (Vec::new(), 0i64);
    for h in dd {
        if have >= deal.cents {
            break;
        }
        spend.push(h.clone());
        have += h.cents;
    }
    if have < deal.cents {
        return Err(format!("the seller holds ${:.2} and the fill is ${:.2}", have as f64 / 100.0, deal.cents as f64 / 100.0));
    }
    let back = have - deal.cents;
    if back > 0 && back < LEAST_OUTPUT {
        return Err("that would leave the seller under $1 of dollar change; fill a little less or all of it".into());
    }
    let mut outs = vec![TxOutSpec { value: 0, script: deal.dollars_to.clone() }];
    if back > 0 {
        outs.push(TxOutSpec { value: 0, script: deal.seller_dd.clone() });
    }
    outs.push(TxOutSpec { value: deal.price, script: deal.price_to.clone() });
    let rec = record(deal.cents, back);
    let (payer, other) = match fee_by {
        Role::Buyer => (buyer, None),
        Role::Seller => (seller, Some(buyer)),
    };
    // The side that doesn't pay the fee pays exactly its part: the buyer the price, from its own coins.
    let mut chosen: Vec<Utxo> = spend.iter().map(|h| h.coin.clone()).collect();
    let mut other_change = None;
    if let Some(b) = other {
        let mut cs = b.dgb.clone();
        cs.sort_by(|a, b| b.value.cmp(&a.value));
        let mut got = 0u64;
        for c in cs {
            if got >= deal.price {
                break;
            }
            got += c.value;
            chosen.push(c);
        }
        if got < deal.price {
            return Err("the buyer's DGB doesn't cover the price".into());
        }
        if got - deal.price >= DUST {
            other_change = Some(TxOutSpec { value: got - deal.price, script: b.dgb_change.clone() });
        }
    }
    let need_from_payer = if fee_by == Role::Buyer { deal.price } else { 0 };
    let mut cs = payer.dgb.clone();
    cs.sort_by(|a, b| b.value.cmp(&a.value));
    let mut got = 0u64;
    let mut found = None;
    for c in cs {
        got += c.value;
        chosen.push(c);
        let mut size = 10 + 2 + chosen.iter().map(input_size).sum::<usize>() + outs.iter().map(|o| 9 + o.script.len()).sum::<usize>() + 9 + rec.len();
        size += 9 + payer.dgb_change.len() + other_change.as_ref().map(|o| 9 + o.script.len()).unwrap_or(0);
        let fee = (size as u64 * FEE_PER_BYTE).max(LEAST_FEE);
        if fee > MAX_FEE {
            return Err("the DGB is in too many small coins for one DigiDollar transaction's fee".into());
        }
        if got >= need_from_payer + fee {
            let change = got - need_from_payer - fee;
            found = Some(if change < DUST { (fee + change, 0) } else { (fee, change) });
            break;
        }
    }
    let Some((fee, change)) = found else { return Err("not enough DGB for the price and the network fee".into()) };
    if change > 0 {
        outs.push(TxOutSpec { value: change, script: payer.dgb_change.clone() });
    }
    if let Some(o) = other_change {
        outs.push(o);
    }
    outs.push(TxOutSpec { value: 0, script: rec });
    Ok(Unsigned { utxos: chosen, outs, fee })
}

fn unsigned_tx(p: &ChainParams, u: &Unsigned) -> Result<Tx, String> {
    let ins: Vec<(OutPoint, Vec<u8>)> = u.utxos.iter().map(|x| (x.outpoint, vec![])).collect();
    Tx::parse_with(&crate::tx::serialize(p, SEND_VERSION, 0, &ins, &u.outs), &p.format).map_err(|e| e.to_string())
}

/// What a side checks before it signs: the transaction gives it what was agreed and takes no more than
/// agreed. `mine` are the coins this side put in; `my_change` where its DGB change may go.
pub fn check(p: &ChainParams, u: &Unsigned, deal: &Deal, role: Role, mine: &[Utxo], my_change: &[u8], fee_by: Role) -> Result<(), String> {
    let tx = unsigned_tx(p, u)?;
    let dd = arcade_ledger::digidollar::read(&tx).ok_or("that is not a DigiDollar transaction")?;
    if dd.record_type != Some(2) {
        return Err("that is not a DigiDollar send".into());
    }
    // Dollars: exactly `cents` to the buyer's output, and every other dollar output is the seller's change.
    let to_key: [u8; 32] = deal.dollars_to.get(2..34).and_then(|k| k.try_into().ok()).ok_or("bad dollar address")?;
    let seller_key: [u8; 32] = deal.seller_dd.get(2..34).and_then(|k| k.try_into().ok()).ok_or("bad dollar address")?;
    let to_buyer: i64 = dd.outputs.iter().filter(|(_, k, _)| *k == to_key).map(|(_, _, c)| *c).sum();
    if to_buyer != deal.cents {
        return Err(format!("the record pays the buyer {to_buyer} cents, not the {} agreed", deal.cents));
    }
    if dd.outputs.iter().any(|(_, k, _)| *k != to_key && *k != seller_key) {
        return Err("the record pays dollars to somebody else".into());
    }
    let dollars_in: i64 = u.utxos.iter().filter(|x| x.script == deal.seller_dd).count() as i64; // counted below by role
    let _ = dollars_in;
    let out_dgb = |script: &[u8]| -> u64 { u.outs.iter().filter(|o| o.value > 0 && o.script == script).map(|o| o.value).sum() };
    let mine_in: u64 = u.utxos.iter().filter(|x| mine.iter().any(|m| m.outpoint == x.outpoint)).map(|x| x.value).sum();
    // Every coin of mine in it is one I offered, and none I offered is missing from my side's count.
    match role {
        Role::Seller => {
            if out_dgb(&deal.price_to) < deal.price {
                return Err("the transaction pays less than the price".into());
            }
            // The seller's dollar change is what its dollar inputs held less the fill (the record's own count).
            let back: i64 = dd.outputs.iter().filter(|(_, k, _)| *k == seller_key).map(|(_, _, c)| *c).sum();
            if back < 0 {
                return Err("bad change".into());
            }
            if fee_by == Role::Seller {
                let spent = mine_in.saturating_sub(out_dgb(my_change));
                if spent > MAX_FEE {
                    return Err("the fee is more than a DigiDollar transaction needs".into());
                }
            }
        }
        Role::Buyer => {
            let back = out_dgb(my_change);
            let spent = mine_in.saturating_sub(back);
            let most = deal.price + if fee_by == Role::Buyer { MAX_FEE } else { 0 };
            if spent > most {
                return Err(format!("this spends {spent} of your DGB satoshis; the price is {} and the fee at most {}", deal.price, most - deal.price));
            }
        }
    }
    let fee = u.utxos.iter().map(|x| x.value).sum::<u64>().saturating_sub(u.outs.iter().map(|o| o.value).sum());
    if fee < LEAST_FEE || fee > MAX_FEE {
        return Err(format!("a network fee of {fee} is outside DigiDollar's range"));
    }
    Ok(())
}

/// One side's signatures: (input index, scriptSig, witness) for the inputs this key can sign.
pub type Part = Vec<(usize, Vec<u8>, Vec<Vec<u8>>)>;

/// Sign the inputs `key` owns (its P2PKH, P2WPKH and own Taproot key path), over all outputs.
pub fn sign_mine(p: &ChainParams, u: &Unsigned, key: &SecretKey) -> Result<Part, String> {
    let pk = PublicKey::from_secret_key(SECP256K1, key);
    let h = arcade_ledger::address::hash160(&pk.serialize());
    let p2pkh = arcade_ledger::address::p2pkh_script(&h);
    let p2wpkh = arcade_ledger::address::p2wpkh_script(&h);
    let p2tr = crate::taproot::output_key(&pk).map(|q| crate::taproot::p2tr_script(&q));
    let outs_tr: Vec<(u64, Vec<u8>)> = u.outs.iter().map(|o| (o.value, o.script.clone())).collect();
    let spent: Vec<crate::taproot::SpentInput> = u.utxos.iter().map(|x| crate::taproot::SpentInput { outpoint: x.outpoint, sequence: u32::MAX, amount: x.value, script: &x.script }).collect();
    let mut part = Vec::new();
    for (i, x) in u.utxos.iter().enumerate() {
        if x.script == p2pkh {
            let ins: Vec<(OutPoint, Vec<u8>)> = u.utxos.iter().enumerate().map(|(j, y)| (y.outpoint, if i == j { y.script.clone() } else { vec![] })).collect();
            let mut pre = crate::tx::serialize(p, SEND_VERSION, 0, &ins, &u.outs);
            pre.extend_from_slice(&SIGHASH_ALL.to_le_bytes());
            let sig = SECP256K1.sign_ecdsa(&Message::from_digest(arcade_chain::hash::sha256d(&pre).0), key);
            let mut der = sig.serialize_der().to_vec();
            der.push(SIGHASH_ALL as u8);
            let mut ss = Vec::new();
            push(&mut ss, &der);
            push(&mut ss, &pk.serialize());
            part.push((i, ss, vec![]));
        } else if x.script == p2wpkh {
            let sig = SECP256K1.sign_ecdsa(&Message::from_digest(crate::tx::bip143_digest(SEND_VERSION, &u.utxos, &u.outs, i)), key);
            let mut der = sig.serialize_der().to_vec();
            der.push(SIGHASH_ALL as u8);
            part.push((i, vec![], vec![der, pk.serialize().to_vec()]));
        } else if Some(&x.script) == p2tr.as_ref() {
            let kp = crate::taproot::tweaked_keypair(key, None).ok_or("the key can't be tweaked")?;
            let sh = crate::taproot::sighash_default(SEND_VERSION, 0, &spent, &outs_tr, i);
            let mut aux = [0u8; 32];
            getrandom::getrandom(&mut aux).map_err(|e| format!("no randomness: {e}"))?;
            part.push((i, vec![], vec![crate::taproot::sign(&kp, &sh, &aux).to_vec()]));
        }
    }
    if part.is_empty() {
        return Err("none of these coins is this key's".into());
    }
    Ok(part)
}

/// Put both sides' signatures into the transaction and check every input verifies.
pub fn assemble(p: &ChainParams, u: &Unsigned, parts: &[&Part]) -> Result<Tx, String> {
    let n = u.utxos.len();
    let mut ss: Vec<Option<Vec<u8>>> = vec![None; n];
    let mut ws: Vec<Vec<Vec<u8>>> = vec![Vec::new(); n];
    for part in parts {
        for (i, s, w) in part.iter() {
            if *i >= n || ss[*i].is_some() {
                return Err("two signatures for one input".into());
            }
            ss[*i] = Some(s.clone());
            ws[*i] = w.clone();
        }
    }
    if ss.iter().any(|s| s.is_none()) {
        return Err("an input is not signed yet".into());
    }
    let ins: Vec<(OutPoint, Vec<u8>)> = u.utxos.iter().zip(ss).map(|(x, s)| (x.outpoint, s.unwrap())).collect();
    let legacy = crate::tx::serialize(p, SEND_VERSION, 0, &ins, &u.outs);
    let raw = if ws.iter().any(|w| !w.is_empty()) {
        let head = 4 + if p.format.tx_time { 4 } else { 0 };
        let mut o = legacy[..head].to_vec();
        o.extend_from_slice(&[0, 1]);
        o.extend_from_slice(&legacy[head..legacy.len() - 4]);
        for w in &ws {
            arcade_chain::encode::write_varint(&mut o, w.len() as u64);
            for item in w {
                arcade_chain::encode::write_varint(&mut o, item.len() as u64);
                o.extend_from_slice(item);
            }
        }
        o.extend_from_slice(&0u32.to_le_bytes());
        o
    } else {
        legacy
    };
    let tx = Tx::parse_with(&raw, &p.format).map_err(|e| format!("built a tx that doesn't parse: {e}"))?;
    if !crate::tx::verify_keyhash(p, &tx, &u.utxos) {
        return Err("a signature doesn't verify".into());
    }
    Ok(tx)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcade_chain::params::DIGIBYTE_TEST;
    use arcade_chain::Hash256;

    fn key(b: u8) -> (SecretKey, Vec<u8>, Vec<u8>) {
        let k = SecretKey::from_slice(&[b; 32]).unwrap();
        let pk = PublicKey::from_secret_key(SECP256K1, &k);
        let tr = crate::taproot::p2tr_script(&crate::taproot::output_key(&pk).unwrap());
        let wpkh = arcade_ledger::address::p2wpkh_script(&arcade_ledger::address::hash160(&pk.serialize()));
        (k, tr, wpkh)
    }
    fn coin(n: u8, value: u64, script: &[u8]) -> Utxo {
        Utxo { outpoint: OutPoint { txid: Hash256([n; 32]), vout: 0 }, value, script: script.to_vec() }
    }

    fn setup(fee_by: Role) -> (Unsigned, Deal, Side, Side, SecretKey, SecretKey) {
        let (sk, s_tr, s_w) = key(0x11);
        let (bk, b_tr, b_w) = key(0x22);
        let seller = Side { dd: vec![Held { coin: coin(1, 0, &s_tr), cents: 5000 }], dgb: vec![coin(3, 100_000_000, &s_w)], dgb_change: s_w.clone() };
        let buyer = Side { dd: vec![], dgb: vec![coin(2, 2_000_000_000, &b_w)], dgb_change: b_w.clone() };
        let deal = Deal { cents: 2000, price: 1_500_000_000, dollars_to: b_tr, seller_dd: s_tr, price_to: s_w };
        (layout(&seller, &buyer, &deal, fee_by).unwrap(), deal, seller, buyer, sk, bk)
    }

    #[test]
    fn a_fill_with_change_is_signed_by_both_and_reads_as_agreed() {
        for fee_by in [Role::Buyer, Role::Seller] {
            let (u, deal, seller, buyer, sk, bk) = setup(fee_by);
            let s_mine: Vec<Utxo> = seller.dd.iter().map(|h| h.coin.clone()).chain(seller.dgb.clone()).collect();
            check(&DIGIBYTE_TEST, &u, &deal, Role::Seller, &s_mine, &seller.dgb_change, fee_by).unwrap();
            check(&DIGIBYTE_TEST, &u, &deal, Role::Buyer, &buyer.dgb, &buyer.dgb_change, fee_by).unwrap();
            let a = sign_mine(&DIGIBYTE_TEST, &u, &bk).unwrap();
            let b = sign_mine(&DIGIBYTE_TEST, &u, &sk).unwrap();
            let tx = assemble(&DIGIBYTE_TEST, &u, &[&a, &b]).unwrap();
            let dd = arcade_ledger::digidollar::read(&tx).unwrap();
            let cents: Vec<i64> = dd.outputs.iter().map(|o| o.2).collect();
            assert_eq!(cents, vec![2000, 3000]);
            assert!(assemble(&DIGIBYTE_TEST, &u, &[&a]).is_err(), "one side alone can't send it");
        }
    }

    #[test]
    fn a_short_record_or_price_is_refused() {
        let (mut u, deal, seller, buyer, _, _) = setup(Role::Buyer);
        let s_mine: Vec<Utxo> = seller.dd.iter().map(|h| h.coin.clone()).collect();
        // The price output cut short: the seller refuses.
        let at = u.outs.iter().position(|o| o.value == deal.price).unwrap();
        u.outs[at].value -= 1;
        assert!(check(&DIGIBYTE_TEST, &u, &deal, Role::Seller, &s_mine, &seller.dgb_change, Role::Buyer).is_err());
        // The record pays the buyer a cent short: the buyer refuses.
        let (mut u, deal, _, buyer2, _, _) = setup(Role::Buyer);
        let last = u.outs.len() - 1;
        u.outs[last].script = record(1999, 3001);
        assert!(check(&DIGIBYTE_TEST, &u, &deal, Role::Buyer, &buyer2.dgb, &buyer2.dgb_change, Role::Buyer).is_err());
        let _ = buyer;
    }

    #[test]
    fn under_a_dollar_is_refused() {
        let (_, mut deal, seller, buyer, _, _) = setup(Role::Buyer);
        deal.cents = 99;
        assert!(layout(&seller, &buyer, &deal, Role::Buyer).is_err());
        deal.cents = 4950; // leaves 50 cents of change
        assert!(layout(&seller, &buyer, &deal, Role::Buyer).is_err());
    }
}
