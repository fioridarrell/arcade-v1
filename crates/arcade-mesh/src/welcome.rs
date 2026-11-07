//! The "welcome" protocol (t:"wl"): how a new arcade asks the mesh for test coins.
//! Records are Ed25519-signed by the arcade's node key `a` over canonical JSON of the
//! record without `sig` (welcome.py). Kinds: ask {addr, tag}, sent {ask, txid, tag},
//! closed {block, height, txids}.

use crate::canon::canonical;
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde_json::{json, Map, Value};

pub const AMOUNT_COINS: u64 = 1; // 1 test PEP per ask
pub const ASK_DAYS: u64 = 14;
pub const SKEW_SECS: u64 = 600;

fn unsigned(rec: &Value) -> Value {
    let mut m: Map<String, Value> = rec.as_object().cloned().unwrap_or_default();
    m.remove("sig");
    Value::Object(m)
}

pub fn sign(mut rec: Value, key: &SigningKey) -> Value {
    let sig = key.sign(canonical(&unsigned(&rec)).as_bytes());
    rec["sig"] = json!(hex::encode(sig.to_bytes()));
    rec
}

/// True if `sig` is the record author's (`a`) signature over the canonical record.
pub fn verify(rec: &Value) -> bool {
    let (Some(a), Some(sig)) = (rec["a"].as_str(), rec["sig"].as_str()) else { return false };
    let (Ok(a), Ok(sig)) = (hex::decode(a), hex::decode(sig)) else { return false };
    let (Ok(a), Ok(sig)) = (<[u8; 32]>::try_from(a), <[u8; 64]>::try_from(sig)) else { return false };
    let Ok(vk) = VerifyingKey::from_bytes(&a) else { return false };
    vk.verify(canonical(&unsigned(rec)).as_bytes(), &ed25519_dalek::Signature::from_bytes(&sig)).is_ok()
}

/// Our ask: one per arcade, for `addr` (a Pepecoin testnet address), with the tag we want.
pub fn ask(key: &SigningKey, addr: &str, tag: &str, now: u64) -> Value {
    sign(
        json!({"a": hex::encode(key.verifying_key().to_bytes()), "addr": addr, "at": now, "k": "ask", "tag": tag}),
        key,
    )
}

pub fn frame(recs: Vec<Value>) -> crate::link::Frame {
    crate::link::Frame::new(json!({"t": "wl", "recs": recs}))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real ask seen on the live mesh, 2026-11-07: proves our canonical JSON matches.
    #[test]
    fn verifies_live_record() {
        let rec = json!({"a":"86e25ef9c6f1e5fc0d2aa53102828448bff0678de7c1b38e0d19a7e4dff7a9d5","addr":"nYW2BPLENpu2nGa7WCExvzxD3hQYueULFa","at":1793281039,"k":"ask","sig":"9f1ccf41ac9c772f586a7149abbc670d58ea5b7eedfcac9c433bc988d7a4d75ba58e3e47072e6581bf051053c84d7ba00059499522bf0f9c12360a19a38d3107","tag":"bigchiefenergy"});
        assert!(verify(&rec));
        let mut bad = rec.clone();
        bad["tag"] = json!("someoneelse");
        assert!(!verify(&bad));
    }

    #[test]
    fn our_ask_verifies() {
        let k = SigningKey::from_bytes(&[9; 32]);
        assert!(verify(&ask(&k, "nYW2BPLENpu2nGa7WCExvzxD3hQYueULFa", "martin", 1794083702)));
    }
}
