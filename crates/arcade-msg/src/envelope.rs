//! "arcm" envelopes and their encryption (messaging/envelope.py).
//!
//! inner      = crypto_box(sender_sk, rcpt_pk, bound_header || message), random nonce prepended
//! envelope   = sender_pub(32) || inner
//! ciphertext = crypto_box_seal(rcpt_pk, envelope)
//! SINGLE     = "arcm" 01 01 | clen u16 | ciphertext          (bound header: the first 6 bytes)
//! CHUNK      = "arcm" 01 02 | msg_id8 | clen u16 | countdown u16 | piece   (bound: 14 bytes)
//! API        = as SINGLE with type 6.
//! Nothing names the recipient: readers try to open every SINGLE/CHUNK/API payload.

use crypto_box::aead::{Aead, AeadCore};
use crypto_box::{PublicKey, SalsaBox, SecretKey};
use rand_core::OsRng;

pub const MAGIC: &[u8; 4] = b"arcm";
pub const VERSION: u8 = 1;
pub const SINGLE: u8 = 1;
pub const CHUNK: u8 = 2;
pub const KEY_ANNOUNCE: u8 = 3;
pub const GROUP: u8 = 4;
pub const GROUP_CHUNK: u8 = 5;
pub const API: u8 = 6;
pub const FEED_ACT: u8 = 7;
pub const RELEASE: u8 = 8;
pub const INSTANCE: u8 = 9;
pub const MESH: u8 = 10;
pub const GAMESTATE: u8 = 11;
pub const BROADCAST: u8 = 12;

/// Most arcm bytes one Class B transaction carries (7,650 less the AnyData header).
pub const MAX_PAYLOAD: usize = 7650 - 4;
/// Ciphertext bytes per chunk.
pub const CHUNK_CAPACITY: usize = 7650 - 4 - 18;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub kind: u8,
    pub msg_id: Option<[u8; 8]>,
    pub clen: Option<u16>,
    pub countdown: Option<u16>,
}

impl Header {
    /// What the inner box covers: magic, version, type (and msg_id for chunks).
    pub fn bound(&self) -> Vec<u8> {
        let mut b = MAGIC.to_vec();
        b.push(VERSION);
        b.push(self.kind);
        if let Some(id) = self.msg_id.filter(|_| self.kind == CHUNK || self.kind == GROUP_CHUNK) {
            b.extend_from_slice(&id);
        }
        b
    }

