//! ChainDb: blocks stored once, indexes updated in the same commit, reorgs by undo.

use crate::schema::*;
use crate::undo::{self, Logged};
use arcade_chain::params::ChainParams;
use arcade_chain::pow::{work_from_bits, U256};
use arcade_chain::{Block, Hash256, Header, OutPoint, Tx};
use arcade_store::key::{be_u32, be_u64};
use arcade_store::{ReadTxn, Result, Store, StoreError, WriteTxn};
use std::path::Path;

/// Headers kept in the undo window; deeper reorgs are refused.
pub const UNDO_DEPTH: u32 = 1000;

pub const STATUS_HEADER: u8 = 0;
pub const STATUS_MAIN: u8 = 1;
pub const STATUS_INVALID: u8 = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeaderRec {
    /// Local block number once the block is stored; 0 = header only.
    pub b: u32,
    pub height: u32,
    pub status: u8,
    /// Total chain work up to and including this header.
    pub work: U256,
    pub header: Header,
    /// Unused (kept empty): AuxPoW proofs live once, with the block, in block_meta.
    pub auxpow: Vec<u8>,
}

impl HeaderRec {
    fn index_value(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(9);
        v.extend_from_slice(&self.height.to_be_bytes());
        v.extend_from_slice(&self.b.to_be_bytes());
        v.push(self.status);
        v
    }
    fn data_value(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(112);
        v.extend_from_slice(&self.header.encode());
        v.extend_from_slice(&self.work.to_be_bytes());
        v
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tip {
    pub hash: Hash256,
    pub height: u32,
    pub b: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Coin {
    pub value: u64,
    pub height: u32,
    pub coinbase: bool,
    pub script: Vec<u8>,
}

/// Outputs that can never be spent are not coins.
pub fn is_unspendable(value: u64, script: &[u8]) -> bool {
    script.first() == Some(&0x6a) || (value == 0 && script.is_empty())
}

fn other(e: impl std::fmt::Display) -> StoreError {
    StoreError::Other(e.to_string())
}

// ---------- script compression (outputs value) ----------
// 00+h160 P2PKH | 01+h160 P2SH | 02/03+32 P2PK compressed | 05+65 P2PK uncompressed
// | FE = read the script from the stored transaction (anything else).

const SCRIPT_IN_TX: u8 = 0xfe;

fn compress_script(s: &[u8]) -> Vec<u8> {
    if s.len() == 25 && s[0] == 0x76 && s[1] == 0xa9 && s[2] == 0x14 && s[23] == 0x88 && s[24] == 0xac {
        let mut v = vec![0x00];
        v.extend_from_slice(&s[3..23]);
        v
    } else if s.len() == 23 && s[0] == 0xa9 && s[1] == 0x14 && s[22] == 0x87 {
        let mut v = vec![0x01];
        v.extend_from_slice(&s[2..22]);
        v
    } else if s.len() == 35 && s[0] == 33 && s[34] == 0xac && (s[1] == 2 || s[1] == 3) {
        s[1..34].to_vec()
    } else if s.len() == 67 && s[0] == 65 && s[66] == 0xac {
        let mut v = vec![0x05];
        v.extend_from_slice(&s[1..66]);
        v
    } else {
        vec![SCRIPT_IN_TX]
    }
}

/// Expand a compressed script; `from_tx` reads the full script when it was not inlined.
fn expand_script(r: &[u8], from_tx: impl FnOnce() -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    let bad = || StoreError::Corrupt { table: OUTPUTS, why: "bad script ref" };
    Ok(match r.first().copied().ok_or_else(bad)? {
        0x00 => {
            let mut s = vec![0x76, 0xa9, 0x14];
            s.extend_from_slice(r.get(1..21).ok_or_else(bad)?);
            s.extend_from_slice(&[0x88, 0xac]);
            s
        }
        0x01 => {
            let mut s = vec![0xa9, 0x14];
            s.extend_from_slice(r.get(1..21).ok_or_else(bad)?);
            s.push(0x87);
            s
        }
        0x02 | 0x03 => {
            let mut s = vec![33];
            s.extend_from_slice(r.get(..33).ok_or_else(bad)?);
            s.push(0xac);
            s
        }
        0x05 => {
            let mut s = vec![65];
            s.extend_from_slice(r.get(1..66).ok_or_else(bad)?);
            s.push(0xac);
            s
        }
        SCRIPT_IN_TX => from_tx()?,
        _ => return Err(bad()),
    })
}

fn encode_coin(value: u64, height: u32, coinbase: bool, script: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(13 + 22);
    v.extend_from_slice(&value.to_le_bytes());
    v.extend_from_slice(&height.to_le_bytes());
    v.push(coinbase as u8);
    v.extend_from_slice(&compress_script(script));
    v
}

fn block_meta(b: &Block) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&b.header.encode());
    let aux = b.auxpow.as_ref().map(|a| a.raw.as_slice()).unwrap_or(&[]);
    let field = |v: &mut Vec<u8>, x: &[u8]| {
        v.extend_from_slice(&(x.len() as u32).to_le_bytes());
        v.extend_from_slice(x);
    };
    field(&mut v, aux);
    match &b.sig {
        Some(s) => {
            v.push(1);
            field(&mut v, s);
        }
        None => v.push(0),
    }
    field(&mut v, &b.extra);
    v.extend_from_slice(&(b.txs.len() as u32).to_le_bytes());
    v
}

struct Meta {
    header80: [u8; 80],
    auxpow: Vec<u8>,
    sig: Option<Vec<u8>>,
    extra: Vec<u8>,
    tx_count: u32,
}

fn parse_meta(v: &[u8]) -> Result<Meta> {
    let bad = || StoreError::Corrupt { table: BLOCK_META, why: "bad block meta" };
    let mut p = 80usize;
    let field = |p: &mut usize| -> Result<Vec<u8>> {
        let l = u32::from_le_bytes(v.get(*p..*p + 4).ok_or_else(bad)?.try_into().unwrap()) as usize;
        let s = v.get(*p + 4..*p + 4 + l).ok_or_else(bad)?.to_vec();
        *p += 4 + l;
        Ok(s)
    };
    let header80: [u8; 80] = v.get(..80).ok_or_else(bad)?.try_into().unwrap();
    let auxpow = field(&mut p)?;
    let has_sig = *v.get(p).ok_or_else(bad)?;
    p += 1;
    let sig = if has_sig == 1 { Some(field(&mut p)?) } else { None };
    let extra = field(&mut p)?;
    let tx_count = u32::from_le_bytes(v.get(p..p + 4).ok_or_else(bad)?.try_into().unwrap());
    Ok(Meta { header80, auxpow, sig, extra, tx_count })
}

/// Read access shared by read and write transactions.
pub trait Rd {
    fn get(&self, table: &str, key: &[u8]) -> Result<Option<Vec<u8>>>;
    fn scan(&self, table: &str, prefix: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;
}
impl Rd for ReadTxn<'_> {
    fn get(&self, t: &str, k: &[u8]) -> Result<Option<Vec<u8>>> {
        ReadTxn::get(self, t, k)
    }
    fn scan(&self, t: &str, p: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_prefix(t, p)
    }
}
impl Rd for WriteTxn<'_> {
    fn get(&self, t: &str, k: &[u8]) -> Result<Option<Vec<u8>>> {
        WriteTxn::get(self, t, k)
    }
    fn scan(&self, t: &str, p: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        self.scan_prefix(t, p)
    }
}

