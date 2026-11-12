//! An ARCADE block, byte for byte (docs/ARCADECHAIN.md §1, §5.1, §9).
//!
//! ```text
//! block   = header ‖ varint(n) ‖ n transactions (Dogecoin's legacy format)
//!                  ‖ varint(m) ‖ m storage items
//! item    = tag (1 byte) ‖ varint(length) ‖ payload
//! ```
//!
//! All integers are little-endian. The **block id** is the double SHA-256 of the header
//! without its signature. The header holds the roots of both parts, so the id covers the
//! whole block; the next block's proof of time starts from it (§5.1). The signature is left
//! out of the id so that signing the same header twice can't make two ids.
//!
//! This module checks what a block can be checked for on its own: its bytes, its roots, its
//! writer's signature, the order and count of its storage items. Whether an item is *true*
//! (the right leaves, a real pick, a key with draw weight) needs the chain's state and is
//! checked where the state is kept.

use crate::merkle::{self, Hash};
use crate::params::{self, Net};
use crate::sloth::{DATA_LEAF, SEALED_LEAF};
use crate::tagged;
use arcade_chain::encode::write_varint;
use arcade_chain::hash::sha256d;
use arcade_chain::{DecodeError, Format, Hash256, Reader, Tx};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

/// An arcade's owner key: its mesh identity (ed25519 public key).
pub type Key = [u8; 32];
pub type Sig = [u8; 64];
/// An ECVRF-EDWARDS25519-SHA512-TAI proof (RFC 9381): gamma ‖ c ‖ s.
pub type VrfProof = [u8; 80];
/// Where pay goes: an address version byte, then the 20-byte hash (P2PKH or P2SH).
pub type Payout = [u8; 21];

/// ARCADE's coin transactions are Dogecoin's: legacy, no SegWit, no AuxPoW.
pub const TX_FORMAT: Format = Format { auxpow: false, ..Format::AUXPOW };