    /// Parse the clear header of a payload; returns the header and where the body starts.
    pub fn parse(p: &[u8]) -> Option<(Header, usize)> {
        if p.len() < 6 || &p[..4] != MAGIC || p[4] != VERSION {
            return None;
        }
        let kind = p[5];
        let u16at = |i: usize| p.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
        match kind {
            SINGLE | API => Some((Header { kind, msg_id: None, clen: Some(u16at(6)?), countdown: None }, 8)),
            CHUNK => Some((Header { kind, msg_id: Some(p.get(6..14)?.try_into().ok()?), clen: Some(u16at(14)?), countdown: Some(u16at(16)?) }, 18)),
            GROUP_CHUNK => Some((Header { kind, msg_id: Some(p.get(6..14)?.try_into().ok()?), countdown: Some(u16at(14)?), clen: Some(u16at(16)?) }, 18)),
            _ => Some((Header { kind, msg_id: None, clen: None, countdown: None }, 6)),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("not an arcm payload")]
    NotArcm,
    #[error("not for this key")]
    NotForUs,
    #[error("the sealed header does not match")]
    Header,
    #[error("too large for one message ({0} bytes)")]
    TooLarge(usize),
}

/// Seal `message` from `sender` to `rcpt` with the given bound header: the ciphertext.
pub fn seal(sender: &SecretKey, rcpt: &PublicKey, bound: &[u8], message: &[u8]) -> Vec<u8> {
    let b = SalsaBox::new(rcpt, sender);
    let nonce = SalsaBox::generate_nonce(&mut OsRng);
    let mut plain = bound.to_vec();
    plain.extend_from_slice(message);
    let ct = b.encrypt(&nonce, plain.as_slice()).expect("box");
    let mut env = sender.public_key().as_bytes().to_vec();
    env.extend_from_slice(&nonce);
    env.extend_from_slice(&ct);
    rcpt.seal(&mut OsRng, &env).expect("seal")
}

/// Open a ciphertext with our key: (sender's public key, message).
pub fn open(me: &SecretKey, bound: &[u8], ciphertext: &[u8]) -> Result<([u8; 32], Vec<u8>), Error> {
    let env = me.unseal(ciphertext).map_err(|_| Error::NotForUs)?;
    if env.len() < 32 + 24 + 16 {
        return Err(Error::NotForUs);
    }
    let sender: [u8; 32] = env[..32].try_into().unwrap();
    let b = SalsaBox::new(&PublicKey::from(sender), me);
    let nonce = crypto_box::Nonce::from_slice(&env[32..56]);
    let plain = b.decrypt(nonce, &env[56..]).map_err(|_| Error::NotForUs)?;
    if !plain.starts_with(bound) {
        return Err(Error::Header);
    }
    Ok((sender, plain[bound.len()..].to_vec()))
}

/// One SINGLE (or API, kind 6) payload: header(clen) || ciphertext.
pub fn single(kind: u8, sender: &SecretKey, rcpt: &PublicKey, message: &[u8]) -> Vec<u8> {
    let h = Header { kind, msg_id: None, clen: None, countdown: None };
    let ct = seal(sender, rcpt, &h.bound(), message);
    let mut p = h.bound();
    p.extend_from_slice(&(ct.len() as u16).to_be_bytes());
    p.extend_from_slice(&ct);
    p
}

/// The payloads carrying a private message: one SINGLE when it fits one transaction, else
/// CHUNKs (sealed once, the ciphertext split evenly; countdown reaches 0 on the last).
pub fn private(sender: &SecretKey, rcpt: &PublicKey, message: &[u8]) -> Vec<Vec<u8>> {
    // Seal overhead: 48 (seal) + 32 (sender) + 40 (nonce+MAC) + 6 (bound) = 126, plus 8 clear.
    if message.len() + 134 <= MAX_PAYLOAD {
        return vec![single(SINGLE, sender, rcpt, message)];
    }
    let mut id = [0u8; 8];
    rand_core::RngCore::fill_bytes(&mut OsRng, &mut id);
    chunks(CHUNK, id, &seal(sender, rcpt, &Header { kind: CHUNK, msg_id: Some(id), clen: None, countdown: None }.bound(), message))
}

/// Split bytes evenly into chunk payloads of `kind` (CHUNK or GROUP_CHUNK, whose header orders differ).
pub fn chunks(kind: u8, id: [u8; 8], data: &[u8]) -> Vec<Vec<u8>> {
    let count = data.len().div_ceil(CHUNK_CAPACITY).max(1);
    let even = data.len().div_ceil(count);
    data.chunks(even.max(1))
        .enumerate()
        .map(|(i, piece)| {
            let countdown = (count - 1 - i) as u16;
            let mut p = MAGIC.to_vec();
            p.push(VERSION);
            p.push(kind);
            p.extend_from_slice(&id);
            let (a, b) = if kind == CHUNK { (piece.len() as u16, countdown) } else { (countdown, piece.len() as u16) };
            p.extend_from_slice(&a.to_be_bytes());
            p.extend_from_slice(&b.to_be_bytes());
            p.extend_from_slice(piece);
            p
        })
        .collect()
}

/// A payload opened with our key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Opened {
    pub kind: u8,
    pub sender: [u8; 32],
    pub message: Vec<u8>,
}

