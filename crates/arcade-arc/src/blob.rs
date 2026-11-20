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

/// The copies a blob's fee pays for, by class (rules version 5, Martin 2026-11-18: "a min of three copies for lower level
/// blobs and more copies for higher cost blobs"): Temporary 3, Release 5, Permanent 7; a boost adds 2. This is the
/// **price** only: from rules version 6 the copies the network counts against a floor are [`floor_at`]'s (Permanent 3,
/// Martin 2026-11-19), while what a blob pays stays as version 5 set it.
pub fn priced_copies(class: Class) -> usize {
    match class {
        Class::Temporary => 3,
        Class::Release => 5,
        Class::Permanent => 7,
        Class::Boost => 2,
    }
}

/// The copy floors by rules version (docs/FOGSTORE.md): from each version on, the least number of copies the network
/// keeps of a blob of each class, as `[Permanent, Release, Temporary, Boost]` (indexed by `Class as usize`). A later
/// version raises a floor with a new row (Martin 2026-11-19: Permanent back toward 7 as the network grows); nothing scales
/// by itself. Below the first row's version the first row holds.
pub const FLOORS: &[(u32, [usize; 4])] = &[
    // Version 5: the floors are the copies the fee pays for.
    (5, [7, 5, 3, 2]),
    // Version 6 (Martin 2026-11-19): Permanent 3 while the network is small; its price stays version 5's (7 copies).
    (6, [3, 5, 3, 2]),
];

/// The least number of copies the network keeps of a blob of `class` as the block at `height` stands: [`FLOORS`] at its
/// rules version (Permanent 7 from version 5, 3 from version 6; Release 5; Temporary 3; a boost adds 2).
pub fn floor_at(net: &crate::params::Net, height: u64, class: Class) -> usize {
    let v = net.rules_at(height);
    let row = FLOORS.iter().rev().find(|(from, _)| *from <= v).unwrap_or(&FLOORS[0]);
    row.1[class as usize]
}

/// Months a blob's fee prepays its copies (rules version 5): Temporary and a boost 3 (90 days); Release and Permanent 12.
pub fn months(class: Class) -> u64 {
    match class {
        Class::Temporary | Class::Boost => 3,
        Class::Release | Class::Permanent => 12,
    }
}