pub const HEADER_VERSION: u32 = 1;
/// Longest proof-of-time output or proof, in bytes.
pub const MAX_TIME_BYTES: usize = 512;
/// Longest Merkle path (a tree of 2^40 leaves).
pub const MAX_PATH: usize = 40;
/// Longest source chain key ("doge", "peptest", ...).
pub const MAX_CHAIN_KEY: usize = 16;
/// No block is longer than this: both parts full, plus the header and the counts.
pub const MAX_BLOCK_BYTES: usize = params::MAX_TX_BYTES + params::MAX_STORAGE_BYTES + 4_096;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BlockError {
    #[error("decode: {0}")]
    Decode(#[from] DecodeError),
    #[error("unknown storage item tag {0:#04x}")]
    UnknownTag(u8),
    #[error("bytes left over after {0}")]
    Trailing(&'static str),
    #[error("block has no transactions")]
    Empty,
    #[error("first transaction is not a coinbase")]
    NoCoinbase,
    #[error("a later transaction is a coinbase")]
    ExtraCoinbase,
    #[error("the same transaction twice")]
    DuplicateTx,
    #[error("transaction root doesn't match the header")]
    TxRoot,
    #[error("storage root doesn't match the header")]
    StorageRoot,
    #[error("storage items are out of order")]
    ItemOrder,
    #[error("too many {0}")]
    ItemLimit(&'static str),
    #[error("too many bytes of {0}")]
    TooBig(&'static str),
    #[error("bad signature on {0}")]
    BadSignature(&'static str),
    #[error("{0}")]
    Invalid(&'static str),
}

pub fn sign(key: &SigningKey, msg: &[u8]) -> Sig {
    key.sign(msg).to_bytes()
}

/// Strict ed25519: one valid encoding per signature, no weak keys.
pub fn verify(key: &Key, msg: &[u8], sig: &Sig) -> bool {
    let Ok(vk) = VerifyingKey::from_bytes(key) else { return false };
    vk.verify_strict(msg, &Signature::from_bytes(sig)).is_ok()
}

/// A replica is one owner key's sealed copy of one segment.
pub fn replica_id(net: &Net, segment: &Hash, owner: &Key) -> Hash {
    tagged::hash(net, "replica", &[segment, owner])
}

fn put_bytes(out: &mut Vec<u8>, b: &[u8]) {
    write_varint(out, b.len() as u64);
    out.extend_from_slice(b);
}

fn get_bytes(r: &mut Reader, max: usize, what: &'static str) -> Result<Vec<u8>, DecodeError> {
    let b = r.var_bytes()?;
    if b.len() > max {
        return Err(DecodeError::Invalid(what));
    }
    Ok(b.to_vec())
}

fn put_path(out: &mut Vec<u8>, path: &[Hash]) {
    write_varint(out, path.len() as u64);
    for h in path {
        out.extend_from_slice(h);
    }
}

fn get_path(r: &mut Reader) -> Result<Vec<Hash>, DecodeError> {
    let n = r.len()?;
    if n > MAX_PATH {
        return Err(DecodeError::Invalid("Merkle path too long"));
    }
    (0..n).map(|_| r.array()).collect()
}

// ---------------------------------------------------------------------------------------
// Header

/// A proof of time: the calculation's output and the short proof of it.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct TimeProof {
    pub output: Vec<u8>,
    pub proof: Vec<u8>,
}

impl TimeProof {
    fn encode(&self, out: &mut Vec<u8>) {
        put_bytes(out, &self.output);
        put_bytes(out, &self.proof);
    }
    fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(TimeProof {
            output: get_bytes(r, MAX_TIME_BYTES, "proof-of-time output too long")?,
            proof: get_bytes(r, MAX_TIME_BYTES, "proof of time too long")?,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub version: u32,
    pub height: u64,
    /// The id of the block before; this block's proof of time starts from it.
    pub prev: Hash256,
    /// Seconds since 1970, UTC.
    pub time: u64,
    /// The Merkle root (`merkle.rs`) over the transaction ids.
    pub tx_root: Hash,
    /// The Merkle root over the storage items; zeros when there are none.
    pub storage_root: Hash,
    /// `D` (§6.2).
    pub difficulty: u128,
    pub writer: Key,
    /// The writer's `W` in bytes, as it claims; checked against the chain's state.
    pub weight: u64,
    /// The writer's draw for this height.
    pub vrf_proof: VrfProof,
    /// The proof of time: `score × D` steps from `prev`.
    pub pot: TimeProof,
    /// In the first block of an epoch: the proof of time behind the epoch's draw seed (§6.1).
    pub draw_seed: Option<TimeProof>,
    pub sig: Sig,
}

impl Header {
    /// Everything but the signature.
    pub fn unsigned_bytes(&self) -> Vec<u8> {
        let mut o = Vec::with_capacity(512);
        o.extend_from_slice(&self.version.to_le_bytes());
        o.extend_from_slice(&self.height.to_le_bytes());
        o.extend_from_slice(&self.prev.0);
        o.extend_from_slice(&self.time.to_le_bytes());
        o.extend_from_slice(&self.tx_root);
        o.extend_from_slice(&self.storage_root);
        o.extend_from_slice(&self.difficulty.to_le_bytes());
        o.extend_from_slice(&self.writer);
        o.extend_from_slice(&self.weight.to_le_bytes());
        o.extend_from_slice(&self.vrf_proof);
        self.pot.encode(&mut o);
        match &self.draw_seed {
            None => o.push(0),
            Some(t) => {
                o.push(1);
                t.encode(&mut o);
            }
        }
        o
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.unsigned_bytes());
        out.extend_from_slice(&self.sig);
    }

    pub fn decode(r: &mut Reader) -> Result<Header, DecodeError> {
        let version = r.u32()?;
        if version != HEADER_VERSION {
            return Err(DecodeError::Invalid("unknown header version"));
        }
        let height = r.u64()?;
        let prev = r.hash()?;
        let time = r.u64()?;
        let tx_root = r.array()?;
        let storage_root = r.array()?;
        let difficulty = u128::from_le_bytes(r.array()?);
        let writer = r.array()?;
        let weight = r.u64()?;
        let vrf_proof = r.array()?;
        let pot = TimeProof::decode(r)?;
        let draw_seed = match r.u8()? {
            0 => None,
            1 => Some(TimeProof::decode(r)?),
            _ => return Err(DecodeError::Invalid("bad draw-seed flag")),
        };
        let sig = r.array()?;
        Ok(Header {
            version,
            height,
            prev,
            time,
            tx_root,
            storage_root,
            difficulty,
            writer,
            weight,
            vrf_proof,
            pot,
            draw_seed,
            sig,
        })
    }

    /// The block id.
    pub fn id(&self) -> Hash256 {
        sha256d(&self.unsigned_bytes())
    }

    fn message(&self, net: &Net) -> Vec<u8> {
        tagged::message(net, "block", &[&self.id().0])
    }

    /// Sets the writer to `key` and signs.
    pub fn sign(&mut self, net: &Net, key: &SigningKey) {
        self.writer = key.verifying_key().to_bytes();
        self.sig = sign(key, &self.message(net));
    }

    pub fn signature_ok(&self, net: &Net) -> bool {
        verify(&self.writer, &self.message(net), &self.sig)
    }

    /// What a header (of block 1 or later) can be checked for without the chain.
    pub fn check(&self, net: &Net) -> Result<(), BlockError> {
        if self.height == 0 {
            return Err(BlockError::Invalid("block 0 is fixed by the rules, not written"));
        }
        if self.difficulty == 0 {
            return Err(BlockError::Invalid("difficulty is at least 1"));
        }
        if self.weight == 0 {
            return Err(BlockError::Invalid("a writer has draw weight"));
        }
        if self.draw_seed.is_some() != (self.height % params::EPOCH_BLOCKS == 0) {
            return Err(BlockError::Invalid("the first block of an epoch, and only that one, proves its draw seed"));
        }
        if !self.signature_ok(net) {
            return Err(BlockError::BadSignature("block"));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Storage items

/// One leaf of a replica shown both ways: sealed, with its path to the replica root, and
/// plain, with its path to the segment's root. Which leaf it is follows from the challenge,
/// so the item doesn't say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Opened {
    pub sealed: [u8; SEALED_LEAF],
    pub sealed_path: Vec<Hash>,
    pub data: [u8; DATA_LEAF],
    pub data_path: Vec<Hash>,
}

impl Opened {
    fn encode(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.sealed);
        put_path(out, &self.sealed_path);
        out.extend_from_slice(&self.data);
        put_path(out, &self.data_path);
    }
    fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        Ok(Opened { sealed: r.array()?, sealed_path: get_path(r)?, data: r.array()?, data_path: get_path(r)? })
    }
}

/// An audit's answer (§5.2 step 3): the 4 leaves the seed chose, each with its path to `A`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditOpening {
    pub replica: Hash,
    pub leaves: Vec<(Opened, Vec<Hash>)>,
}

/// An audit's commit `A` (§5.2 step 2), signed so nobody can post a wrong one for a rival.
/// The signature covers the picking block's id, so it means nothing on another branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditCommit {
    pub replica: Hash,
    /// Height of the picking block.
    pub pick: u64,
    pub root: Hash,
    pub sig: Sig,
}

impl AuditCommit {
    pub fn message(&self, net: &Net, pick_id: &Hash256) -> Vec<u8> {
        tagged::message(net, "audit", &[&self.replica, &pick_id.0, &self.root])
    }
}

/// A replica that missed an audit asks for a new one (§5.3). The signature covers the id of
/// the missed audit's picking block, so an old request can't be posted again later.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reaudit {
    pub replica: Hash,
    /// Height of the missed audit's picking block.
    pub missed: u64,
    pub sig: Sig,
}

impl Reaudit {
    pub fn message(&self, net: &Net, missed_id: &Hash256) -> Vec<u8> {
        tagged::message(net, "reaudit", &[&self.replica, &missed_id.0])
    }
}

/// A registration's proof (§4 step 2): the 16 leaves the seed chose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegProof {
    pub replica: Hash,
    pub leaves: Vec<Opened>,
}

/// A serving check's verdict (§5.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub replica: Hash,
    /// The block whose seed made `checker` a checker of this replica.
    pub height: u64,
    pub checker: Key,
    pub vrf_proof: VrfProof,
    pub served: bool,
    pub sig: Sig,
}

impl Verdict {
    pub fn message(&self, net: &Net, block_id: &Hash256) -> Vec<u8> {
        tagged::message(net, "verdict", &[&self.replica, &block_id.0, &self.checker, &self.vrf_proof, &[self.served as u8]])
    }
}

/// A registration's commit (§4 step 1). Signed by the owner key, and stamped with work.
/// Both cover the id of a recent block (`anchor`), so stamps can't be stocked up for a
/// flood and an old commit can't be posted again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegCommit {
    pub segment: Hash,
    pub owner: Key,
    pub payout: Payout,
    pub replica_root: Hash,
    /// Height of a recent block.
    pub anchor: u64,
    /// Chosen to give the stamp its work.
    pub nonce: u64,
    pub sig: Sig,
}

