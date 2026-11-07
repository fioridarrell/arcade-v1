//! Find arcade transactions and read their payload, sender and reference, exactly as
//! the existing arcade's tx.py / encoding.py do (docs/compat/SPEC.md §4a).

use crate::address::{address_of, base58check, hash160, sender_address_of};
use crate::script::{classify, op_return_data, ScriptKind};
use arcade_chain::params::ChainParams;
use arcade_chain::Tx;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const MARKER: &[u8; 4] = b"arcd";
pub const CLASS_B_MARKER_SEED: &[u8] = b"DogecoinArcade Class B marker v1";

/// The class B marker address for a chain.
pub fn class_b_marker(p: &ChainParams) -> String {
    base58check(p.p2pkh_version, &hash160(CLASS_B_MARKER_SEED))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    B,
    C,
}

impl Class {
    pub fn as_str(self) -> &'static str {
        match self {
            Class::B => "B",
            Class::C => "C",
        }
    }
}

/// What an input spent: its value and scriptPubKey.
#[derive(Debug, Clone)]
pub struct Prevout {
    pub value: u64,
    pub script: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArcadeTx {
    pub class: Class,
    pub sender: String,
    pub reference: Option<String>,
    pub payload: Vec<u8>,
}

/// Marked but unreadable: skipped with nothing recorded (indexer.py TxError).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct TxError(pub &'static str);

pub fn detect_class(tx: &Tx, marker: &str, p: &ChainParams) -> Option<Class> {
    for o in &tx.outputs {
        if let Some(d) = op_return_data(&o.script_pubkey) {
            if d.starts_with(MARKER) {
                return Some(Class::C);
            }
        }
    }
    let to_marker = tx.outputs.iter().any(|o| {
        matches!(classify(&o.script_pubkey), ScriptKind::P2pkh(_))
            && address_of(&o.script_pubkey, p).as_deref() == Some(marker)
    });
    let multisig = tx.outputs.iter().any(|o| matches!(classify(&o.script_pubkey), ScriptKind::Multisig { .. }));
    (to_marker && multisig).then_some(Class::B)
}

fn sender(tx: &Tx, class: Class, prev: &[Prevout], p: &ChainParams) -> Result<String, TxError> {
    if tx.is_coinbase() || tx.inputs.is_empty() || prev.len() != tx.inputs.len() {
        return Err(TxError("no usable inputs"));
    }
    match class {
        Class::C => sender_address_of(&prev[0].script, p).ok_or(TxError("first input has no address")),
        Class::B => {
            let mut sums: BTreeMap<String, u64> = BTreeMap::new();
            for pv in prev {
                let a = sender_address_of(&pv.script, p).ok_or(TxError("class B input is not P2PKH/P2SH"))?;
                *sums.entry(a).or_default() += pv.value;
            }
            // Iterate sorted, replace only on strictly greater: ties go to the smaller address.
            let mut best: Option<(&String, u64)> = None;
            for (a, v) in &sums {
                if best.map(|(_, bv)| *v > bv).unwrap_or(true) {
                    best = Some((a, *v));
                }
            }
            Ok(best.unwrap().0.clone())
        }
    }
}

fn reference(tx: &Tx, sender: &str, marker: &str, p: &ChainParams) -> Option<String> {
    let cands: Vec<String> = tx
        .outputs
        .iter()
        .filter_map(|o| address_of(&o.script_pubkey, p))
        .filter(|a| a != marker)
        .collect();
    match cands.len() {
        0 => None,
        1 => Some(cands[0].clone()),
        _ => {
            let mut rest = cands;
            if let Some(i) = rest.iter().position(|a| a == sender) {
                rest.remove(i);
            }
            rest.last().cloned()
        }
    }
}

fn class_c_payload(tx: &Tx) -> Vec<u8> {
    let mut out = Vec::new();
    for o in &tx.outputs {
        if let Some(d) = op_return_data(&o.script_pubkey) {
            if d.starts_with(MARKER) {
                out.extend_from_slice(&d[4..]);
            }
        }
    }
    out
}

fn class_b_payload(tx: &Tx, sender: &str) -> Result<Vec<u8>, TxError> {
    let mut keys: Vec<Vec<u8>> = Vec::new();
    for o in &tx.outputs {
        if let ScriptKind::Multisig { keys: k, .. } = classify(&o.script_pubkey) {
            keys.extend(k.into_iter().skip(1));
        }
    }
    if keys.len() > 255 {
        keys.truncate(255);
    }
    let mut out = Vec::with_capacity(keys.len() * 30);
    let mut h: [u8; 32] = Sha256::digest(sender.as_bytes()).into();
    for (i, k) in keys.iter().enumerate() {
        let seq = (i + 1) as u8;
        if i > 0 {
            h = Sha256::digest(hex::encode_upper(h).as_bytes()).into();
        }
        let mut packet = [0u8; 31];
        for j in 0..31 {
            packet[j] = k[1 + j] ^ h[j];
        }
        if packet[0] != seq {
            return Err(TxError("class B packet out of sequence"));
        }
        out.extend_from_slice(&packet[1..31]);
    }
    if out.is_empty() {
        return Err(TxError("class B with no data"));
    }
    Ok(out)
}

/// None = not an arcade transaction. Err = marked but unreadable (skip, record nothing).
pub fn extract(tx: &Tx, prev: &[Prevout], p: &ChainParams) -> Option<Result<ArcadeTx, TxError>> {
    let marker = class_b_marker(p);
    let class = detect_class(tx, &marker, p)?;
    Some((|| {
        let sender = sender(tx, class, prev, p)?;
        let payload = match class {
            Class::C => class_c_payload(tx),
            Class::B => class_b_payload(tx, &sender)?,
        };
        let reference = reference(tx, &sender, &marker, p);
        Ok(ArcadeTx { class, sender, reference, payload })
    })())
}

/// Encode a class B payload (used by tests and, later, the wallet's tx builder).
pub fn class_b_keys(payload: &[u8], sender: &str) -> Vec<[u8; 33]> {
    let mut h: [u8; 32] = Sha256::digest(sender.as_bytes()).into();
    payload
        .chunks(30)
        .enumerate()
        .map(|(i, chunk)| {
            if i > 0 {
                h = Sha256::digest(hex::encode_upper(h).as_bytes()).into();
            }
            let mut packet = [0u8; 31];
            packet[0] = (i + 1) as u8;
            packet[1..1 + chunk.len()].copy_from_slice(chunk);
            let mut key = [0u8; 33];
            key[0] = 0x02;
            for j in 0..31 {
                key[1 + j] = packet[j] ^ h[j];
            }
            key
        })
        .collect()
}
