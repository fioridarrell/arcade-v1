//! The legacy signature hash for any input, with the three hash types pre-signed trades use
//! (the original arcade's `funding.sighash`, byte for byte):
//!
//! - ALL: every input and every output;
//! - ALL|ANYONECANPAY: this input alone, and every output (a bid's coins: whoever completes the
//!   transaction may add inputs, never change what it pays);
//! - SINGLE|ANYONECANPAY: this input alone, and the output standing at its own index, the outputs
//!   before it empty at value -1 (a seller's leg: their piece for their price).
//!
//! A SINGLE whose index runs past the outputs is refused: the legacy rule hashes a constant then,
//! and a signature over a constant is good for every transaction. NONE is not offered.
//! Any input may be spent by any script: the script code is the coin's script (P2PKH) or the
//! redeem script (P2SH). Sequences and the lock time are the transaction's own, and a chain that
//! writes a transaction time signs it right after the version.

use arcade_chain::encode::write_varint;
use arcade_chain::hash::sha256d;
use arcade_chain::OutPoint;
use secp256k1::{ecdsa::Signature, Message, PublicKey, SecretKey, SECP256K1};

pub const ALL: u32 = 0x01;
pub const ALL_ANYONECANPAY: u32 = 0x81;
pub const SINGLE_ANYONECANPAY: u32 = 0x83;

/// One input as signed: the coin it spends and its sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct In {
    pub prevout: OutPoint,
    pub sequence: u32,
}

/// A transaction's shape, apart from its scriptSigs.
#[derive(Debug, Clone)]
pub struct Shape {
    pub version: i32,
    /// The transaction time, on a chain whose transactions carry one.
    pub time: Option<u32>,
    pub inputs: Vec<In>,
    pub outputs: Vec<(u64, Vec<u8>)>,
    pub lock_time: u32,
}

/// The 32 bytes input `index` signs, spending a coin whose script code is `script`.
pub fn sighash(tx: &Shape, index: usize, script: &[u8], hash_type: u32) -> Result<[u8; 32], String> {
    if index >= tx.inputs.len() {
        return Err(format!("there is no input {index}"));
    }
    let null: (u64, Vec<u8>) = (u64::MAX, Vec::new());
    let (inputs, signed_at, outs): (&[In], usize, Vec<&(u64, Vec<u8>)>) = match hash_type {
        ALL => (&tx.inputs, index, tx.outputs.iter().collect()),
        ALL_ANYONECANPAY => (&tx.inputs[index..=index], 0, tx.outputs.iter().collect()),
        SINGLE_ANYONECANPAY => {
            if index >= tx.outputs.len() {
                return Err(format!("input {index} has no output {index} to commit to: a signature over that would authorise every transaction"));
            }
            let mut o: Vec<&(u64, Vec<u8>)> = vec![&null; index];
            o.push(&tx.outputs[index]);
            (&tx.inputs[index..=index], 0, o)
        }
        t => return Err(format!("signing is offered for ALL, ALL|ANYONECANPAY and SINGLE|ANYONECANPAY, not {t:#04x}")),
    };
    let mut raw = Vec::new();
    raw.extend_from_slice(&tx.version.to_le_bytes());
    if let Some(t) = tx.time {
        raw.extend_from_slice(&t.to_le_bytes());
    }
    write_varint(&mut raw, inputs.len() as u64);
    for (n, i) in inputs.iter().enumerate() {
        raw.extend_from_slice(&i.prevout.txid.0);
        raw.extend_from_slice(&i.prevout.vout.to_le_bytes());
        if n == signed_at {
            write_varint(&mut raw, script.len() as u64);
            raw.extend_from_slice(script);
        } else {
            write_varint(&mut raw, 0);
        }
        raw.extend_from_slice(&i.sequence.to_le_bytes());
    }
    write_varint(&mut raw, outs.len() as u64);
    for (value, s) in outs {
        raw.extend_from_slice(&value.to_le_bytes());
        write_varint(&mut raw, s.len() as u64);
        raw.extend_from_slice(s);
    }
    raw.extend_from_slice(&tx.lock_time.to_le_bytes());
    raw.extend_from_slice(&hash_type.to_le_bytes());
    Ok(sha256d(&raw).0)
}

/// A signature as a scriptSig carries it: DER, then the hash type byte.
pub fn sign(key: &SecretKey, digest: &[u8; 32], hash_type: u32) -> Vec<u8> {
    let mut der = SECP256K1.sign_ecdsa(&Message::from_digest(*digest), key).serialize_der().to_vec();
    der.push(hash_type as u8);
    der
}