impl RegCommit {
    pub fn message(&self, net: &Net, anchor_id: &Hash256) -> Vec<u8> {
        tagged::message(net, "register", &[&self.segment, &self.owner, &self.payout, &self.replica_root, &anchor_id.0])
    }

    /// The work stamp: the more zero bits it starts with, the more work it took.
    pub fn stamp(&self, net: &Net, anchor_id: &Hash256) -> Hash {
        tagged::hash(
            net,
            "stamp",
            &[&self.segment, &self.owner, &self.payout, &self.replica_root, &anchor_id.0, &self.nonce.to_le_bytes()],
        )
    }

    pub fn replica(&self, net: &Net) -> Hash {
        replica_id(net, &self.segment, &self.owner)
    }
}

/// Zero bits at the start of a hash.
pub fn work_bits(h: &Hash) -> u32 {
    let mut bits = 0;
    for b in h {
        bits += b.leading_zeros();
        if *b != 0 {
            break;
        }
    }
    bits
}

/// A segment offered to the catalog (§2.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CatalogEntry {
    /// The source chain's key, as V1.0 names it ("doge", "btc", ...).
    pub chain: String,
    /// 0 = blocks.
    pub family: u8,
    pub first: u64,
    pub last: u64,
    /// Raw bytes in the segment.
    pub bytes: u64,
    /// V1SHARE's manifest id of the same data, for fetching it.
    pub manifest: Hash,
    /// The Merkle root over the raw bytes in 127-byte leaves.
    pub root: Hash,
    pub poster: Key,
    pub sig: Sig,
}

impl CatalogEntry {
    /// The segment id: the mask of every sealed leaf is made from it (§3.3).
    pub fn segment(&self, net: &Net) -> Hash {
        tagged::hash(
            net,
            "segment",
            &[
                &[self.chain.len() as u8],
                self.chain.as_bytes(),
                &[self.family],
                &self.first.to_le_bytes(),
                &self.last.to_le_bytes(),
                &self.bytes.to_le_bytes(),
                &self.root,
            ],
        )
    }

    /// 127-byte leaves in the segment; the last is filled up with zeros.
    pub fn leaves(&self) -> u64 {
        self.bytes.div_ceil(DATA_LEAF as u64)
    }

    pub fn message(&self, net: &Net) -> Vec<u8> {
        tagged::message(net, "entry", &[&self.segment(net), &self.manifest, &self.poster])
    }
}

/// "My own root for this range is the same" (§2.4).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attest {
    pub segment: Hash,
    pub attester: Key,
    pub sig: Sig,
}

impl Attest {
    pub fn message(&self, net: &Net) -> Vec<u8> {
        tagged::message(net, "attest", &[&self.segment, &self.attester])
    }
}

/// Two signed headers from one writer on the same previous block (§5.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Equivocation {
    pub a: Header,
    pub b: Header,
}

impl Equivocation {
    /// Is this proof of cheating by `a.writer`?
    pub fn proves(&self, net: &Net) -> bool {
        self.a.writer == self.b.writer
            && self.a.prev == self.b.prev
            && self.a.id() != self.b.id()
            && self.a.signature_ok(net)
            && self.b.signature_ok(net)
    }
}

/// Everything the storage section can hold. The tag's high half is the item's place in the
/// section's order (§9); answers with a deadline come before new requests.
///
/// Tags 0x11, 0x12, 0x62 and 0x63 are kept for the answers to reveal demands and fraud
/// proofs, and for those demands and proofs themselves (build step 2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Item {
    AuditOpening(AuditOpening),
    AuditCommit(AuditCommit),
    Reaudit(Reaudit),
    RegProof(RegProof),
    Verdict(Verdict),
    RegCommit(RegCommit),
    CatalogEntry(CatalogEntry),
    Attest(Attest),
    Equivocation(Box<Equivocation>),
}

pub const TAG_AUDIT_OPENING: u8 = 0x10;
pub const TAG_AUDIT_COMMIT: u8 = 0x20;
pub const TAG_REAUDIT: u8 = 0x21;
pub const TAG_REG_PROOF: u8 = 0x30;
pub const TAG_VERDICT: u8 = 0x40;
pub const TAG_REG_COMMIT: u8 = 0x50;
pub const TAG_CATALOG_ENTRY: u8 = 0x60;
pub const TAG_ATTEST: u8 = 0x61;
pub const TAG_EQUIVOCATION: u8 = 0x64;