fn header_rec<R: Rd>(r: &R, c: u16, hash: &Hash256) -> Result<Option<HeaderRec>> {
    let Some(ix) = r.get(HEADERS, &k_header(c, &hash.0))? else { return Ok(None) };
    let (height, b, status) = (be_u32(&ix, 0), be_u32(&ix, 4), ix[8]);
    let d = r
        .get(HDR, &k_hdr(c, height, &hash.0))?
        .ok_or(StoreError::Corrupt { table: HDR, why: "header index without header data" })?;
    Ok(Some(HeaderRec {
        b,
        height,
        status,
        work: U256::from_be_bytes(d[80..112].try_into().unwrap()),
        header: Header::parse(d[..80].try_into().unwrap()),
        auxpow: Vec::new(),
    }))
}

/// Where a transaction is stored: (B, P). Several candidates only on a T12 collision or a
/// duplicate txid; then each is checked by full hash.
fn locate<R: Rd>(r: &R, params: &ChainParams, txid: &Hash256) -> Result<Option<(u32, u32)>> {
    let c = params.chain_no;
    let cands = r.scan(TXS, &k_txs(c, &txid.t12()))?;
    if cands.len() == 1 {
        let bp = &cands[0].1;
        return Ok(Some((be_u32(bp, 0), be_u32(bp, 4))));
    }
    let mut found = None;
    for (_, bp) in cands {
        let (b, p) = (be_u32(&bp, 0), be_u32(&bp, 4));
        if let Some(raw) = r.get(BLOCK_TX, &k_block_tx(c, b, p))? {
            if Tx::parse_with(&raw, &params.format).map_err(other)?.txid == *txid {
                found = Some((b, p)); // the latest stored copy wins (BIP30-style duplicates)
            }
        }
    }
    Ok(found)
}

