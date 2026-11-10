//! Block maps on ord ("<block>.pepemap", "<block>.dogemap", "<block>.bitmap", ...). The old
//! arcades only kept these out of the names; this index is new (V1.0) and follows the Bitmap
//! rules: the body is exactly "<n>.<namespace>" (text/plain, UTF-8, no whitespace, no leading
//! zeros, lowercase namespace ending in map/maps/mep), n is at most the height of the block the
//! claim was inscribed in, and the first claim (lowest inscription number) wins. The holder is
//! whoever holds the claim inscription now.
//!
//! Tables (riding on ord.v1, same commit; C = chain number):
//!   maps.v1.map  C|namespace (var)|n u64  -> number u64 | iid

use crate::names::is_block_map;
use crate::ord::{iid_str, Born, OrdHook};
use arcade_chaindb::undo::Logged;
use arcade_chaindb::ChainDb;
use arcade_store::{KeyBuf, Result, TableDef};

pub const MAP: &str = "maps.v1.map";

/// (n, namespace) of a valid map claim.
pub fn map_claim(ctype: &str, body: &[u8], height: u32) -> Option<(u64, String)> {
    let t = ctype.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    if t != "text/plain" {
        return None;
    }
    let s = std::str::from_utf8(body).ok()?;
    let (n, ns) = s.split_once('.')?;
    if n.is_empty() || n.len() > 10 || !n.bytes().all(|b| b.is_ascii_digit()) || (n.len() > 1 && n.starts_with('0')) {
        return None;
    }
    if ns.is_empty() || ns.len() > 32 || !ns.bytes().all(|b| b.is_ascii_lowercase()) || !is_block_map(ns) {
        return None;
    }
    let n: u64 = n.parse().ok()?;
    if n > height as u64 {
        return None;
    }
    Some((n, ns.to_string()))
}

fn key(c: u16, ns: &str, n: u64) -> Vec<u8> {
    KeyBuf::new().u16(c).var(ns.as_bytes()).u64(n).done()
}

pub struct MapsHook;

impl OrdHook for MapsHook {
    fn name(&self) -> &'static str {
        "maps"
    }
    fn tables(&self) -> Vec<TableDef> {
        vec![TableDef::plain(MAP)]
    }
    fn born(&self, l: &Logged, c: u16, b: &Born) -> Result<()> {
        if let Some((n, ns)) = map_claim(b.ctype, b.body, b.height) {
            let k = key(c, &ns, n);
            if l.get(MAP, &k)?.is_none() {
                let mut v = b.number.to_be_bytes().to_vec();
                v.extend_from_slice(b.iid);
                l.put(MAP, &k, &v)?;
            }
        }
        Ok(())
    }
}

/// The winning claim of "<n>.<namespace>": (number, iid).
pub fn lookup(db: &ChainDb, c: u16, claim: &str) -> Option<(u64, String)> {
    let (n, ns) = claim.trim().split_once('.')?;
    let n: u64 = n.parse().ok()?;
    let v = db.store(c).read(|r| r.get(MAP, &key(c, &ns.to_ascii_lowercase(), n))).ok()??;
    (v.len() == 44).then(|| (u64::from_be_bytes(v[..8].try_into().unwrap()), iid_str(&v[8..])))
}

/// Claimed blocks per namespace.
pub fn counts(db: &ChainDb, c: u16) -> Vec<(String, u64)> {
    let rows = db.store(c).read(|r| r.scan_prefix(MAP, &KeyBuf::new().u16(c).done())).unwrap_or_default();
    let mut out: Vec<(String, u64)> = Vec::new();
    for (k, _) in rows {
        let len = k[2] as usize;
        let ns = String::from_utf8_lossy(&k[3..3 + len]).into_owned();
        match out.last_mut() {
            Some((x, n)) if *x == ns => *n += 1,
            _ => out.push((ns, 1)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn claims() {
        assert_eq!(map_claim("text/plain;charset=utf-8", b"100000.pepemap", 200_000), Some((100_000, "pepemap".into())));
        assert_eq!(map_claim("text/plain", b"100000.pepemap", 99_999), None);
        assert_eq!(map_claim("text/plain", b"0100.dogemap", 1000), None);
        assert_eq!(map_claim("text/plain", b"0.bitmap", 10), Some((0, "bitmap".into())));
        assert_eq!(map_claim("text/plain", b"5.pepe", 10), None);
        assert_eq!(map_claim("text/plain", b"5.dogemap\n", 10), None);
    }
}