impl Item {
    pub fn tag(&self) -> u8 {
        match self {
            Item::AuditOpening(_) => TAG_AUDIT_OPENING,
            Item::AuditCommit(_) => TAG_AUDIT_COMMIT,
            Item::Reaudit(_) => TAG_REAUDIT,
            Item::RegProof(_) => TAG_REG_PROOF,
            Item::Verdict(_) => TAG_VERDICT,
            Item::RegCommit(_) => TAG_REG_COMMIT,
            Item::CatalogEntry(_) => TAG_CATALOG_ENTRY,
            Item::Attest(_) => TAG_ATTEST,
            Item::Equivocation(_) => TAG_EQUIVOCATION,
        }
    }

    /// The item's place in the section's order: 1 (first) to 6.
    pub fn group(&self) -> u8 {
        self.tag() >> 4
    }

    pub fn payload(&self) -> Vec<u8> {
        let mut o = Vec::new();
        match self {
            Item::AuditOpening(x) => {
                o.extend_from_slice(&x.replica);
                for (opened, audit_path) in &x.leaves {
                    opened.encode(&mut o);
                    put_path(&mut o, audit_path);
                }
            }
            Item::AuditCommit(x) => {
                o.extend_from_slice(&x.replica);
                o.extend_from_slice(&x.pick.to_le_bytes());
                o.extend_from_slice(&x.root);
                o.extend_from_slice(&x.sig);
            }
            Item::Reaudit(x) => {
                o.extend_from_slice(&x.replica);
                o.extend_from_slice(&x.missed.to_le_bytes());
                o.extend_from_slice(&x.sig);
            }
            Item::RegProof(x) => {
                o.extend_from_slice(&x.replica);
                for opened in &x.leaves {
                    opened.encode(&mut o);
                }
            }
            Item::Verdict(x) => {
                o.extend_from_slice(&x.replica);
                o.extend_from_slice(&x.height.to_le_bytes());
                o.extend_from_slice(&x.checker);
                o.extend_from_slice(&x.vrf_proof);
                o.push(x.served as u8);
                o.extend_from_slice(&x.sig);
            }
            Item::RegCommit(x) => {
                o.extend_from_slice(&x.segment);
                o.extend_from_slice(&x.owner);
                o.extend_from_slice(&x.payout);
                o.extend_from_slice(&x.replica_root);
                o.extend_from_slice(&x.anchor.to_le_bytes());
                o.extend_from_slice(&x.nonce.to_le_bytes());
                o.extend_from_slice(&x.sig);
            }
            Item::CatalogEntry(x) => {
                put_bytes(&mut o, x.chain.as_bytes());
                o.push(x.family);
                o.extend_from_slice(&x.first.to_le_bytes());
                o.extend_from_slice(&x.last.to_le_bytes());
                o.extend_from_slice(&x.bytes.to_le_bytes());
                o.extend_from_slice(&x.manifest);
                o.extend_from_slice(&x.root);
                o.extend_from_slice(&x.poster);
                o.extend_from_slice(&x.sig);
            }
            Item::Attest(x) => {
                o.extend_from_slice(&x.segment);
                o.extend_from_slice(&x.attester);
                o.extend_from_slice(&x.sig);
            }
            Item::Equivocation(x) => {
                x.a.encode(&mut o);
                x.b.encode(&mut o);
            }
        }
        o
    }

    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.tag());
        put_bytes(out, &self.payload());
    }

    /// The bytes this item takes in a block.
    pub fn encoded_len(&self) -> usize {
        let mut o = Vec::new();
        self.encode(&mut o);
        o.len()
    }

    /// The item's leaf in the header's storage root.
    pub fn hash(&self) -> Hash {
        let mut leaf = vec![self.tag()];
        leaf.extend_from_slice(&self.payload());
        merkle::leaf_hash(&leaf)
    }

    pub fn decode(r: &mut Reader) -> Result<Item, BlockError> {
        let tag = r.u8()?;
        let payload = r.var_bytes()?;
        let p = &mut Reader::new(payload);
        let item = match tag {
            TAG_AUDIT_OPENING => {
                let replica = p.array()?;
                let mut leaves = Vec::with_capacity(params::AUDIT_OPENINGS);
                for _ in 0..params::AUDIT_OPENINGS {
                    leaves.push((Opened::decode(p)?, get_path(p)?));
                }
                Item::AuditOpening(AuditOpening { replica, leaves })
            }
            TAG_AUDIT_COMMIT => {
                Item::AuditCommit(AuditCommit { replica: p.array()?, pick: p.u64()?, root: p.array()?, sig: p.array()? })
            }
            TAG_REAUDIT => Item::Reaudit(Reaudit { replica: p.array()?, missed: p.u64()?, sig: p.array()? }),
            TAG_REG_PROOF => {
                let replica = p.array()?;
                let mut leaves = Vec::with_capacity(params::REGISTRATION_LEAVES);
                for _ in 0..params::REGISTRATION_LEAVES {
                    leaves.push(Opened::decode(p)?);
                }
                Item::RegProof(RegProof { replica, leaves })
            }
            TAG_VERDICT => Item::Verdict(Verdict {
                replica: p.array()?,
                height: p.u64()?,
                checker: p.array()?,
                vrf_proof: p.array()?,
                served: match p.u8()? {
                    0 => false,
                    1 => true,
                    _ => return Err(BlockError::Invalid("a verdict is 0 or 1")),
                },
                sig: p.array()?,
            }),
            TAG_REG_COMMIT => Item::RegCommit(RegCommit {
                segment: p.array()?,
                owner: p.array()?,
                payout: p.array()?,
                replica_root: p.array()?,
                anchor: p.u64()?,
                nonce: p.u64()?,
                sig: p.array()?,
            }),
            TAG_CATALOG_ENTRY => {
                let chain = get_bytes(p, MAX_CHAIN_KEY, "chain key too long")?;
                if chain.is_empty() || !chain.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit()) {
                    return Err(BlockError::Invalid("a chain key is lower-case letters and digits"));
                }
                Item::CatalogEntry(CatalogEntry {
                    chain: String::from_utf8(chain).expect("checked ASCII"),
                    family: p.u8()?,
                    first: p.u64()?,
                    last: p.u64()?,
                    bytes: p.u64()?,
                    manifest: p.array()?,
                    root: p.array()?,
                    poster: p.array()?,
                    sig: p.array()?,
                })
            }
            TAG_ATTEST => Item::Attest(Attest { segment: p.array()?, attester: p.array()?, sig: p.array()? }),
            TAG_EQUIVOCATION => Item::Equivocation(Box::new(Equivocation { a: Header::decode(p)?, b: Header::decode(p)? })),
            other => return Err(BlockError::UnknownTag(other)),
        };
        if !p.is_empty() {
            return Err(BlockError::Trailing("a storage item"));
        }
        Ok(item)
    }
}