fn coin_at<R: Rd>(r: &R, params: &ChainParams, b: u32, p: u32, vout: u32) -> Result<Option<Coin>> {
    let c = params.chain_no;
    let Some(v) = r.get(OUTPUTS, &k_pos(c, b, p, vout))? else { return Ok(None) };
    let script = expand_script(&v[13..], || {
        let raw = r.get(BLOCK_TX, &k_block_tx(c, b, p))?.ok_or(StoreError::Corrupt { table: BLOCK_TX, why: "coin's tx missing" })?;
        let tx = Tx::parse_with(&raw, &params.format).map_err(other)?;
        Ok(tx.outputs.get(vout as usize).ok_or(StoreError::Corrupt { table: OUTPUTS, why: "bad vout" })?.script_pubkey.clone())
    })?;
    Ok(Some(Coin {
        value: u64::from_le_bytes(v[0..8].try_into().unwrap()),
        height: u32::from_le_bytes(v[8..12].try_into().unwrap()),
        coinbase: v[12] != 0,
        script,
    }))
}

pub struct ChainDb {
    pub store: Store,
}

impl ChainDb {
    pub fn open(path: impl AsRef<Path>) -> Result<ChainDb> {
        Ok(ChainDb { store: Store::open(path, &CHAIN_SCHEMA)? })
    }

    // ---------- reads ----------

    pub fn header(&self, c: u16, hash: &Hash256) -> Result<Option<HeaderRec>> {
        self.store.read(|r| header_rec(r, c, hash))
    }

    pub fn tip(&self, c: u16) -> Result<Option<Tip>> {
        self.store.read(|r| Ok(r.get(STATE, &k_state(c, "tip"))?.map(|v| decode_tip(&v))))
    }

    /// Best known header (most work), which may be ahead of the tip.
    pub fn best_header(&self, c: u16) -> Result<Option<Hash256>> {
        self.store.read(|r| Ok(r.get(STATE, &k_state(c, "best"))?.map(|v| Hash256(v[..32].try_into().unwrap()))))
    }

    pub fn hash_at(&self, c: u16, height: u32) -> Result<Option<Hash256>> {
        self.store.read(|r| Ok(r.get(HEIGHT, &k_height(c, height))?.map(|v| Hash256(v[..32].try_into().unwrap()))))
    }

    /// Raw block bytes, rebuilt exactly from the stored parts.
    pub fn block_raw(&self, c: u16, hash: &Hash256) -> Result<Option<Vec<u8>>> {
        let Some(h) = self.header(c, hash)? else { return Ok(None) };
        if h.b == 0 {
            return Ok(None);
        }
        self.store.read(|r| {
            let m = parse_meta(&r.get(BLOCK_META, &k_block(c, h.b))?.ok_or(StoreError::Corrupt {
                table: BLOCK_META,
                why: "header points at a missing block",
            })?)?;
            let mut o = m.header80.to_vec();
            o.extend_from_slice(&m.auxpow);
            arcade_chain::encode::write_varint(&mut o, m.tx_count as u64);
            for (_, tx) in r.scan_prefix(BLOCK_TX, &k_block(c, h.b))? {
                o.extend_from_slice(&tx);
            }
            if let Some(s) = m.sig {
                arcade_chain::encode::write_varint(&mut o, s.len() as u64);
                o.extend_from_slice(&s);
            }
            o.extend_from_slice(&m.extra);
            Ok(Some(o))
        })
    }