/// A blob's rate per started 1,000 bytes from rules version 5: the copies it pays for ([`priced_copies`]), for its
/// months, at [`COPY_MONTH_PER_KB`].
pub fn class_fee_per_kb(class: Class) -> u64 {
    COPY_MONTH_PER_KB * priced_copies(class) as u64 * months(class)
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

// ---------------- rules version 6: a Temporary blob upgraded to Permanent (docs/FOGSTORE.md) ----------------

/// The upgrade output's marker.
pub const UPGRADE_MARKER: &[u8; 4] = b"aupg";
/// Most blobs one upgrade output names: a large blob's manifest and all its parts (at most 26) with room to spare.
pub const MAX_UPGRADE_IDS: usize = 64;

/// The output script of an upgrade (rules version 6, Martin 2026-11-19): `OP_RETURN <push>` whose push is `"aupg"` ‖
/// the blob ids it upgrades to Permanent, 32 bytes each (1 to [`MAX_UPGRADE_IDS`] of them, each once). The
/// transaction pays [`upgrade_fee_at`] for each on top of its own fee floor; anyone may send it, and the blobs stay their
/// publishers'.
pub fn upgrade_script(ids: &[Hash]) -> Vec<u8> {
    let mut data = UPGRADE_MARKER.to_vec();
    for id in ids {
        data.extend_from_slice(id);
    }
    let mut s = vec![crate::script::OP_RETURN];
    crate::script::push(&mut s, &data);
    s
}

/// The blob ids an output script upgrades, if it is a well-formed upgrade: the marker, then 1 to [`MAX_UPGRADE_IDS`] ids
/// and nothing else. Anything else (another marker, a length that isn't the marker plus whole ids, none or too many
/// ids) is no upgrade, only an OP_RETURN's data, as before rules version 6.
pub fn parse_upgrade(script: &[u8]) -> Option<Vec<Hash>> {
    let (&op, rest) = script.split_first()?;
    if op != crate::script::OP_RETURN {
        return None;
    }
    let data = single_push(rest)?;
    let ids = data.strip_prefix(UPGRADE_MARKER.as_slice())?;
    if ids.is_empty() || ids.len() % 32 != 0 || ids.len() / 32 > MAX_UPGRADE_IDS {
        return None;
    }
    Some(ids.chunks(32).map(|c| c.try_into().expect("32 bytes")).collect())
}

/// Every blob id a transaction's upgrade outputs name, in order (a repeat included: [`check_upgrades`] refuses it).
pub fn upgrades(tx: &arcade_chain::Tx) -> Vec<Hash> {
    tx.outputs.iter().filter_map(|o| parse_upgrade(&o.script_pubkey)).flatten().collect()
}

/// What upgrading a Temporary blob of `len` bytes to Permanent costs in the block at `height`: the Permanent fee minus
/// the Temporary fee for the same bytes, both by [`fee_at`] at that height (rules version 5 rates: 394,800 − 42,300 =
/// 352,500 base units per started kB).
pub fn upgrade_fee_at(net: &Net, height: u64, len: u64) -> u64 {
    fee_at(net, height, &[(Class::Permanent, len)]).saturating_sub(fee_at(net, height, &[(Class::Temporary, len)]))
}

/// Blocks a Temporary blob (and one a boost brought, or a boost kept) is kept from its block: the network's Temporary
/// time ([`Net::blob_temporary`], 90 days) in target block times.
pub fn temporary_blocks(net: &Net) -> u64 {
    (net.blob_temporary / net.block_seconds.max(1)).max(1)
}

/// Whether a blob brought by a record of `class` is kept for a time only (Temporary, or brought by a boost).
pub fn timed(class: Class) -> bool {
    matches!(class, Class::Temporary | Class::Boost)
}

/// An upgrade a block made (rules version 6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Upgraded {
    /// The block that holds it.
    pub height: u64,
    pub txid: arcade_chain::hash::Hash256,
    /// What its transaction paid for this blob: [`upgrade_fee_at`] at `height`.
    pub paid: u64,
    /// Who paid: the script of the coin its transaction spends first (anyone may upgrade any Temporary blob).
    pub by: Vec<u8>,
}

/// What the chain knows of a blob it holds, as consensus uses it (rules version 6's upgrades).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct State {
    /// The class of the record that brought it.
    pub class: Class,
    pub len: u64,
    /// The block that brought it.
    pub height: u64,
    /// A timed blob ([`timed`]): the first height at which it is past its time ([`temporary_blocks`] after the block
    /// that brought it, or after its latest boost, whichever is later). `u64::MAX` for any other, and once upgraded.
    pub until: u64,
    pub upgraded: Option<Upgraded>,
}

impl State {
    /// A blob a record of `class` brings in the block at `height`.
    pub fn brought(net: &Net, class: Class, len: u64, height: u64) -> State {
        let until = if timed(class) { height.saturating_add(temporary_blocks(net)) } else { u64::MAX };
        State { class, len, height, until, upgraded: None }
    }

    /// A boost in the block at `height`: a timed blob is kept [`temporary_blocks`] past it.
    pub fn boost(&mut self, net: &Net, height: u64) {
        if self.until != u64::MAX {
            self.until = self.until.max(height.saturating_add(temporary_blocks(net)));
        }
    }

    /// Its class now: Permanent once upgraded, else its record's.
    pub fn class_now(&self) -> Class {
        if self.upgraded.is_some() {
            Class::Permanent
        } else {
            self.class
        }
    }
}

/// Why an upgrade is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum UpgradeError {
    #[error("blob upgrades start at ARCADE rules version 6")]
    NotYet,
    #[error("the chain holds no such blob (a blob brought in the same block can't be upgraded in it)")]
    Unknown,
    #[error("the blob is Permanent or Release already: only a Temporary blob is upgraded")]
    NotTemporary,
    #[error("the Temporary blob is past its time: its bytes may be gone, publish it again as Permanent")]
    Expired,
    #[error("the blob is upgraded twice in one transaction or block")]
    Twice,
    #[error("the fee doesn't cover the upgrade (Permanent minus Temporary for its bytes)")]
    Underpaid,
}

