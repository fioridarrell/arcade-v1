//! @tag claims and transfers (tags.py; state.py:949-958, 1374-1412).
//!
//! Wire: AnyData payload (version u16 BE, type u16 BE = 200) whose data is
//! "INSC" + [1, kind, len] + utf-8 tag; kind 3 = claim, 4 = transfer.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const TYPE_ANY_DATA: u16 = 200;
pub const KIND_CLAIM: u8 = 3;
pub const KIND_TRANSFER: u8 = 4;
pub const MIN_LENGTH: usize = 2;
pub const MAX_LENGTH: usize = 24;
pub const RESERVED: &[&str] = &[
    "admin", "administrator", "arcade", "dogecoinarcade", "support", "help", "official", "system",
    "root", "moderator", "mod", "staff", "team", "pepecoin", "dogecoin", "null", "none",
    "anonymous", "me", "you",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TagOp {
    Claim(String),
    Transfer(String),
}

/// Why a tag is not acceptable, or None if it is.
pub fn tag_problem(tag: &str) -> Option<&'static str> {
    if tag.len() < MIN_LENGTH {
        return Some("too short");
    }
    if tag.len() > MAX_LENGTH {
        return Some("too long");
    }
    if !tag.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        return Some("only a-z, 0-9 and _");
    }
    if RESERVED.contains(&tag) {
        return Some("reserved");
    }
    None
}

/// Parse an arcade payload as a tag op. None = not a tag message;
/// Some(Err) = a tag message that is invalid (recorded as invalid).
pub fn parse(payload: &[u8]) -> Option<Result<TagOp, &'static str>> {
    if payload.len() < 4 || u16::from_be_bytes([payload[2], payload[3]]) != TYPE_ANY_DATA {
        return None;
    }
    let d = &payload[4..];
    if d.len() < 6 || &d[..4] != b"INSC" || d[4] != 1 {
        return None;
    }
    let kind = d[5];
    if kind != KIND_CLAIM && kind != KIND_TRANSFER {
        return None;
    }
    Some((|| {
        let len = *d.get(6).ok_or("truncated")? as usize;
        let raw = d.get(7..7 + len).ok_or("truncated")?;
        let tag = std::str::from_utf8(raw).map_err(|_| "not utf-8")?.to_string();
        if let Some(p) = tag_problem(&tag) {
            return Err(p);
        }
        Ok(if kind == KIND_CLAIM { TagOp::Claim(tag) } else { TagOp::Transfer(tag) })
    })())
}

/// Build the payload for a claim (what the wallet puts after the "arcd" marker).
pub fn claim_payload(tag: &str) -> Vec<u8> {
    let mut p = vec![0, 0];
    p.extend_from_slice(&TYPE_ANY_DATA.to_be_bytes());
    p.extend_from_slice(b"INSC");
    p.extend_from_slice(&[1, KIND_CLAIM, tag.len() as u8]);
    p.extend_from_slice(tag.as_bytes());
    p
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagRow {
    pub address: String,
    pub claimed_txid: String,
    pub block_height: u32,
    pub position: u32,
}

/// Storage for the tag table: tag PRIMARY KEY, address UNIQUE.
pub trait TagStore {
    type E;
    fn tag(&self, tag: &str) -> Result<Option<TagRow>, Self::E>;
    fn tag_of(&self, address: &str) -> Result<Option<String>, Self::E>;
    fn set(&mut self, tag: &str, row: TagRow) -> Result<(), Self::E>;
    fn remove(&mut self, tag: &str, address: &str) -> Result<(), Self::E>;
}

/// Apply one op from `sender` (with `reference`). Ok(Err) = invalid, nothing changed.
pub fn apply<S: TagStore>(
    s: &mut S,
    op: &TagOp,
    sender: &str,
    reference: Option<&str>,
    txid: &str,
    height: u32,
    position: u32,
) -> Result<Result<(), &'static str>, S::E> {
    let row = |address: &str| TagRow { address: address.into(), claimed_txid: txid.into(), block_height: height, position };
    match op {
        TagOp::Claim(t) => {
            if let Some(r) = s.tag(t)? {
                return Ok(Err(if r.address == sender { "already yours" } else { "taken" }));
            }
            if let Some(old) = s.tag_of(sender)? {
                s.remove(&old, sender)?;
            }
            s.set(t, row(sender))?;
            Ok(Ok(()))
        }
        TagOp::Transfer(t) => {
            match s.tag(t)? {
                Some(r) if r.address == sender => {}
                _ => return Ok(Err("not yours")),
            }
            let Some(to) = reference else { return Ok(Err("no recipient")) };
            if to == sender {
                return Ok(Err("to yourself"));
            }
            if s.tag_of(to)?.is_some() {
                return Ok(Err("recipient already has a tag"));
            }
            s.remove(t, sender)?;
            s.set(t, row(to))?;
            Ok(Ok(()))
        }
    }
}