    /// Headers after the first locator hash on our main chain, as a P2P `headers` payload
    /// (header [+ AuxPoW] + tx count 0 [+ sig length 0]); at most `max`.
    pub fn headers_payload(&self, params: &ChainParams, locator: &[Hash256], max: u32) -> Result<Vec<u8>> {
        let c = params.chain_no;
        let tip = match self.tip(c)? {
            Some(t) => t,
            None => return Ok(vec![0]),
        };
        let mut start = 0u32;
        for h in locator {
            if let Some(rec) = self.header(c, h)? {
                if self.hash_at(c, rec.height)? == Some(*h) {
                    start = rec.height + 1;
                    break;
                }
            }
        }
        let end = tip.height.min(start.saturating_add(max).saturating_sub(1));
        let n = if start > tip.height { 0 } else { end - start + 1 };
        self.store.read(|r| {
            let mut o = Vec::new();
            arcade_chain::encode::write_varint(&mut o, n as u64);
            for h in start..start + n {
                let v = r.get(HEIGHT, &k_height(c, h))?.ok_or(StoreError::Corrupt { table: HEIGHT, why: "gap" })?;
                let b = be_u32(&v, 32);
                let m = parse_meta(&r.get(BLOCK_META, &k_block(c, b))?.ok_or(StoreError::Corrupt {
                    table: BLOCK_META,
                    why: "missing block",
                })?)?;
                o.extend_from_slice(&m.header80);
                o.extend_from_slice(&m.auxpow);
                o.push(0);
                if params.format.block_sig {
                    o.push(0);
                }
            }
            Ok(o)
        })
    }

    /// Raw transaction by txid (checked by full hash) and its block number.
    pub fn tx_raw(&self, params: &ChainParams, txid: &Hash256) -> Result<Option<(Vec<u8>, u32)>> {
        let c = params.chain_no;
        self.store.read(|r| {
            for (_, bp) in r.scan_prefix(TXS, &k_txs(c, &txid.t12()))? {
                let (b, p) = (be_u32(&bp, 0), be_u32(&bp, 4));
                let Some(raw) = r.get(BLOCK_TX, &k_block_tx(c, b, p))? else { continue };
                if Tx::parse_with(&raw, &params.format).map_err(other)?.txid == *txid {
                    return Ok(Some((raw, b)));
                }
            }
            Ok(None)
        })
    }

    /// Full txids of stored transactions whose id starts with `t12` (normally one).
    pub fn txids_by_t12(&self, params: &ChainParams, t12: &[u8; 12]) -> Result<Vec<Hash256>> {
        let c = params.chain_no;
        self.store.read(|r| {
            let mut out = Vec::new();
            for (_, bp) in r.scan_prefix(TXS, &k_txs(c, t12))? {
                if let Some(raw) = r.get(BLOCK_TX, &k_block_tx(c, be_u32(&bp, 0), be_u32(&bp, 4)))? {
                    let tx = Tx::parse_with(&raw, &params.format).map_err(other)?;
                    if !out.contains(&tx.txid) {
                        out.push(tx.txid);
                    }
                }
            }
            Ok(out)
        })
    }

    /// An unspent output.
    pub fn coin(&self, params: &ChainParams, op: &OutPoint) -> Result<Option<Coin>> {
        self.store.read(|r| match locate(r, params, &op.txid)? {
            Some((b, p)) => coin_at(r, params, b, p, op.vout),
            None => Ok(None),
        })
    }

    /// Every unspent coin of the address `script` belongs to: (outpoint, value, height).
    pub fn coins_for_script(&self, params: &ChainParams, script: &[u8]) -> Result<Vec<(OutPoint, u64, u32)>> {
        let c = params.chain_no;
        let sid = script_id(script);
        self.store.read(|r| {
            let pre = k_sid(c, &sid);
            let mut out = Vec::new();
            for (k, v) in r.scan_prefix(BY_SCRIPT, &pre)? {
                let n = pre.len();
                let (b, p, vout) = (be_u32(&k, n), be_u32(&k, n + 4), be_u32(&k, n + 8));
                let raw = r.get(BLOCK_TX, &k_block_tx(c, b, p))?.ok_or(StoreError::Corrupt { table: BLOCK_TX, why: "coin's tx missing" })?;
                let txid = Tx::parse_with(&raw, &params.format).map_err(other)?.txid;
                out.push((
                    OutPoint { txid, vout },
                    u64::from_le_bytes(v[0..8].try_into().unwrap()),
                    u32::from_le_bytes(v[8..12].try_into().unwrap()),
                ));
            }
            Ok(out)
        })
    }

