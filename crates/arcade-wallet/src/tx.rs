//! Building and signing legacy P2PKH transactions, and the @tag claim transaction.

use arcade_chain::encode::write_varint;
use arcade_chain::hash::sha256d;
use arcade_chain::params::ChainParams;
use arcade_chain::{OutPoint, Tx};
use arcade_ledger::extract::MARKER;
use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};

pub const SIGHASH_ALL: u32 = 1;

#[derive(Debug, Clone)]
pub struct Utxo {
    pub outpoint: OutPoint,
    pub value: u64,
    pub script: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BuildError {
    #[error("not enough coins: have {have}, need {need}")]
    InsufficientFunds { have: u64, need: u64 },
    #[error("payload too large for one OP_RETURN ({0} bytes)")]
    PayloadTooLarge(usize),
}

pub struct TxOutSpec {
    pub value: u64,
    pub script: Vec<u8>,
}

fn push(script: &mut Vec<u8>, data: &[u8]) {
    match data.len() {
        0..=75 => script.push(data.len() as u8),
        76..=255 => {
            script.push(0x4c);
            script.push(data.len() as u8);
        }
        _ => {
            script.push(0x4d);
            script.extend_from_slice(&(data.len() as u16).to_le_bytes());
        }
    }
    script.extend_from_slice(data);
}

pub fn op_return(data: &[u8]) -> Vec<u8> {
    let mut s = vec![0x6a];
    push(&mut s, data);
    s
}

/// Serialize with the given per-input scriptSigs (legacy, no witness).
fn serialize(p: &ChainParams, version: i32, time: u32, ins: &[(OutPoint, Vec<u8>)], outs: &[TxOutSpec]) -> Vec<u8> {
    let mut o = Vec::new();
    o.extend_from_slice(&version.to_le_bytes());
    if p.format.tx_time {
        o.extend_from_slice(&time.to_le_bytes());
    }
    write_varint(&mut o, ins.len() as u64);
    for (op, ss) in ins {
        o.extend_from_slice(&op.txid.0);
        o.extend_from_slice(&op.vout.to_le_bytes());
        write_varint(&mut o, ss.len() as u64);
        o.extend_from_slice(ss);
        o.extend_from_slice(&u32::MAX.to_le_bytes());
    }
    write_varint(&mut o, outs.len() as u64);
    for out in outs {
        o.extend_from_slice(&out.value.to_le_bytes());
        write_varint(&mut o, out.script.len() as u64);
        o.extend_from_slice(&out.script);
    }
    o.extend_from_slice(&0u32.to_le_bytes());
    o
}

/// Sign every input (all P2PKH, all spendable by `key`) with SIGHASH_ALL.
pub fn sign_p2pkh(p: &ChainParams, utxos: &[Utxo], outs: &[TxOutSpec], key: &SecretKey, time: u32) -> Tx {
    let pk: PublicKey = PublicKey::from_secret_key(SECP256K1, key);
    let mut sigs = Vec::with_capacity(utxos.len());
    for i in 0..utxos.len() {
        let ins: Vec<(OutPoint, Vec<u8>)> = utxos
            .iter()
            .enumerate()
            .map(|(j, u)| (u.outpoint, if i == j { u.script.clone() } else { vec![] }))
            .collect();
        let mut pre = serialize(p, 1, time, &ins, outs);
        pre.extend_from_slice(&SIGHASH_ALL.to_le_bytes());
        let digest = sha256d(&pre);
        let sig = SECP256K1.sign_ecdsa(&Message::from_digest(digest.0), key);
        let mut der = sig.serialize_der().to_vec();
        der.push(SIGHASH_ALL as u8);
        let mut ss = Vec::new();
        push(&mut ss, &der);
        push(&mut ss, &pk.serialize());
        sigs.push(ss);
    }
    let ins: Vec<(OutPoint, Vec<u8>)> = utxos.iter().zip(sigs).map(|(u, s)| (u.outpoint, s)).collect();
    Tx::parse_with(&serialize(p, 1, time, &ins, outs), &p.format).expect("we just built it")
}

/// Verify a P2PKH input signature we produced (self-check before broadcasting).
pub fn verify_p2pkh_input(p: &ChainParams, tx: &Tx, utxos: &[Utxo], i: usize) -> bool {
    let pushes = arcade_ledger::script::pushes(&tx.inputs[i].script_sig);
    if pushes.len() != 2 {
        return false;
    }
    let (sig, pk) = (pushes[0], pushes[1]);
    let Ok(pk) = PublicKey::from_slice(pk) else { return false };
    if arcade_ledger::address::p2pkh_script(&arcade_ledger::address::hash160(&pk.serialize())) != utxos[i].script {
        return false;
    }
    let ins: Vec<(OutPoint, Vec<u8>)> = utxos
        .iter()
        .enumerate()
        .map(|(j, u)| (u.outpoint, if i == j { u.script.clone() } else { vec![] }))
        .collect();
    let outs: Vec<TxOutSpec> =
        tx.outputs.iter().map(|o| TxOutSpec { value: o.value, script: o.script_pubkey.clone() }).collect();
    let mut pre = serialize(p, tx.version, tx.time.unwrap_or(0), &ins, &outs);
    pre.extend_from_slice(&SIGHASH_ALL.to_le_bytes());
    let digest = sha256d(&pre);
    let Some((&ht, der)) = sig.split_last() else { return false };
    if ht as u32 != SIGHASH_ALL {
        return false;
    }
    let Ok(sig) = secp256k1::ecdsa::Signature::from_der(der) else { return false };
    SECP256K1.verify_ecdsa(&Message::from_digest(digest.0), &sig, &pk).is_ok()
}

/// Estimated size of a P2PKH tx (compressed keys).
pub fn estimate_size(n_in: usize, out_scripts: &[usize]) -> usize {
    10 + 148 * n_in + out_scripts.iter().map(|l| 9 + l).sum::<usize>()
}

/// Build the @tag claim: class C, first input owned by the tag key (the sender),
/// OP_RETURN "arcd"+payload, change back to the same address (the only reference).
pub fn build_claim(
    p: &ChainParams,
    payload: &[u8],
    key: &SecretKey,
    coins: &[Utxo],
    fee_per_kb: u64,
    dust: u64,
    time: u32,
) -> Result<Tx, BuildError> {
    let mut data = MARKER.to_vec();
    data.extend_from_slice(payload);
    if data.len() > 80 {
        return Err(BuildError::PayloadTooLarge(data.len()));
    }
    let pk = PublicKey::from_secret_key(SECP256K1, key);
    let change_script =
        arcade_ledger::address::p2pkh_script(&arcade_ledger::address::hash160(&pk.serialize()));
    let ret = op_return(&data);

    // Largest coins first until fee is covered.
    let mut sorted: Vec<&Utxo> = coins.iter().filter(|u| u.script == change_script).collect();
    sorted.sort_by(|a, b| b.value.cmp(&a.value));
    let mut chosen: Vec<Utxo> = Vec::new();
    let mut have = 0u64;
    let fee_for = |n_in: usize| (estimate_size(n_in, &[ret.len(), 25]) as u64 * fee_per_kb).div_ceil(1000);
    for u in sorted {
        chosen.push(u.clone());
        have += u.value;
        if have >= fee_for(chosen.len()) + dust {
            break;
        }
    }
    let fee = fee_for(chosen.len().max(1));
    if chosen.is_empty() || have < fee + dust {
        return Err(BuildError::InsufficientFunds { have, need: fee + dust });
    }
    let outs = vec![
        TxOutSpec { value: 0, script: ret },
        TxOutSpec { value: have - fee, script: change_script },
    ];
    Ok(sign_p2pkh(p, &chosen, &outs, key, time))
}

/// Pay `amount` to `to_script` from coins of `key`'s P2PKH address, change back to it.
pub fn build_send(
    p: &ChainParams,
    to_script: &[u8],
    amount: u64,
    key: &SecretKey,
    coins: &[Utxo],
    fee_per_kb: u64,
    dust: u64,
    time: u32,
) -> Result<Tx, BuildError> {
    let pk = PublicKey::from_secret_key(SECP256K1, key);
    let change_script =
        arcade_ledger::address::p2pkh_script(&arcade_ledger::address::hash160(&pk.serialize()));
    let mut sorted: Vec<&Utxo> = coins.iter().filter(|u| u.script == change_script).collect();
    sorted.sort_by(|a, b| b.value.cmp(&a.value));
    let fee_for = |n_in: usize| (estimate_size(n_in, &[to_script.len(), 25]) as u64 * fee_per_kb).div_ceil(1000);
    let mut chosen: Vec<Utxo> = Vec::new();
    let mut have = 0u64;
    for u in sorted {
        chosen.push(u.clone());
        have += u.value;
        if have >= amount + fee_for(chosen.len()) {
            break;
        }
    }
    let fee = fee_for(chosen.len().max(1));
    if chosen.is_empty() || have < amount + fee {
        return Err(BuildError::InsufficientFunds { have, need: amount + fee });
    }
    let mut outs = vec![TxOutSpec { value: amount, script: to_script.to_vec() }];
    let change = have - amount - fee;
    if change >= dust {
        outs.push(TxOutSpec { value: change, script: change_script });
    }
    Ok(sign_p2pkh(p, &chosen, &outs, key, time))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::{seed_from_phrase, tag_key};
    use arcade_chain::params::{KOTIA_MAIN, PEPECOIN_TEST};
    use arcade_chain::Hash256;
    use arcade_ledger::extract::{extract, Class, Prevout};
    use arcade_ledger::tags;

    fn key() -> SecretKey {
        let seed = seed_from_phrase(
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about",
        )
        .unwrap();
        tag_key(&seed[..], 1, 0).unwrap().key
    }

    #[test]
    fn claim_tx_is_read_back_by_the_ledger() {
        let p = &PEPECOIN_TEST;
        let k = key();
        let pk = PublicKey::from_secret_key(SECP256K1, &k);
        let script = crate::keys::p2pkh_script_for(&pk);
        let coins = vec![Utxo { outpoint: OutPoint { txid: Hash256([5; 32]), vout: 1 }, value: 100_000_000, script: script.clone() }];
        let tx = build_claim(p, &tags::claim_payload("martin"), &k, &coins, 1_000_000, 100_000, 0).unwrap();
        assert!(verify_p2pkh_input(p, &tx, &coins, 0), "signature verifies");

        let prev = vec![Prevout { value: coins[0].value, script }];
        let atx = extract(&tx, &prev, p).unwrap().unwrap();
        let me = crate::keys::p2pkh_address(&pk, p.p2pkh_version);
        assert_eq!(atx.class, Class::C);
        assert_eq!(atx.sender, me);
        assert_eq!(atx.reference.as_deref(), Some(me.as_str()));
        assert_eq!(tags::parse(&atx.payload), Some(Ok(tags::TagOp::Claim("martin".into()))));
        let fee = 100_000_000 - tx.outputs[1].value;
        assert!(fee > 0 && fee < 1_000_000, "fee {fee}");
    }

    #[test]
    fn kotia_tx_carries_time_and_verifies() {
        let p = &KOTIA_MAIN;
        let k = key();
        let script = crate::keys::p2pkh_script_for(&PublicKey::from_secret_key(SECP256K1, &k));
        let coins = vec![Utxo { outpoint: OutPoint { txid: Hash256([6; 32]), vout: 0 }, value: 10_000_000, script }];
        let tx = build_claim(p, &tags::claim_payload("abc"), &k, &coins, 10_000, 100_000, 1792790000).unwrap();
        assert_eq!(tx.time, Some(1792790000));
        assert!(verify_p2pkh_input(p, &tx, &coins, 0));
    }

    #[test]
    fn send_pays_and_changes() {
        let p = &PEPECOIN_TEST;
        let k = key();
        let me = crate::keys::p2pkh_script_for(&PublicKey::from_secret_key(SECP256K1, &k));
        let to = arcade_ledger::address::p2pkh_script(&[3; 20]);
        let coins = vec![Utxo { outpoint: OutPoint { txid: Hash256([5; 32]), vout: 0 }, value: 500_000_000, script: me.clone() }];
        let tx = build_send(p, &to, 100_000_000, &k, &coins, 1_000_000, 1_000_000, 0).unwrap();
        assert!(verify_p2pkh_input(p, &tx, &coins, 0));
        assert_eq!(tx.outputs[0].value, 100_000_000);
        assert_eq!(tx.outputs[0].script_pubkey, to);
        assert_eq!(tx.outputs[1].script_pubkey, me);
        assert!(build_send(p, &to, 600_000_000, &k, &coins, 1_000_000, 1_000_000, 0).is_err());
    }

    #[test]
    fn not_enough_coins() {
        let p = &PEPECOIN_TEST;
        let k = key();
        assert!(matches!(
            build_claim(p, &tags::claim_payload("abc"), &k, &[], 1_000_000, 100_000, 0),
            Err(BuildError::InsufficientFunds { .. })
        ));
    }
}