/// names_digest over rows in tag order: sha256 of "{tag}|{address}|{block_height}\n".
pub fn digest<'a>(rows: impl Iterator<Item = (&'a str, &'a TagRow)>) -> String {
    let mut h = Sha256::new();
    for (t, r) in rows {
        h.update(format!("{t}|{}|{}\n", r.address, r.block_height).as_bytes());
    }
    hex::encode(h.finalize())
}

/// In-memory tag table (replays, tests).
#[derive(Debug, Default, Clone)]
pub struct Tags {
    pub by_tag: BTreeMap<String, TagRow>,
    pub by_addr: BTreeMap<String, String>,
}

impl TagStore for Tags {
    type E = std::convert::Infallible;
    fn tag(&self, t: &str) -> Result<Option<TagRow>, Self::E> {
        Ok(self.by_tag.get(t).cloned())
    }
    fn tag_of(&self, a: &str) -> Result<Option<String>, Self::E> {
        Ok(self.by_addr.get(a).cloned())
    }
    fn set(&mut self, t: &str, row: TagRow) -> Result<(), Self::E> {
        self.by_addr.insert(row.address.clone(), t.into());
        self.by_tag.insert(t.into(), row);
        Ok(())
    }
    fn remove(&mut self, t: &str, a: &str) -> Result<(), Self::E> {
        self.by_tag.remove(t);
        self.by_addr.remove(a);
        Ok(())
    }
}

impl Tags {
    pub fn apply(&mut self, op: &TagOp, sender: &str, reference: Option<&str>, txid: &str, height: u32, position: u32) -> Result<(), &'static str> {
        match apply(self, op, sender, reference, txid, height, position) {
            Ok(r) => r,
            Err(never) => match never {},
        }
    }

    pub fn digest(&self) -> String {
        digest(self.by_tag.iter().map(|(t, r)| (t.as_str(), r)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_claim_roundtrip_and_rules() {
        assert_eq!(parse(&claim_payload("martin")), Some(Ok(TagOp::Claim("martin".into()))));
        assert_eq!(parse(&claim_payload("Martin")), Some(Err("only a-z, 0-9 and _")));
        assert_eq!(parse(&claim_payload("a")), Some(Err("too short")));
        assert_eq!(parse(&claim_payload("admin")), Some(Err("reserved")));
        let mut t = claim_payload("abc");
        t.truncate(t.len() - 1);
        assert_eq!(parse(&t), Some(Err("truncated")));
        assert_eq!(parse(&[0, 0, 0, 0]), None);
    }

    #[test]
    fn claim_and_transfer_rules() {
        let mut t = Tags::default();
        let c = |s: &str| TagOp::Claim(s.into());
        t.apply(&c("alice"), "A", None, "t1", 10, 1).unwrap();
        assert_eq!(t.apply(&c("alice"), "A", None, "t2", 11, 1), Err("already yours"));
        assert_eq!(t.apply(&c("alice"), "B", None, "t3", 11, 2), Err("taken"));
        // A claims a new tag: the old one is released.
        t.apply(&c("alice2"), "A", None, "t4", 12, 1).unwrap();
        assert!(!t.by_tag.contains_key("alice"));
        t.apply(&c("bob"), "B", None, "t5", 13, 1).unwrap();
        let tr = TagOp::Transfer("alice2".into());
        assert_eq!(t.apply(&tr, "A", Some("B"), "t6", 14, 1), Err("recipient already has a tag"));
        t.apply(&tr, "A", Some("C"), "t7", 14, 2).unwrap();
        assert_eq!(t.by_addr.get("C").map(String::as_str), Some("alice2"));
        assert!(!t.by_addr.contains_key("A"));
        // Empty table digest is sha256("").
        assert_eq!(
            Tags::default().digest(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }
}