    /// Sum of an address's unspent coins (no transaction reads).
    pub fn balance_for_script(&self, c: u16, script: &[u8]) -> Result<u64> {
        let sid = script_id(script);
        Ok(self
            .store
            .read(|r| r.scan_prefix(BY_SCRIPT, &k_sid(c, &sid)))?
            .iter()
            .map(|(_, v)| u64::from_le_bytes(v[0..8].try_into().unwrap()))
            .sum())
    }

    /// An address's history rows: (height, txid prefix, received, spent).
    pub fn history_for_script(&self, c: u16, script: &[u8]) -> Result<Vec<(u32, [u8; 12], u64, u64)>> {
        let sid = script_id(script);
        let pre = k_sid(c, &sid);
        Ok(self
            .store
            .read(|r| r.scan_prefix(SCRIPT_HIST, &pre))?
            .iter()
            .map(|(k, v)| {
                let n = pre.len();
                (be_u32(k, n), k[n + 4..n + 16].try_into().unwrap(), be_u64(v, 0), be_u64(v, 8))
            })
            .collect())
    }

    /// Who spent an outpoint: (block number, position, input index).
    pub fn spender(&self, params: &ChainParams, op: &OutPoint) -> Result<Option<(u32, u32, u32)>> {
        let c = params.chain_no;
        self.store.read(|r| match locate(r, params, &op.txid)? {
            Some((b, p)) => Ok(r
                .get(SPENDS, &k_pos(c, b, p, op.vout))?
                .map(|v| (be_u32(&v, 0), be_u32(&v, 4), be_u32(&v, 8)))),
            None => Ok(None),
        })
    }

    // ---------- mempool ----------

    /// Add an unconfirmed transaction (not undo-logged: the mempool is volatile).
    /// Refused if it is already in a block, already pooled, or spends a coin another
    /// pooled transaction spends. Returns whether it was added.
    pub fn mempool_add(&self, params: &ChainParams, tx: &Tx, now: u64) -> Result<bool> {
        let c = params.chain_no;
        if self.tx_raw(params, &tx.txid)?.is_some() {
            return Ok(false);
        }
        self.store.write(|w| {
            let k = k_txs(c, &tx.txid.t12());
            if w.contains(MEMPOOL, &k)? {
                return Ok(false);
            }
            for i in &tx.inputs {
                if w.contains(MEMPOOL_SPENDS, &k_out(c, &i.prevout.txid.t12(), i.prevout.vout))? {
                    return Ok(false);
                }
            }
            let mut v = now.to_le_bytes().to_vec();
            v.extend_from_slice(&tx.raw);
            w.put(MEMPOOL, &k, &v)?;
            for i in &tx.inputs {
                w.put(MEMPOOL_SPENDS, &k_out(c, &i.prevout.txid.t12(), i.prevout.vout), &tx.txid.0)?;
            }
            Ok(true)
        })
    }

    /// Every pooled transaction: (first seen, raw).
    pub fn mempool(&self, c: u16) -> Result<Vec<(u64, Vec<u8>)>> {
        Ok(self
            .store
            .read(|r| r.scan_prefix(MEMPOOL, &k_chain(c)))?
            .into_iter()
            .map(|(_, v)| (u64::from_le_bytes(v[..8].try_into().unwrap()), v[8..].to_vec()))
            .collect())
    }

    /// The pooled transaction spending an outpoint, if any.
    pub fn mempool_spender(&self, c: u16, op: &OutPoint) -> Result<Option<Hash256>> {
        self.store.read(|r| {
            Ok(r.get(MEMPOOL_SPENDS, &k_out(c, &op.txid.t12(), op.vout))?.map(|v| Hash256(v[..32].try_into().unwrap())))
        })
    }