/// Whether `sig` (DER + hash type byte) is `pubkey`'s over `digest`, with the hash type `want`.
pub fn verify(pubkey: &[u8], sig: &[u8], digest: &[u8; 32], want: u32) -> bool {
    let Some((&ht, der)) = sig.split_last() else { return false };
    if ht as u32 != want {
        return false;
    }
    let (Ok(pk), Ok(mut s)) = (PublicKey::from_slice(pubkey), Signature::from_der(der)) else { return false };
    s.normalize_s();
    SECP256K1.verify_ecdsa(&Message::from_digest(*digest), &s, &pk).is_ok()
}

/// The whole transaction, with these scriptSigs (one per input, in order).
pub fn serialize(tx: &Shape, script_sigs: &[Vec<u8>]) -> Vec<u8> {
    let mut o = Vec::new();
    o.extend_from_slice(&tx.version.to_le_bytes());
    if let Some(t) = tx.time {
        o.extend_from_slice(&t.to_le_bytes());
    }
    write_varint(&mut o, tx.inputs.len() as u64);
    for (i, inp) in tx.inputs.iter().enumerate() {
        o.extend_from_slice(&inp.prevout.txid.0);
        o.extend_from_slice(&inp.prevout.vout.to_le_bytes());
        let ss = script_sigs.get(i).map(Vec::as_slice).unwrap_or(&[]);
        write_varint(&mut o, ss.len() as u64);
        o.extend_from_slice(ss);
        o.extend_from_slice(&inp.sequence.to_le_bytes());
    }
    write_varint(&mut o, tx.outputs.len() as u64);
    for (v, s) in &tx.outputs {
        o.extend_from_slice(&v.to_le_bytes());
        write_varint(&mut o, s.len() as u64);
        o.extend_from_slice(s);
    }
    o.extend_from_slice(&tx.lock_time.to_le_bytes());
    o
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcade_chain::hash::Hash256;
    use serde_json::Value;

    fn shape_of(c: &Value) -> Shape {
        let inputs = c["inputs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| {
                let mut txid: [u8; 32] = hex::decode(i["txid"].as_str().unwrap()).unwrap().try_into().unwrap();
                txid.reverse();
                In { prevout: OutPoint { txid: Hash256(txid), vout: i["vout"].as_u64().unwrap() as u32 }, sequence: i["sequence"].as_u64().unwrap_or(0xffff_ffff) as u32 }
            })
            .collect();
        let outputs = c["outputs"].as_array().unwrap().iter().map(|o| (o[0].as_u64().unwrap(), hex::decode(o[1].as_str().unwrap()).unwrap())).collect();
        Shape { version: c["version"].as_i64().unwrap() as i32, time: c["ntime"].as_u64().map(|t| t as u32), inputs, outputs, lock_time: c["locktime"].as_u64().unwrap() as u32 }
    }

    #[test]
    fn equal_to_the_original_arcade_digest_for_digest() {
        let v: Value = serde_json::from_str(include_str!("../tests/data/sighash_vectors.json")).unwrap();
        let cases = v["cases"].as_array().unwrap();
        assert!(cases.len() >= 60);
        for (n, c) in cases.iter().enumerate() {
            let d = sighash(&shape_of(c), c["index"].as_u64().unwrap() as usize, &hex::decode(c["script"].as_str().unwrap()).unwrap(), c["type"].as_u64().unwrap() as u32).unwrap();
            assert_eq!(hex::encode(d), c["digest"].as_str().unwrap(), "case {n}");
        }
        assert_eq!(v["single_past_outputs_refused"], true);
    }

    #[test]
    fn single_past_the_outputs_and_other_types_are_refused() {
        let i = In { prevout: OutPoint { txid: Hash256([0; 32]), vout: 0 }, sequence: u32::MAX };
        let tx = Shape { version: 1, time: None, inputs: vec![i, i], outputs: vec![(1, vec![0x51])], lock_time: 0 };
        assert!(sighash(&tx, 1, &[0x51], SINGLE_ANYONECANPAY).is_err());
        assert!(sighash(&tx, 0, &[0x51], SINGLE_ANYONECANPAY).is_ok());
        for t in [0x02, 0x03, 0x82] {
            assert!(sighash(&tx, 0, &[0x51], t).is_err(), "{t:#x}");
        }
    }

    #[test]
    fn a_signature_checks_only_for_its_own_type_and_digest() {
        let key = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let pk = PublicKey::from_secret_key(SECP256K1, &key).serialize();
        let d = [9u8; 32];
        let s = sign(&key, &d, SINGLE_ANYONECANPAY);
        assert!(verify(&pk, &s, &d, SINGLE_ANYONECANPAY));
        assert!(!verify(&pk, &s, &d, ALL));
        assert!(!verify(&pk, &s, &[8u8; 32], SINGLE_ANYONECANPAY));
    }
}
