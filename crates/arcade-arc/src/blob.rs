//! Blobs (docs/ARCADECHAIN.md §15 step 1, rules version 3): content stored once by the chain, named by a small record
//! in a transaction while its bytes ride in the block's own blob section.
//!
//! - **The record** is a transaction output `OP_RETURN <push>` whose push is `"ablb"` ‖ root (32) ‖ length (8, LE) ‖
//!   class (1) ‖ encoding (1) ‖ type (the rest, UTF-8, at most [`MAX_TYPE`] bytes). About 110 bytes whatever the size.
//! - **The root** is the RFC 6962 Merkle root over the bytes cut into [`LEAF`]-byte leaves (the last zero-padded): the
//!   sealing leaf, so a blob seals as any segment does. **The id** is `H("blob" ‖ root ‖ length)`, so two contents that
//!   differ only in trailing zeros have different ids.
//! - **Stored once:** a block's blob section holds the bytes of exactly the blobs its transactions name for the first
//!   time on the chain, in the order the records come; a record of a blob the chain has is just a name for it.
//! - **Paid for:** a transaction that brings a new blob pays at least [`fee_for`] its bytes (0.01 ARCADE a kB, ten
//!   times the transaction floor), into the pot like every fee.

use crate::merkle::{self, Hash};
use crate::params::{Net, COIN};
use crate::tagged;

/// Bytes in a leaf (the sealing leaf, sloth::DATA_LEAF).
pub const LEAF: usize = crate::sloth::DATA_LEAF;
/// Most bytes of one blob, and of all the blobs of one block.
pub const MAX_BLOB_BYTES: usize = 4_000_000;
/// Longest type string in a record.
pub const MAX_TYPE: usize = 64;
/// Blob bytes' fee floor per 1,000 bytes: ten times the transaction floor (0.01 ARCADE).
pub const BLOB_FEE_PER_KB: u64 = COIN / 100;
/// The record's marker.
pub const MARKER: &[u8; 4] = b"ablb";

/// How long the network keeps a blob (§15.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Kept for good (collectibles, documents, anything owned).
    Permanent = 0,
    /// A release of a release line: the newest few are kept.
    Release = 1,
    /// Kept a set time (90 days).
    Temporary = 2,
    /// A boost (§15 step 4): paying again for a blob, at its full blob fee whether or not the chain holds it, so full
    /// nodes keep it plain 90 more days. A blob a boost brings first is kept as a Temporary one.
    Boost = 3,
}

impl Class {
    pub fn from_u8(b: u8) -> Option<Class> {
        match b {
            0 => Some(Class::Permanent),
            1 => Some(Class::Release),
            2 => Some(Class::Temporary),
            3 => Some(Class::Boost),
            _ => None,
        }
    }
}

/// How the bytes are stored (readers undo it; the root is over the stored bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Plain = 0,
    Gzip = 1,
}

impl Encoding {
    pub fn from_u8(b: u8) -> Option<Encoding> {
        match b {
            0 => Some(Encoding::Plain),
            1 => Some(Encoding::Gzip),
            _ => None,
        }
    }
}

/// A blob record, as a transaction carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub root: Hash,
    pub len: u64,
    pub class: Class,
    pub encoding: Encoding,
    pub kind: String,
}

impl Record {
    pub fn id(&self, net: &Net) -> Hash {
        id_of(net, &self.root, self.len)
    }

    /// The output script that carries it.
    pub fn script(&self) -> Vec<u8> {
        let mut data = MARKER.to_vec();
        data.extend_from_slice(&self.root);
        data.extend_from_slice(&self.len.to_le_bytes());
        data.push(self.class as u8);
        data.push(self.encoding as u8);
        data.extend_from_slice(self.kind.as_bytes());
        let mut s = vec![crate::script::OP_RETURN];
        crate::script::push(&mut s, &data);
        s
    }

    /// A record from an output script, if it is one (and a well-formed one).
    pub fn parse(script: &[u8]) -> Option<Record> {
        let (&op, rest) = script.split_first()?;
        if op != crate::script::OP_RETURN {
            return None;
        }
        let data = single_push(rest)?;
        if data.len() < 4 + 32 + 8 + 2 || &data[..4] != MARKER {
            return None;
        }
        let root: Hash = data[4..36].try_into().ok()?;
        let len = u64::from_le_bytes(data[36..44].try_into().ok()?);
        let class = Class::from_u8(data[44])?;
        let encoding = Encoding::from_u8(data[45])?;
        let kind = std::str::from_utf8(&data[46..]).ok()?;
        if kind.len() > MAX_TYPE || len == 0 || len > MAX_BLOB_BYTES as u64 {
            return None;
        }
        Some(Record { root, len, class, encoding, kind: kind.to_string() })
    }

    /// A record for `bytes`.
    pub fn of(bytes: &[u8], class: Class, encoding: Encoding, kind: &str) -> Record {
        Record { root: root_of(bytes), len: bytes.len() as u64, class, encoding, kind: kind.to_string() }
    }
}

/// The one push after OP_RETURN, written the shortest way, and nothing after it.
fn single_push(s: &[u8]) -> Option<&[u8]> {
    let (&op, rest) = s.split_first()?;
    let (n, rest) = match op {
        1..=75 => (op as usize, rest),
        0x4c => (*rest.first()? as usize, &rest[1..]),
        0x4d => (u16::from_le_bytes(rest.get(..2)?.try_into().ok()?) as usize, &rest[2..]),
        _ => return None,
    };
    (rest.len() == n).then_some(rest)
}