    /// Drop pooled transactions older than `max_age` seconds.
    pub fn mempool_expire(&self, params: &ChainParams, now: u64, max_age: u64) -> Result<u32> {
        let c = params.chain_no;
        self.store.write(|w| {
            let mut n = 0;
            for (k, v) in w.scan_prefix(MEMPOOL, &k_chain(c))? {
                if now.saturating_sub(u64::from_le_bytes(v[..8].try_into().unwrap())) > max_age {
                    if let Ok(tx) = Tx::parse_with(&v[8..], &params.format) {
                        remove_pooled(w, c, &tx)?;
                    }
                    w.del(MEMPOOL, &k)?;
                    n += 1;
                }
            }
            Ok(n)
        })
    }

    // ---------- headers ----------

    /// Store headers (already checked by the caller) and move `best` if any has more work.
    pub fn put_headers(&self, c: u16, recs: &[(Hash256, HeaderRec)]) -> Result<()> {
        self.store.write(|w| {
            let mut best = w.get(STATE, &k_state(c, "best"))?.map(|v| U256::from_be_bytes(v[32..64].try_into().unwrap()));
            let mut best_hash = None;
            for (hash, rec) in recs {
                let k = k_header(c, &hash.0);
                if w.contains(HEADERS, &k)? {
                    continue;
                }
                w.put(HEADERS, &k, &rec.index_value())?;
                w.put(HDR, &k_hdr(c, rec.height, &hash.0), &rec.data_value())?;
                if best.map(|b| rec.work > b).unwrap_or(true) {
                    best = Some(rec.work);
                    best_hash = Some(*hash);
                }
            }
            if let (Some(h), Some(wk)) = (best_hash, best) {
                let mut v = h.0.to_vec();
                v.extend_from_slice(&wk.to_be_bytes());
                w.put(STATE, &k_state(c, "best"), &v)?;
            }
            Ok(())
        })
    }

    /// Work of a child of `parent` with `bits`.
    pub fn child_work(parent: &HeaderRec, bits: u32) -> U256 {
        parent.work.checked_add(work_from_bits(bits)).unwrap_or(U256::MAX)
    }

    // ---------- blocks ----------

    /// Connect blocks on top of the tip, in one transaction (all or none).
    /// The first block's parent must be the current tip (or there is no tip: genesis).
    pub fn connect_blocks(&self, params: &ChainParams, blocks: &[Block]) -> Result<Tip> {
        let c = params.chain_no;
        self.store.write(|w| {
            let mut tip = w.get(STATE, &k_state(c, "tip"))?.map(|v| decode_tip(&v));
            for b in blocks {
                tip = Some(connect_one(w, params, b, tip)?);
            }
            tip.ok_or_else(|| other("nothing connected"))
        })
    }

    /// Disconnect the tip block using its undo record.
    pub fn disconnect_tip(&self, c: u16) -> Result<Tip> {
        self.store.write(|w| {
            let tip = w.get(STATE, &k_state(c, "tip"))?.map(|v| decode_tip(&v)).ok_or_else(|| other("no tip"))?;
            if tip.height == 0 {
                return Err(other("refusing to disconnect genesis"));
            }
            let rec = w.get(UNDO, &k_block(c, tip.b))?.ok_or_else(|| {
                other(format!("no undo record for height {} (reorg deeper than {UNDO_DEPTH})", tip.height))
            })?;
            undo::apply_undo(w, &undo::decode(&rec)?)?;
            w.del(UNDO, &k_block(c, tip.b))?;
            let new = w.get(STATE, &k_state(c, "tip"))?.map(|v| decode_tip(&v)).ok_or_else(|| other("undo left no tip"))?;
            Ok(new)
        })
    }
}

fn encode_tip(t: &Tip) -> Vec<u8> {
    let mut v = t.hash.0.to_vec();
    v.extend_from_slice(&t.height.to_be_bytes());
    v.extend_from_slice(&t.b.to_be_bytes());
    v
}

fn decode_tip(v: &[u8]) -> Tip {
    Tip { hash: Hash256(v[..32].try_into().unwrap()), height: be_u32(v, 32), b: be_u32(v, 36) }
}