// ---------------------------------------------------------------------------------------
// Block

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub header: Header,
    pub txs: Vec<Tx>,
    pub items: Vec<Item>,
}

/// The header's root over transaction ids, in block order.
pub fn tx_root(txs: &[Tx]) -> Hash {
    let mut b = merkle::RootBuilder::new();
    for tx in txs {
        b.push_leaf(&tx.txid.0);
    }
    b.finish().unwrap_or([0; 32])
}

/// The header's root over storage items, in block order; zeros when there are none.
pub fn storage_root(items: &[Item]) -> Hash {
    let mut b = merkle::RootBuilder::new();
    for item in items {
        b.push_hash(item.hash());
    }
    b.finish().unwrap_or([0; 32])
}

impl Block {
    pub fn id(&self) -> Hash256 {
        self.header.id()
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        self.header.encode(&mut o);
        write_varint(&mut o, self.txs.len() as u64);
        for tx in &self.txs {
            o.extend_from_slice(&tx.raw);
        }
        write_varint(&mut o, self.items.len() as u64);
        for item in &self.items {
            item.encode(&mut o);
        }
        o
    }

    pub fn decode(bytes: &[u8]) -> Result<Block, BlockError> {
        if bytes.len() > MAX_BLOCK_BYTES {
            return Err(BlockError::TooBig("block"));
        }
        let r = &mut Reader::new(bytes);
        let header = Header::decode(r)?;
        let n = r.len()?;
        let mut txs = Vec::with_capacity(n.min(4_096));
        for _ in 0..n {
            txs.push(Tx::decode_with(r, &TX_FORMAT)?);
        }
        let m = r.len()?;
        let mut items = Vec::with_capacity(m.min(1_024));
        for _ in 0..m {
            items.push(Item::decode(r)?);
        }
        if !r.is_empty() {
            return Err(BlockError::Trailing("the block"));
        }
        Ok(Block { header, txs, items })
    }

