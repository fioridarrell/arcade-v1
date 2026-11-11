//! Writing a one-piece ord inscription the Pepecoin/Dogecoin scriptSig way (Pepinals, Doginals), as the ord index
//! (arcade-ledger ord.rs) reads it: a commit pays a P2SH lock of the key,
//!   redeem: <pubkey> OP_CHECKSIGVERIFY OP_DROP x (pushes) OP_TRUE
//! and the reveal spends it with  "ord" <1> <content type> <0> <body> <sig> <redeem>, the inscription landing on the
//! reveal's first output. One piece holds a body of up to 520 bytes (one push), which is every DRC-20 / PRC-20 note.

use crate::legacy::{self, In, Shape, ALL};
use crate::tx::{push, TxOutSpec, Utxo};
use arcade_chain::params::ChainParams;
use arcade_chain::{OutPoint, Tx};
use secp256k1::{PublicKey, SecretKey, SECP256K1};

pub const MAX_BODY: usize = 520;

fn envelope(ctype: &[u8], body: &[u8]) -> Vec<Vec<u8>> {
    vec![b"ord".to_vec(), vec![1], ctype.to_vec(), vec![], body.to_vec()]
}

fn push_item(s: &mut Vec<u8>, item: &[u8]) {
    match item {
        [] => s.push(0x00),
        [n] if (1..=16).contains(n) => s.push(0x50 + n),
        _ => push(s, item),
    }
}

/// The lock the commit pays: the key's signature, then every envelope push dropped, then true.
pub fn redeem(pk: &PublicKey, pushes: usize) -> Vec<u8> {
    let mut r = Vec::new();
    push(&mut r, &pk.serialize());
    r.push(0xad); // OP_CHECKSIGVERIFY
    let mut left = pushes;
    while left >= 2 {
        r.push(0x6d); // OP_2DROP
        left -= 2;
    }
    if left == 1 {
        r.push(0x75); // OP_DROP
    }
    r.push(0x51); // OP_TRUE
    r
}

/// The P2SH script a commit pays for this body, and its redeem script.
pub fn commit_lock(pk: &PublicKey, ctype: &str, body: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let r = redeem(pk, envelope(ctype.as_bytes(), body).len());
    (arcade_ledger::address::p2sh_script(&arcade_ledger::address::hash160(&r)), r)
}

/// The reveal of `body` spending the commit coin `commit` (worth `value`), paying `outputs` (the inscription lands
/// on the first output's first coin).
pub fn reveal(p: &ChainParams, key: &SecretKey, commit: OutPoint, ctype: &str, body: &[u8], outputs: Vec<(u64, Vec<u8>)>, time: u32) -> Result<Vec<u8>, String> {
    let pk = PublicKey::from_secret_key(SECP256K1, key);
    let env = envelope(ctype.as_bytes(), body);
    let r = redeem(&pk, env.len());
    let shape = Shape { version: 1, time: p.format.tx_time.then_some(time), inputs: vec![In { prevout: commit, sequence: 0xffff_ffff }], outputs, lock_time: 0 };
    let d = legacy::sighash(&shape, 0, &r, ALL)?;
    let mut ss = Vec::new();
    for item in &env {
        push_item(&mut ss, item);
    }
    push(&mut ss, &legacy::sign(key, &d, ALL));
    push(&mut ss, &r);
    Ok(legacy::serialize(&shape, &[ss]))
}

pub struct Inscription {
    pub commit: Tx,
    pub reveal: Vec<u8>,
    pub reveal_txid: String,
}

/// Inscribe `body` (one piece) from `key`'s P2PKH coins, the inscription going to `to` with `value`; the reveal
/// pays `reveal_fee`. Coins are the key's own, sorted largest first; change goes back to the key.
pub fn inscribe(p: &ChainParams, key: &SecretKey, coins: &[Utxo], ctype: &str, body: &[u8], to: &[u8], value: u64, reveal_fee: u64, fee_per_kb: u64, dust: u64, time: u32) -> Result<Inscription, String> {
    if body.is_empty() || body.len() > MAX_BODY {
        return Err(format!("a one-piece inscription holds 1 to {MAX_BODY} bytes"));
    }
    let pk = PublicKey::from_secret_key(SECP256K1, key);
    let env = envelope(ctype.as_bytes(), body);
    let r = redeem(&pk, env.len());
    let lock = arcade_ledger::address::p2sh_script(&arcade_ledger::address::hash160(&r));
    let commit = crate::tx::build_send(p, &lock, value + reveal_fee, key, coins, fee_per_kb, dust, time).map_err(|e| e.to_string())?;
    let vout = commit.outputs.iter().position(|o| o.script_pubkey == lock).ok_or("the commit doesn't pay the lock")? as u32;
    let shape = Shape {
        version: 1,
        time: p.format.tx_time.then_some(time),
        inputs: vec![In { prevout: OutPoint { txid: commit.txid, vout }, sequence: 0xffff_ffff }],
        outputs: vec![(value, to.to_vec())],
        lock_time: 0,
    };
    let d = legacy::sighash(&shape, 0, &r, ALL)?;
    let mut ss = Vec::new();
    for item in &env {
        push_item(&mut ss, item);
    }
    push(&mut ss, &legacy::sign(key, &d, ALL));
    push(&mut ss, &r);
    let reveal = legacy::serialize(&shape, &[ss]);
    let txid = Tx::parse_with(&reveal, &p.format).map_err(|e| e.to_string())?.txid.to_hex();
    let _ = TxOutSpec { value, script: to.to_vec() };
    let _ = Utxo { outpoint: OutPoint { txid: commit.txid, vout }, value: value + reveal_fee, script: lock };
    Ok(Inscription { commit, reveal, reveal_txid: txid })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ord index reads the reveal back: one piece, its content type and body.
    #[test]
    fn the_ord_index_reads_the_reveal() {
        let p = &arcade_chain::params::PEPECOIN_TEST;
        let key = SecretKey::from_slice(&[3; 32]).unwrap();
        let pk = PublicKey::from_secret_key(SECP256K1, &key);
        let own = arcade_ledger::address::p2pkh_script(&arcade_ledger::address::hash160(&pk.serialize()));
        let coins = vec![Utxo { outpoint: OutPoint { txid: arcade_chain::Hash256([4; 32]), vout: 0 }, value: 1_000_000_000, script: own.clone() }];
        let body = br#"{"p":"prc-20","op":"mint","tick":"abcd","amt":"100"}"#;
        let i = inscribe(p, &key, &coins, "text/plain;charset=utf-8", body, &own, 1_000_000, 1_000_000, 1_000_000, 1_000_000, 0).unwrap();
        let tx = Tx::parse_with(&i.reveal, &p.format).unwrap();
        let (n, ct, r) = arcade_ledger::ord::start(&tx.inputs[0].script_sig, arcade_ledger::ord::Dialect::Pepecoin).unwrap();
        assert_eq!((n, ct.as_slice(), r.left, r.body.as_slice()), (1, b"text/plain;charset=utf-8".as_slice(), 0, body.as_slice()));
    }
}