fn next_b(w: &WriteTxn, c: u16) -> Result<u32> {
    let k = k_state(c, "next_b");
    let n = w.get(STATE, &k)?.map(|v| be_u32(&v, 0)).unwrap_or(1);
    w.put(STATE, &k, &(n + 1).to_be_bytes())?;
    Ok(n)
}

fn connect_one(w: &WriteTxn, params: &ChainParams, blk: &Block, tip: Option<Tip>) -> Result<Tip> {
    let c = params.chain_no;
    let hash = blk.hash();
    let height = match tip {
        Some(t) => {
            if blk.header.prev != t.hash {
                return Err(other(format!("block {hash} does not extend tip {}", t.hash)));
            }
            t.height + 1
        }
        None => {
            if hash != params.genesis_hash() {
                return Err(other("first block must be genesis"));
            }
            0
        }
    };
    blk.check().map_err(other)?;

    // The block's bytes: stored once, kept even if later disconnected (forks included).
    let mut hrec = match header_rec(w, c, &hash)? {
        Some(r) => r,
        None => {
            let work = match tip {
                Some(t) => header_rec(w, c, &t.hash)?
                    .map(|p| ChainDb::child_work(&p, blk.header.bits))
                    .unwrap_or_else(|| work_from_bits(blk.header.bits)),
                None => work_from_bits(blk.header.bits),
            };
            let r = HeaderRec { b: 0, height, status: STATUS_HEADER, work, header: blk.header, auxpow: Vec::new() };
            w.put(HDR, &k_hdr(c, height, &hash.0), &r.data_value())?;
            r
        }
    };
    if hrec.b == 0 {
        hrec.b = next_b(w, c)?;
        w.put(BLOCK_META, &k_block(c, hrec.b), &block_meta(blk))?;
        for (p, tx) in blk.txs.iter().enumerate() {
            w.put(BLOCK_TX, &k_block_tx(c, hrec.b, p as u32), &tx.raw)?;
        }
    }
    let b = hrec.b;

    // Index changes, undo-logged.
    let l = Logged::new(w);
    for (p, tx) in blk.txs.iter().enumerate() {
        let p = p as u32;
        let t12 = tx.txid.t12();
        settle_mempool(w, params, tx)?;
        let mut bp = b.to_be_bytes().to_vec();
        bp.extend_from_slice(&p.to_be_bytes());
        l.dup_add(TXS, &k_txs(c, &t12), &bp)?;

        if !tx.is_coinbase() {
            for (vin, inp) in tx.inputs.iter().enumerate() {
                let missing = || {
                    other(format!(
                        "height {height}: tx {} spends unknown coin {}:{}",
                        tx.txid, inp.prevout.txid, inp.prevout.vout
                    ))
                };
                let (pb, pp) = locate(w, params, &inp.prevout.txid)?.ok_or_else(missing)?;
                let coin = coin_at(w, params, pb, pp, inp.prevout.vout)?.ok_or_else(missing)?;
                let sid = script_id(&coin.script);
                let ok = k_pos(c, pb, pp, inp.prevout.vout);
                l.del(OUTPUTS, &ok)?;
                l.del(BY_SCRIPT, &k_by_script(c, &sid, pb, pp, inp.prevout.vout))?;
                let mut sp = bp.clone();
                sp.extend_from_slice(&(vin as u32).to_be_bytes());
                l.put(SPENDS, &ok, &sp)?;
                add_hist(&l, c, &sid, height, &t12, 0, coin.value)?;
            }
        }
        // Block 0's outputs are never coins (Bitcoin-family rule).
        if height == 0 {
            continue;
        }
        let coinbase = tx.is_coinbase() || tx.is_coinstake();
        for (vout, out) in tx.outputs.iter().enumerate() {
            if is_unspendable(out.value, &out.script_pubkey) {
                continue;
            }
            let vout = vout as u32;
            let sid = script_id(&out.script_pubkey);
            l.put(OUTPUTS, &k_pos(c, b, p, vout), &encode_coin(out.value, height, coinbase, &out.script_pubkey))?;
            let mut v = out.value.to_le_bytes().to_vec();
            v.extend_from_slice(&height.to_le_bytes());
            l.put(BY_SCRIPT, &k_by_script(c, &sid, b, p, vout), &v)?;
            add_hist(&l, c, &sid, height, &t12, out.value, 0)?;
        }
    }

    hrec.status = STATUS_MAIN;
    hrec.height = height;
    l.put(HEADERS, &k_header(c, &hash.0), &hrec.index_value())?;
    let mut hv = hash.0.to_vec();
    hv.extend_from_slice(&b.to_be_bytes());
    l.put(HEIGHT, &k_height(c, height), &hv)?;
    let new_tip = Tip { hash, height, b };
    l.put(STATE, &k_state(c, "tip"), &encode_tip(&new_tip))?;

    let ops = l.into_ops();
    if height > 0 {
        w.put(UNDO, &k_block(c, b), &undo::encode(&ops))?;
    }
    // Drop the undo record that just left the window.
    if height > UNDO_DEPTH {
        if let Some(v) = w.get(HEIGHT, &k_height(c, height - UNDO_DEPTH - 1))? {
            w.del(UNDO, &k_block(c, be_u32(&v, 32)))?;
        }
    }
    Ok(new_tip)
}