/// Try to open a SINGLE or API payload (Class B padding after clen is ignored).
pub fn open_single(me: &SecretKey, payload: &[u8]) -> Result<Opened, Error> {
    let (h, at) = Header::parse(payload).ok_or(Error::NotArcm)?;
    if h.kind != SINGLE && h.kind != API {
        return Err(Error::NotArcm);
    }
    let clen = h.clen.unwrap() as usize;
    let ct = payload.get(at..at + clen).ok_or(Error::NotArcm)?;
    let (sender, message) = open(me, &h.bound(), ct)?;
    Ok(Opened { kind: h.kind, sender, message })
}

/// One chunk's parts: (msg_id, countdown, piece).
pub fn chunk_parts(payload: &[u8]) -> Option<([u8; 8], u16, Vec<u8>)> {
    let (h, at) = Header::parse(payload)?;
    if h.kind != CHUNK && h.kind != GROUP_CHUNK {
        return None;
    }
    let piece = payload.get(at..at + h.clen? as usize)?;
    Some((h.msg_id?, h.countdown?, piece.to_vec()))
}

/// Open a private message from its chunks (any order; all of them, countdown down to 0).
pub fn open_chunks(me: &SecretKey, id: [u8; 8], parts: &mut [(u16, Vec<u8>)]) -> Result<Opened, Error> {
    parts.sort_by(|a, b| b.0.cmp(&a.0));
    if parts.last().map(|p| p.0) != Some(0) || parts.first().map(|p| p.0 as usize + 1) != Some(parts.len()) {
        return Err(Error::NotArcm);
    }
    let ct: Vec<u8> = parts.iter().flat_map(|p| p.1.iter().copied()).collect();
    let bound = Header { kind: CHUNK, msg_id: Some(id), clen: None, countdown: None }.bound();
    let (sender, message) = open(me, &bound, &ct)?;
    Ok(Opened { kind: CHUNK, sender, message })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::MsgKeys;

    fn keys(phrase: &str) -> MsgKeys {
        MsgKeys::from_seed(&bip39::Mnemonic::parse(phrase).unwrap().to_seed(""))
    }
    const A: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about";
    const B: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";

    #[test]
    fn opens_the_old_arcades_message() {
        let a = keys(A);
        let p = hex::decode("6172636d0101008ac5916add42fcef56fc986fe956ec8b0e8619b834453b619d7d99604e8683643be5bf4be4a2447020497f474964f04e2fa3342df90a140c102e57f581fcd85393f8e8ad79abd812a105b55d2c3f38fada1a5faa39c78eab5dea4518f2c2eef0805cdcf526a2aae66c63d8a7ee99de208a0971f206c8580022af674059dfa14fa765169933cf87ff39aa21").unwrap();
        let o = open_single(&a.secret, &p).unwrap();
        assert_eq!(o.message, b"hello from B");
        assert_eq!(o.sender, keys(B).public_bytes());
        // Not for B.
        assert_eq!(open_single(&keys(B).secret, &p), Err(Error::NotForUs));
    }

    #[test]
    fn round_trip_and_overhead() {
        let (a, b) = (keys(A), keys(B));
        let p = single(SINGLE, &b.secret, &a.public, b"hello from B");
        assert_eq!(p.len(), 12 + 134);
        let mut padded = p.clone();
        padded.extend_from_slice(&[0; 9]); // Class B padding
        assert_eq!(open_single(&a.secret, &padded).unwrap().message, b"hello from B");
    }

    #[test]
    fn chunks_round_trip() {
        let (a, b) = (keys(A), keys(B));
        let big: Vec<u8> = (0..20_000u32).map(|i| (i % 251) as u8).collect();
        let parts = private(&b.secret, &a.public, &big);
        assert_eq!(parts.len(), 3);
        assert!(parts.iter().all(|p| p.len() <= MAX_PAYLOAD));
        let mut got: Vec<(u16, Vec<u8>)> = Vec::new();
        let mut id = [0; 8];
        for p in parts.iter().rev() {
            let (i, cd, piece) = chunk_parts(p).unwrap();
            id = i;
            got.push((cd, piece));
        }
        let o = open_chunks(&a.secret, id, &mut got).unwrap();
        assert_eq!((o.message, o.sender), (big, b.public_bytes()));
    }
}