/// The Merkle root over `bytes` in LEAF-byte leaves (the last zero-padded).
pub fn root_of(bytes: &[u8]) -> Hash {
    let mut b = merkle::RootBuilder::new();
    for chunk in bytes.chunks(LEAF) {
        let mut leaf = [0u8; LEAF];
        leaf[..chunk.len()].copy_from_slice(chunk);
        b.push_leaf(&leaf);
    }
    b.finish().unwrap_or([0; 32])
}

pub fn id_of(net: &Net, root: &Hash, len: u64) -> Hash {
    tagged::hash(net, "blob", &[root, &len.to_le_bytes()])
}

/// The id of a blob's bytes.
pub fn id_of_bytes(net: &Net, bytes: &[u8]) -> Hash {
    id_of(net, &root_of(bytes), bytes.len() as u64)
}

/// The least fee a transaction pays for bringing `bytes` of new blobs.
pub fn fee_for(bytes: u64) -> u64 {
    bytes.div_ceil(1_000).saturating_mul(BLOB_FEE_PER_KB)
}

// ---------------- rules version 5: copies by class (docs/FOGSTORE.md) ----------------

/// What one copy of 1,000 bytes costs for a month (rules version 5): 47 ARCADE a GB a copy a month (Martin 2026-11-18:
/// at a cent an ARCADE, an arcade's VPS paid for twice over a month; the first figures, to settle with the real bills).
pub const COPY_MONTH_PER_KB: u64 = 47 * COIN / 1_000_000;

/// The least number of copies the network keeps of a blob of `class` (Martin 2026-11-18: "a min of three copies for
/// lower level blobs and more copies for higher cost blobs"): Temporary 3, Release 5, Permanent 7; a boost adds 2.
pub fn floor(class: Class) -> usize {
    match class {
        Class::Temporary => 3,
        Class::Release => 5,
        Class::Permanent => 7,
        Class::Boost => 2,
    }
}

/// Months a blob's fee prepays its copies (rules version 5): Temporary and a boost 3 (90 days); Release and Permanent 12.
pub fn months(class: Class) -> u64 {
    match class {
        Class::Temporary | Class::Boost => 3,
        Class::Release | Class::Permanent => 12,
    }
}

/// A blob's rate per started 1,000 bytes from rules version 5: its copies, for its months, at [`COPY_MONTH_PER_KB`].
pub fn class_fee_per_kb(class: Class) -> u64 {
    COPY_MONTH_PER_KB * floor(class) as u64 * months(class)
}

/// The least fee for the blobs a transaction pays for, each `(class, length)`, in the block at `height`: the flat rate
/// ([`fee_for`] over their bytes) before rules version 5, each by its class from then on.
pub fn fee_at(net: &crate::params::Net, height: u64, paid: &[(Class, u64)]) -> u64 {
    if crate::params::rules_v5(net, height) {
        paid.iter().map(|(c, len)| len.div_ceil(1_000).saturating_mul(class_fee_per_kb(*c))).fold(0u64, u64::saturating_add)
    } else {
        fee_for(paid.iter().map(|p| p.1).sum())
    }
}

/// The header's root over a block's blob ids, in order; zeros when it carries none.
pub fn ids_root(ids: &[Hash]) -> Hash {
    let mut b = merkle::RootBuilder::new();
    for id in ids {
        b.push_leaf(id);
    }
    b.finish().unwrap_or([0; 32])
}

/// The records of a transaction's outputs, in order.
pub fn records(tx: &arcade_chain::Tx) -> Vec<Record> {
    tx.outputs.iter().filter_map(|o| Record::parse(&o.script_pubkey)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::TESTNET;

    #[test]
    fn a_record_reads_back_and_names_its_bytes() {
        let bytes: Vec<u8> = (0..1_000u32).map(|i| (i * 7) as u8).collect();
        let r = Record::of(&bytes, Class::Release, Encoding::Gzip, "application/javascript");
        let s = r.script();
        assert!(s.len() < 160, "a record is small whatever the size: {}", s.len());
        assert_eq!(Record::parse(&s), Some(r.clone()));
        assert_eq!(r.id(&TESTNET), id_of_bytes(&TESTNET, &bytes));
        // Trailing zeros change the id (the length is in it), another network too.
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(root_of(&longer[..LEAF * 7]), root_of(&bytes[..LEAF * 7]));
        assert_ne!(id_of_bytes(&TESTNET, &longer), id_of_bytes(&TESTNET, &bytes));
        assert_ne!(r.id(&crate::params::MAINNET), r.id(&TESTNET));
        // Not records: another marker, an empty or too long blob, bytes after the push, an unknown class.
        let mut bad = s.clone();
        bad[3] = b'x';
        assert!(Record::parse(&bad).is_none());
        assert!(Record::parse(&Record { len: 0, ..r.clone() }.script()).is_none());
        assert!(Record::parse(&Record { len: MAX_BLOB_BYTES as u64 + 1, ..r.clone() }.script()).is_none());
        let mut trailing = s.clone();
        trailing.push(0);
        assert!(Record::parse(&trailing).is_none());
        assert!(Class::from_u8(4).is_none() && Encoding::from_u8(2).is_none());
        // The fee: 0.01 ARCADE a started kB.
        assert_eq!(fee_for(1), BLOB_FEE_PER_KB);
        assert_eq!(fee_for(1_000), BLOB_FEE_PER_KB);
        assert_eq!(fee_for(3_600_000), 3_600 * BLOB_FEE_PER_KB);
    }
}