fn remove_pooled(w: &WriteTxn, c: u16, tx: &Tx) -> Result<()> {
    w.del(MEMPOOL, &k_txs(c, &tx.txid.t12()))?;
    for i in &tx.inputs {
        let k = k_out(c, &i.prevout.txid.t12(), i.prevout.vout);
        if w.get(MEMPOOL_SPENDS, &k)?.as_deref() == Some(&tx.txid.0[..]) {
            w.del(MEMPOOL_SPENDS, &k)?;
        }
    }
    Ok(())
}

/// A block confirmed `tx`: drop it from the pool, and drop pooled txs that spend the
/// same coins (they can never confirm now).
fn settle_mempool(w: &WriteTxn, params: &ChainParams, tx: &Tx) -> Result<()> {
    let c = params.chain_no;
    if w.contains(MEMPOOL, &k_txs(c, &tx.txid.t12()))? {
        remove_pooled(w, c, tx)?;
    }
    for i in &tx.inputs {
        let k = k_out(c, &i.prevout.txid.t12(), i.prevout.vout);
        if let Some(other) = w.get(MEMPOOL_SPENDS, &k)? {
            let ot12: [u8; 12] = other[..12].try_into().unwrap();
            if let Some(v) = w.get(MEMPOOL, &k_txs(c, &ot12))? {
                if let Ok(otx) = Tx::parse_with(&v[8..], &params.format) {
                    remove_pooled(w, c, &otx)?;
                }
            }
            w.del(MEMPOOL_SPENDS, &k)?;
        }
    }
    Ok(())
}

fn add_hist(l: &Logged, c: u16, sid: &[u8; 21], height: u32, t12: &[u8; 12], recv: u64, spent: u64) -> Result<()> {
    let k = k_hist(c, sid, height, t12);
    let (r0, s0) = match l.get(SCRIPT_HIST, &k)? {
        Some(v) => (be_u64(&v, 0), be_u64(&v, 8)),
        None => (0, 0),
    };
    let mut v = (r0 + recv).to_be_bytes().to_vec();
    v.extend_from_slice(&(s0 + spent).to_be_bytes());
    l.put(SCRIPT_HIST, &k, &v)
}

#[cfg(test)]
mod compress_tests {
    use super::*;

    #[test]
    fn scripts_round_trip() {
        let p2pkh = [&[0x76, 0xa9, 0x14][..], &[1; 20], &[0x88, 0xac]].concat();
        let p2sh = [&[0xa9, 0x14][..], &[2; 20], &[0x87]].concat();
        let p2pkc = [&[33, 0x02][..], &[3; 32], &[0xac]].concat();
        let p2pku = [&[65, 0x04][..], &[4; 64], &[0xac]].concat();
        for s in [&p2pkh, &p2sh, &p2pkc, &p2pku] {
            let c = compress_script(s);
            assert!(c.len() < s.len());
            assert_eq!(&expand_script(&c, || unreachable!()).unwrap(), s);
        }
        let ms = vec![0x51, 0x21, 9, 9, 0xae];
        assert_eq!(compress_script(&ms), vec![SCRIPT_IN_TX]);
        assert_eq!(expand_script(&[SCRIPT_IN_TX], || Ok(ms.clone())).unwrap(), ms);
    }
}