    /// Everything about a block (1 or later) that can be checked without the chain.
    pub fn check(&self, net: &Net) -> Result<(), BlockError> {
        self.header.check(net)?;

        // Coin transactions.
        let (first, rest) = self.txs.split_first().ok_or(BlockError::Empty)?;
        if !first.is_coinbase() {
            return Err(BlockError::NoCoinbase);
        }
        if rest.iter().any(Tx::is_coinbase) {
            return Err(BlockError::ExtraCoinbase);
        }
        if self.txs.iter().map(|t| t.raw.len()).sum::<usize>() > params::MAX_TX_BYTES {
            return Err(BlockError::TooBig("coin transactions"));
        }
        let mut ids: Vec<&Hash256> = self.txs.iter().map(|t| &t.txid).collect();
        ids.sort_unstable_by_key(|h| h.0);
        if ids.windows(2).any(|w| w[0] == w[1]) {
            return Err(BlockError::DuplicateTx);
        }
        if tx_root(&self.txs) != self.header.tx_root {
            return Err(BlockError::TxRoot);
        }

        // The storage section: in order, and within each kind's limit (§9).
        let mut counts = [0usize; 16];
        let mut last_group = 0;
        let mut bytes = 0;
        for item in &self.items {
            let g = item.group();
            if g < last_group {
                return Err(BlockError::ItemOrder);
            }
            last_group = g;
            counts[g as usize] += 1;
            bytes += item.encoded_len();
        }
        if bytes > params::MAX_STORAGE_BYTES {
            return Err(BlockError::TooBig("storage items"));
        }
        for (group, limit, what) in [
            (3, params::MAX_REGISTRATION_PROOFS, "registration proofs"),
            (4, params::MAX_VERDICTS, "serving verdicts"),
            (5, params::MAX_REGISTRATION_COMMITS, "registration commits"),
            (6, params::MAX_CATALOG_ITEMS, "catalog items"),
        ] {
            if counts[group] > limit {
                return Err(BlockError::ItemLimit(what));
            }
        }
        if storage_root(&self.items) != self.header.storage_root {
            return Err(BlockError::StorageRoot);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{MAINNET, TESTNET};

    fn key(n: u8) -> SigningKey {
        SigningKey::from_bytes(&[n; 32])
    }

    fn coinbase(height: u64, value: u64) -> Tx {
        let mut raw = vec![1, 0, 0, 0, 1];
        raw.extend_from_slice(&[0; 32]);
        raw.extend_from_slice(&[0xff; 4]);
        let script = height.to_le_bytes();
        put_bytes(&mut raw, &script);
        raw.extend_from_slice(&[0xff; 4]);
        raw.push(1);
        raw.extend_from_slice(&value.to_le_bytes());
        put_bytes(&mut raw, &[0x76, 0xa9, 0x14]);
        raw.extend_from_slice(&[0; 4]);
        Tx::parse_with(&raw, &TX_FORMAT).unwrap()
    }

    fn spend(n: u8) -> Tx {
        let mut raw = vec![1, 0, 0, 0, 1];
        raw.extend_from_slice(&[n; 32]);
        raw.extend_from_slice(&[0; 4]);
        put_bytes(&mut raw, &[n]);
        raw.extend_from_slice(&[0xff; 4]);
        raw.push(1);
        raw.extend_from_slice(&7u64.to_le_bytes());
        put_bytes(&mut raw, &[0x51]);
        raw.extend_from_slice(&[0; 4]);
        Tx::parse_with(&raw, &TX_FORMAT).unwrap()
    }

    fn header(height: u64) -> Header {
        Header {
            version: HEADER_VERSION,
            height,
            prev: Hash256([9; 32]),
            time: 1_798_761_750,
            tx_root: [0; 32],
            storage_root: [0; 32],
            difficulty: 1 << 40,
            writer: [0; 32],
            weight: 1 << 30,
            vrf_proof: [3; 80],
            pot: TimeProof { output: vec![1; 100], proof: vec![2; 100] },
            draw_seed: (height % params::EPOCH_BLOCKS == 0).then(|| TimeProof { output: vec![4; 100], proof: vec![5; 100] }),
            sig: [0; 64],
        }
    }

    fn opened(n: u8) -> Opened {
        Opened { sealed: [n; SEALED_LEAF], sealed_path: vec![[n; 32]; 24], data: [n; DATA_LEAF], data_path: vec![[n; 32]; 24] }
    }

    fn one_of_each() -> Vec<Item> {
        let net = &TESTNET;
        let owner = key(1);
        let mut audit = AuditCommit { replica: [1; 32], pick: 77, root: [2; 32], sig: [0; 64] };
        audit.sig = sign(&owner, &audit.message(net, &Hash256([8; 32])));
        let mut entry = CatalogEntry {
            chain: "doge".into(),
            family: 0,
            first: 0,
            last: 1_239_999,
            bytes: 1 << 30,
            manifest: [5; 32],
            root: [6; 32],
            poster: owner.verifying_key().to_bytes(),
            sig: [0; 64],
        };
        entry.sig = sign(&owner, &entry.message(net));
        let (mut a, mut b) = (header(5), header(5));
        b.time += 1;
        a.sign(net, &owner);
        b.sign(net, &owner);
        vec![
            Item::AuditOpening(AuditOpening {
                replica: [1; 32],
                leaves: (0..4).map(|i| (opened(i), vec![[i; 32]; 24])).collect(),
            }),
            Item::AuditCommit(audit),
            Item::Reaudit(Reaudit { replica: [1; 32], missed: 70, sig: [7; 64] }),
            Item::RegProof(RegProof { replica: [1; 32], leaves: (0..16).map(opened).collect() }),
            Item::Verdict(Verdict {
                replica: [1; 32],
                height: 60,
                checker: [4; 32],
                vrf_proof: [5; 80],
                served: true,
                sig: [6; 64],
            }),
            Item::RegCommit(RegCommit {
                segment: [1; 32],
                owner: owner.verifying_key().to_bytes(),
                payout: [0x53; 21],
                replica_root: [2; 32],
                anchor: 50,
                nonce: 123_456,
                sig: [3; 64],
            }),
            Item::CatalogEntry(entry),
            Item::Attest(Attest { segment: [1; 32], attester: [2; 32], sig: [3; 64] }),
            Item::Equivocation(Box::new(Equivocation { a, b })),
        ]
    }

    fn block(height: u64, txs: Vec<Tx>, items: Vec<Item>) -> Block {
        let mut h = header(height);
        h.tx_root = tx_root(&txs);
        h.storage_root = storage_root(&items);
        h.sign(&TESTNET, &key(1));
        Block { header: h, txs, items }
    }

    #[test]
    fn a_header_survives_the_wire() {
        for height in [1, 100, 12_345] {
            let mut h = header(height);
            h.sign(&TESTNET, &key(1));
            let mut bytes = Vec::new();
            h.encode(&mut bytes);
            let r = &mut Reader::new(&bytes);
            assert_eq!(Header::decode(r).unwrap(), h);
            assert!(r.is_empty());
            assert_eq!(h.check(&TESTNET), Ok(()));
        }
    }

    #[test]
    fn the_id_covers_everything_but_the_signature() {
        let mut h = header(1);
        h.sign(&TESTNET, &key(1));
        let id = h.id();
        let mut other = h.clone();
        other.sig = [0; 64];
        assert_eq!(other.id(), id);
        // Each field moves the id.
        let changes: Vec<fn(&mut Header)> = vec![
            |h| h.height += 1,
            |h| h.prev.0[0] ^= 1,
            |h| h.time += 1,
            |h| h.tx_root[0] ^= 1,
            |h| h.storage_root[0] ^= 1,
            |h| h.difficulty += 1,
            |h| h.writer[0] ^= 1,
            |h| h.weight += 1,
            |h| h.vrf_proof[79] ^= 1,
            |h| h.pot.output[0] ^= 1,
            |h| h.pot.proof[0] ^= 1,
            |h| h.draw_seed = Some(TimeProof::default()),
        ];
        for change in changes {
            let mut c = h.clone();
            change(&mut c);
            assert_ne!(c.id(), id);
            assert!(!c.signature_ok(&TESTNET));
        }
    }

    #[test]
    fn a_signature_is_for_one_network_and_one_writer() {
        let mut h = header(1);
        h.sign(&TESTNET, &key(1));
        assert!(h.signature_ok(&TESTNET));
        assert!(!h.signature_ok(&MAINNET));
        let mut stolen = h.clone();
        stolen.writer = key(2).verifying_key().to_bytes();
        assert!(!stolen.signature_ok(&TESTNET));
        assert_eq!(stolen.check(&TESTNET), Err(BlockError::BadSignature("block")));
    }

    #[test]
    fn a_header_is_refused_for_what_it_shows_alone() {
        let signed = |mut h: Header| {
            h.sign(&TESTNET, &key(1));
            h
        };
        let mut h = header(1);
        h.difficulty = 0;
        assert!(signed(h).check(&TESTNET).is_err());
        let mut h = header(1);
        h.weight = 0;
        assert!(signed(h).check(&TESTNET).is_err());
        // The draw-seed proof: in the first block of an epoch and nowhere else.
        let mut h = header(100);
        h.draw_seed = None;
        assert!(signed(h).check(&TESTNET).is_err());
        let mut h = header(101);
        h.draw_seed = Some(TimeProof::default());
        assert!(signed(h).check(&TESTNET).is_err());
        assert!(signed(header(0)).check(&TESTNET).is_err());
        // Decoding: an unknown version, a bad flag, an over-long proof.
        let mut bytes = Vec::new();
        signed(header(1)).encode(&mut bytes);
        let mut v2 = bytes.clone();
        v2[0] = 2;
        assert!(Header::decode(&mut Reader::new(&v2)).is_err());
        let mut long = header(1);
        long.pot.proof = vec![0; MAX_TIME_BYTES + 1];
        let mut bytes = Vec::new();
        long.encode(&mut bytes);
        assert!(Header::decode(&mut Reader::new(&bytes)).is_err());
    }

    #[test]
    fn every_item_survives_the_wire() {
        let items = one_of_each();
        let tags: Vec<u8> = items.iter().map(Item::tag).collect();
        assert_eq!(tags, [0x10, 0x20, 0x21, 0x30, 0x40, 0x50, 0x60, 0x61, 0x64]);
        for item in &items {
            let mut bytes = Vec::new();
            item.encode(&mut bytes);
            assert_eq!(bytes.len(), item.encoded_len());
            let r = &mut Reader::new(&bytes);
            assert_eq!(&Item::decode(r).unwrap(), item);
            assert!(r.is_empty());

            // One byte more or less inside the payload is refused.
            let mut longer = vec![item.tag()];
            let mut payload = item.payload();
            payload.push(0);
            put_bytes(&mut longer, &payload);
            assert!(Item::decode(&mut Reader::new(&longer)).is_err(), "tag {:#x}", item.tag());
            let mut shorter = vec![item.tag()];
            payload.truncate(payload.len() - 2);
            put_bytes(&mut shorter, &payload);
            assert!(Item::decode(&mut Reader::new(&shorter)).is_err(), "tag {:#x}", item.tag());
        }
        assert_eq!(Item::decode(&mut Reader::new(&[0x7f, 0])), Err(BlockError::UnknownTag(0x7f)));
    }

    #[test]
    fn item_sizes_are_what_the_design_counts_on() {
        // §9's table: these sizes set the per-block limits and the scaling limit.
        let sizes: Vec<usize> = one_of_each().iter().map(Item::encoded_len).collect();
        assert!((10_000..11_000).contains(&sizes[0]), "audit opening {}", sizes[0]);
        assert_eq!(sizes[1], 1 + 1 + 136, "audit commit");
        assert!((28_000..30_000).contains(&sizes[3]), "registration proof {}", sizes[3]);
        assert_eq!(sizes[4], 1 + 1 + 217, "verdict");
        assert_eq!(sizes[5], 1 + 1 + 197, "registration commit");
    }

    #[test]
    fn signed_items_are_bound_to_their_block() {
        let net = &TESTNET;
        let owner = key(1);
        let pk = owner.verifying_key().to_bytes();
        let mut a = AuditCommit { replica: [1; 32], pick: 77, root: [2; 32], sig: [0; 64] };
        let pick = Hash256([8; 32]);
        a.sig = sign(&owner, &a.message(net, &pick));
        assert!(verify(&pk, &a.message(net, &pick), &a.sig));
        // The same height on another branch, another network, another key.
        assert!(!verify(&pk, &a.message(net, &Hash256([9; 32])), &a.sig));
        assert!(!verify(&pk, &a.message(&MAINNET, &pick), &a.sig));
        assert!(!verify(&key(2).verifying_key().to_bytes(), &a.message(net, &pick), &a.sig));

        let c = RegCommit {
            segment: [1; 32],
            owner: pk,
            payout: [0x53; 21],
            replica_root: [2; 32],
            anchor: 50,
            nonce: 0,
            sig: [0; 64],
        };
        let anchor = Hash256([7; 32]);
        assert_ne!(c.stamp(net, &anchor), c.stamp(net, &Hash256([6; 32])));
        let mut d = c.clone();
        d.nonce = 1;
        assert_ne!(c.stamp(net, &anchor), d.stamp(net, &anchor));
        assert_eq!(c.replica(net), replica_id(net, &[1; 32], &pk));
        assert_ne!(c.replica(net), c.replica(&MAINNET));
    }

    #[test]
    fn work_is_counted_in_leading_zero_bits() {
        let mut h = [0xff; 32];
        assert_eq!(work_bits(&h), 0);
        h[0] = 0x1f;
        assert_eq!(work_bits(&h), 3);
        h[0] = 0;
        h[1] = 0;
        h[2] = 0x01;
        assert_eq!(work_bits(&h), 23);
        assert_eq!(work_bits(&[0; 32]), 256);
    }

    #[test]
    fn a_segment_id_names_one_set_of_bytes() {
        let net = &TESTNET;
        let items = one_of_each();
        let Item::CatalogEntry(e) = &items[6] else { panic!() };
        let id = e.segment(net);
        assert_eq!(e.leaves(), (1u64 << 30).div_ceil(127));
        assert!(verify(&e.poster, &e.message(net), &e.sig));
        let changes: Vec<fn(&mut CatalogEntry)> = vec![
            |e| e.chain = "dog".into(),
            |e| e.family = 1,
            |e| e.first = 1,
            |e| e.last += 1,
            |e| e.bytes += 1,
            |e| e.root[0] ^= 1,
        ];
        for change in changes {
            let mut c = e.clone();
            change(&mut c);
            assert_ne!(c.segment(net), id);
        }
        assert_ne!(e.segment(&MAINNET), id);
        // The manifest id is a pointer for fetching; the bytes are named by the root.
        let mut c = e.clone();
        c.manifest[0] ^= 1;
        assert_eq!(c.segment(net), id);
        assert!(!verify(&c.poster, &c.message(net), &c.sig));
        // The chain key's length is part of the id: "dog" + family 0x65 isn't "doge".
        let mut c = e.clone();
        c.chain = "dog".into();
        c.family = b'e';
        assert_ne!(c.segment(net), id);
    }

    #[test]
    fn two_signed_blocks_on_one_parent_prove_cheating() {
        let net = &TESTNET;
        let (mut a, mut b) = (header(5), header(5));
        b.tx_root[0] ^= 1;
        a.sign(net, &key(1));
        b.sign(net, &key(1));
        assert!(Equivocation { a: a.clone(), b: b.clone() }.proves(net));
        // The same block twice, another parent, another writer, a forged signature: no.
        assert!(!Equivocation { a: a.clone(), b: a.clone() }.proves(net));
        let mut c = b.clone();
        c.prev.0[0] ^= 1;
        c.sign(net, &key(1));
        assert!(!Equivocation { a: a.clone(), b: c }.proves(net));
        let mut d = b.clone();
        d.sign(net, &key(2));
        assert!(!Equivocation { a: a.clone(), b: d }.proves(net));
        let mut e = b.clone();
        e.time += 1;
        assert!(!Equivocation { a, b: e }.proves(net));
    }

    #[test]
    fn a_block_survives_the_wire_and_checks() {
        let b = block(7, vec![coinbase(7, 0), spend(1), spend(2)], one_of_each());
        let bytes = b.encode();
        let back = Block::decode(&bytes).unwrap();
        assert_eq!(back, b);
        assert_eq!(back.id(), b.id());
        assert_eq!(back.check(&TESTNET), Ok(()));
        assert_eq!(back.check(&MAINNET), Err(BlockError::BadSignature("block")));

        // No storage items: the root is zeros.
        let empty = block(7, vec![coinbase(7, 0)], vec![]);
        assert_eq!(empty.header.storage_root, [0; 32]);
        assert_eq!(Block::decode(&empty.encode()).unwrap().check(&TESTNET), Ok(()));

        // A byte more, a byte less.
        let mut more = bytes.clone();
        more.push(0);
        assert_eq!(Block::decode(&more), Err(BlockError::Trailing("the block")));
        assert!(Block::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn the_id_covers_both_parts() {
        let b = block(7, vec![coinbase(7, 0), spend(1)], one_of_each());
        // Swap, drop or add a transaction or an item under the same header: refused.
        let mut c = b.clone();
        c.txs.push(spend(2));
        assert_eq!(c.check(&TESTNET), Err(BlockError::TxRoot));
        let mut c = b.clone();
        c.txs.pop();
        assert_eq!(c.check(&TESTNET), Err(BlockError::TxRoot));
        let mut c = b.clone();
        c.items.pop();
        assert_eq!(c.check(&TESTNET), Err(BlockError::StorageRoot));
        let mut c = b.clone();
        c.items.swap(1, 2);
        assert_eq!(c.check(&TESTNET), Err(BlockError::StorageRoot));
        let mut c = b.clone();
        if let Item::AuditCommit(a) = &mut c.items[1] {
            a.root[0] ^= 1;
        }
        assert_eq!(c.check(&TESTNET), Err(BlockError::StorageRoot));
    }

    #[test]
    fn coin_transactions_are_one_coinbase_then_spends() {
        assert_eq!(block(7, vec![], vec![]).check(&TESTNET), Err(BlockError::Empty));
        assert_eq!(block(7, vec![spend(1)], vec![]).check(&TESTNET), Err(BlockError::NoCoinbase));
        assert_eq!(block(7, vec![coinbase(7, 0), coinbase(7, 1)], vec![]).check(&TESTNET), Err(BlockError::ExtraCoinbase));
        assert_eq!(block(7, vec![coinbase(7, 0), spend(1), spend(1)], vec![]).check(&TESTNET), Err(BlockError::DuplicateTx));
    }

    #[test]
    fn the_storage_section_keeps_its_order_and_limits() {
        let cb = || vec![coinbase(7, 0)];
        let all = one_of_each();
        // A commit ahead of an opening: answers must come first.
        let mut wrong = all.clone();
        wrong.swap(0, 1);
        assert_eq!(block(7, cb(), wrong).check(&TESTNET), Err(BlockError::ItemOrder));
        // Inside one group the order is free.
        let mut fine = all.clone();
        fine.swap(1, 2);
        assert_eq!(block(7, cb(), fine).check(&TESTNET), Ok(()));

        let many = |item: &Item, n: usize| vec![item.clone(); n];
        assert_eq!(block(7, cb(), many(&all[3], 8)).check(&TESTNET), Ok(()));
        assert_eq!(block(7, cb(), many(&all[3], 9)).check(&TESTNET), Err(BlockError::ItemLimit("registration proofs")));
        assert_eq!(block(7, cb(), many(&all[4], 400)).check(&TESTNET), Ok(()));
        assert_eq!(block(7, cb(), many(&all[4], 401)).check(&TESTNET), Err(BlockError::ItemLimit("serving verdicts")));
        assert_eq!(block(7, cb(), many(&all[5], 201)).check(&TESTNET), Err(BlockError::ItemLimit("registration commits")));
        assert_eq!(block(7, cb(), many(&all[7], 65)).check(&TESTNET), Err(BlockError::ItemLimit("catalog items")));
        // Openings have no count limit, only the section's size: 1 MB holds 96 of them.
        let fits = params::MAX_STORAGE_BYTES / all[0].encoded_len();
        assert_eq!(block(7, cb(), many(&all[0], fits)).check(&TESTNET), Ok(()));
        assert_eq!(block(7, cb(), many(&all[0], fits + 1)).check(&TESTNET), Err(BlockError::TooBig("storage items")));
    }
}