/// Whether one blob, as the chain held it before the block at `height` (`state`, None when it holds no such blob), may be
/// upgraded in that block: what the upgrade costs. A Temporary blob, or one a boost brought, still within its time.
pub fn check_upgrade(net: &Net, height: u64, state: Option<&State>) -> Result<u64, UpgradeError> {
    if !crate::params::rules_v6(net, height) {
        return Err(UpgradeError::NotYet);
    }
    let s = state.ok_or(UpgradeError::Unknown)?;
    if s.upgraded.is_some() || !timed(s.class) {
        return Err(UpgradeError::NotTemporary);
    }
    if height >= s.until {
        return Err(UpgradeError::Expired);
    }
    Ok(upgrade_fee_at(net, height, s.len))
}

/// The upgrades of one transaction in the block at `height` against the blobs as the chain held them before the block
/// (`state`), with `taken` the blobs upgraded by the block's transactions before it (each added here): what they cost
/// together. None from the transaction's ids repeats one, or names one `taken` has.
pub fn check_upgrades(net: &Net, height: u64, ids: &[Hash], state: &dyn Fn(&Hash) -> Option<State>, taken: &mut std::collections::HashSet<Hash>) -> Result<u64, UpgradeError> {
    let mut fee = 0u64;
    for id in ids {
        let f = check_upgrade(net, height, state(id).as_ref())?;
        if !taken.insert(*id) {
            return Err(UpgradeError::Twice);
        }
        fee = fee.saturating_add(f);
    }
    Ok(fee)
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

    /// A test network: version 5 from 0, version 6 from 100, Temporary blobs kept 40 blocks.
    const V6: Net = Net { rules: &[(3, 0), (5, 1), (6, 100)], blob_temporary: 40 * crate::params::BLOCK_SECONDS, ..TESTNET };

    #[test]
    fn rules_v6_floors_come_from_the_rules_table_and_prices_stay() {
        // Version 5: the floors are the copies paid for; version 6: Permanent 3, the rest as before.
        assert_eq!([Class::Permanent, Class::Release, Class::Temporary, Class::Boost].map(|c| floor_at(&V6, 99, c)), [7, 5, 3, 2]);
        assert_eq!([Class::Permanent, Class::Release, Class::Temporary, Class::Boost].map(|c| floor_at(&V6, 100, c)), [3, 5, 3, 2]);
        // Below version 5 (no floors yet) the first row stands.
        assert_eq!(floor_at(&Net { rules: &[(3, 0)], ..TESTNET }, 5, Class::Permanent), 7);
        // Prices don't move: Permanent still pays for 7 copies a year.
        assert_eq!(class_fee_per_kb(Class::Permanent), 4_700 * 7 * 12);
        assert_eq!(class_fee_per_kb(Class::Temporary), 4_700 * 3 * 3);
        assert_eq!(fee_at(&V6, 99, &[(Class::Permanent, 1_000)]), fee_at(&V6, 100, &[(Class::Permanent, 1_000)]));
        // The table's versions rise.
        assert!(FLOORS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn rules_v6_an_upgrade_reads_back_and_costs_the_difference() {
        let ids: Vec<Hash> = (1..=3u8).map(|i| [i; 32]).collect();
        let s = upgrade_script(&ids);
        assert_eq!(parse_upgrade(&s), Some(ids.clone()));
        assert_eq!(s.len(), 1 + 2 + 4 + 96, "OP_RETURN, OP_PUSHDATA1 and its length, the marker, three ids");
        // 64 ids fit (a large blob's manifest and its 26 parts with room); 65, none, a broken id or a blob record don't.
        let many: Vec<Hash> = (0..MAX_UPGRADE_IDS as u8).map(|i| [i; 32]).collect();
        assert_eq!(parse_upgrade(&upgrade_script(&many)).map(|v| v.len()), Some(MAX_UPGRADE_IDS));
        let too_many: Vec<Hash> = (0..=MAX_UPGRADE_IDS as u8).map(|i| [i; 32]).collect();
        assert_eq!(parse_upgrade(&upgrade_script(&too_many)), None);
        assert_eq!(parse_upgrade(&upgrade_script(&[])), None);
        let mut cut = UPGRADE_MARKER.to_vec();
        cut.extend_from_slice(&[7; 31]);
        let mut bad = vec![crate::script::OP_RETURN];
        crate::script::push(&mut bad, &cut);
        assert_eq!(parse_upgrade(&bad), None);
        assert_eq!(parse_upgrade(&Record::of(b"hi", Class::Temporary, Encoding::Plain, "text/plain").script()), None);
        assert!(Record::parse(&s).is_none(), "an upgrade is no blob record");

        // The difference per started kB: 394,800 - 42,300 = 352,500.
        assert_eq!(upgrade_fee_at(&V6, 100, 1), 352_500);
        assert_eq!(upgrade_fee_at(&V6, 100, 1_000), 352_500);
        assert_eq!(upgrade_fee_at(&V6, 100, 1_001), 2 * 352_500);
        assert_eq!(upgrade_fee_at(&V6, 100, 5_000), fee_at(&V6, 100, &[(Class::Permanent, 5_000)]) - fee_at(&V6, 100, &[(Class::Temporary, 5_000)]));
        // A large blob: each part's difference by its own bytes, as each part's fee was paid. 100 MB: 25 parts of
        // 3,999,992 bytes (4,000 started kB each), plus the rest, plus the manifest.
        let part = (crate::blob::MAX_BLOB_BYTES / LEAF * LEAF) as u64;
        let last = 100_000_000 - 24 * part;
        let large: u64 = (0..24).map(|_| upgrade_fee_at(&V6, 100, part)).sum::<u64>() + upgrade_fee_at(&V6, 100, last) + upgrade_fee_at(&V6, 100, 3_000);
        assert_eq!(large, (24 * 4_000 + last.div_ceil(1_000) + 3) * 352_500);
    }

    #[test]
    fn rules_v6_which_blobs_may_be_upgraded_and_when() {
        let t = State::brought(&V6, Class::Temporary, 2_000, 90);
        assert_eq!(t.until, 130);
        // Before version 6: never.
        assert_eq!(check_upgrade(&V6, 99, Some(&t)), Err(UpgradeError::NotYet));
        // From it: a Temporary blob in its time, for the difference.
        assert_eq!(check_upgrade(&V6, 100, Some(&t)), Ok(2 * 352_500));
        assert_eq!(check_upgrade(&V6, 129, Some(&t)), Ok(2 * 352_500));
        // Past its time: refused (its bytes may be gone).
        assert_eq!(check_upgrade(&V6, 130, Some(&t)), Err(UpgradeError::Expired));
        // A boost keeps it its time past the boost's block.
        let mut boosted = t.clone();
        boosted.boost(&V6, 120);
        assert_eq!(boosted.until, 160);
        assert_eq!(check_upgrade(&V6, 130, Some(&boosted)), Ok(2 * 352_500));
        // One a boost brought is timed too; Permanent and Release ones aren't upgraded, nor one upgraded already.
        assert!(check_upgrade(&V6, 100, Some(&State::brought(&V6, Class::Boost, 10, 95))).is_ok());
        assert_eq!(check_upgrade(&V6, 100, Some(&State::brought(&V6, Class::Permanent, 10, 95))), Err(UpgradeError::NotTemporary));
        assert_eq!(check_upgrade(&V6, 100, Some(&State::brought(&V6, Class::Release, 10, 95))), Err(UpgradeError::NotTemporary));
        let mut up = t.clone();
        up.upgraded = Some(Upgraded { height: 100, txid: arcade_chain::hash::Hash256([1; 32]), paid: 1, by: Vec::new() });
        assert_eq!(up.class_now(), Class::Permanent);
        assert_eq!(check_upgrade(&V6, 101, Some(&up)), Err(UpgradeError::NotTemporary));
        // Unknown.
        assert_eq!(check_upgrade(&V6, 100, None), Err(UpgradeError::Unknown));
        // Several in one transaction: summed; a repeat, in it or in the block before it, refused.
        let state = |id: &Hash| (id[0] < 3).then(|| t.clone());
        let mut taken = std::collections::HashSet::new();
        assert_eq!(check_upgrades(&V6, 100, &[[1; 32], [2; 32]], &state, &mut taken), Ok(4 * 352_500));
        assert_eq!(check_upgrades(&V6, 100, &[[0; 32], [0; 32]], &state, &mut std::collections::HashSet::new()), Err(UpgradeError::Twice));
        assert_eq!(check_upgrades(&V6, 100, &[[1; 32]], &state, &mut taken), Err(UpgradeError::Twice));
        assert_eq!(check_upgrades(&V6, 100, &[[5; 32]], &state, &mut taken), Err(UpgradeError::Unknown));
    }
}
