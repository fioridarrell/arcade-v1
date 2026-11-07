//! ChainDb: blocks stored once, indexes updated in the same commit, reorgs by undo.

use crate::schema::*;
use crate::undo::{self, Logged};
use arcade_chain::hash::sha256;
use arcade_chain::params::ChainParams;
use arcade_chain::pow::{work_from_bits, U256};
use arcade_chain::{Block, Hash256, Header, OutPoint, Tx};
use arcade_store::key::{be_u32, be_u64};
use arcade_store::{Result, Store, StoreError, WriteTxn};
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
    /// Raw AuxPoW bytes (merge-mined chains), empty otherwise.
    pub auxpow: Vec<u8>,
}

impl HeaderRec {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(4 + 4 + 1 + 32 + 80 + self.auxpow.len());
        v.extend_from_slice(&self.b.to_be_bytes());
        v.extend_from_slice(&self.height.to_be_bytes());
        v.push(self.status);
        v.extend_from_slice(&self.work.to_be_bytes());
        v.extend_from_slice(&self.header.encode());
        v.extend_from_slice(&self.auxpow);
        v
    }
    pub fn decode(v: &[u8]) -> Result<HeaderRec> {
        if v.len() < 121 {
            return Err(StoreError::Corrupt { table: HEADERS, why: "short header record" });
        }
        Ok(HeaderRec {
            b: be_u32(v, 0),
            height: be_u32(v, 4),
            status: v[8],
            work: U256::from_be_bytes(v[9..41].try_into().unwrap()),
            header: Header::parse(v[41..121].try_into().unwrap()),
            auxpow: v[121..].to_vec(),
        })
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

impl Coin {
    fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(13 + self.script.len());
        v.extend_from_slice(&self.value.to_le_bytes());
        v.extend_from_slice(&self.height.to_le_bytes());
        v.push(self.coinbase as u8);
        v.extend_from_slice(&self.script);
        v
    }
    fn decode(v: &[u8]) -> Result<Coin> {
        if v.len() < 13 {
            return Err(StoreError::Corrupt { table: OUTPUTS, why: "short coin" });
        }
        Ok(Coin {
            value: u64::from_le_bytes(v[0..8].try_into().unwrap()),
            height: u32::from_le_bytes(v[8..12].try_into().unwrap()),
            coinbase: v[12] != 0,
            script: v[13..].to_vec(),
        })
    }
}

/// Outputs that can never be spent are not coins.
pub fn is_unspendable(value: u64, script: &[u8]) -> bool {
    script.first() == Some(&0x6a) || (value == 0 && script.is_empty())
}

pub fn script_hash(script: &[u8]) -> [u8; 32] {
    sha256(script)
}

fn other(e: impl std::fmt::Display) -> StoreError {
    StoreError::Other(e.to_string())
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

pub struct ChainDb {
    pub store: Store,
}

impl ChainDb {
    pub fn open(path: impl AsRef<Path>) -> Result<ChainDb> {
        Ok(ChainDb { store: Store::open(path, &CHAIN_SCHEMA)? })
    }

    // ---------- reads ----------

    pub fn header(&self, c: u16, hash: &Hash256) -> Result<Option<HeaderRec>> {
        self.store.read(|r| r.get(HEADERS, &k_header(c, &hash.0))?.map(|v| HeaderRec::decode(&v)).transpose())
    }

    pub fn tip(&self, c: u16) -> Result<Option<Tip>> {
        self.store.read(|r| Ok(r.get(STATE, &k_state(c, "tip"))?.map(|v| decode_tip(&v))))
    }

    /// Best known header (most work), which may be ahead of the tip.
    pub fn best_header(&self, c: u16) -> Result<Option<Hash256>> {
        self.store.read(|r| {
            Ok(r.get(STATE, &k_state(c, "best"))?.map(|v| Hash256(v[..32].try_into().unwrap())))
        })
    }

    pub fn hash_at(&self, c: u16, height: u32) -> Result<Option<Hash256>> {
        self.store.read(|r| {
            Ok(r.get(HEIGHT, &k_height(c, height))?.map(|v| Hash256(v[..32].try_into().unwrap())))
        })
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

    /// Raw transaction by txid (main chain preferred), checked by full hash.
    pub fn tx_raw(&self, params: &ChainParams, txid: &Hash256) -> Result<Option<(Vec<u8>, u32)>> {
        let c = params.chain_no;
        self.store.read(|r| {
            let mut found = None;
            for (_, bp) in r.scan_prefix(TXS, &k_txs(c, &txid.t12()))? {
                let (b, p) = (be_u32(&bp, 0), be_u32(&bp, 4));
                let Some(raw) = r.get(BLOCK_TX, &k_block_tx(c, b, p))? else { continue };
                let tx = Tx::parse_with(&raw, &params.format).map_err(other)?;
                if tx.txid == *txid {
                    found = Some((raw, b));
                    break;
                }
            }
            Ok(found)
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
    pub fn coin(&self, c: u16, op: &OutPoint) -> Result<Option<Coin>> {
        self.store
            .read(|r| r.get(OUTPUTS, &k_out(c, &op.txid.t12(), op.vout))?.map(|v| Coin::decode(&v)).transpose())
    }

    /// Every unspent coin paying `script`: (t12, vout, value, height).
    pub fn coins_for_script(&self, c: u16, script: &[u8]) -> Result<Vec<([u8; 12], u32, u64, u32)>> {
        let sh = script_hash(script);
        self.store.read(|r| {
            let pre = k_script(c, &sh);
            Ok(r.scan_prefix(BY_SCRIPT, &pre)?
                .into_iter()
                .map(|(k, v)| {
                    let n = pre.len();
                    (
                        k[n..n + 12].try_into().unwrap(),
                        be_u32(&k, n + 12),
                        u64::from_le_bytes(v[0..8].try_into().unwrap()),
                        u32::from_le_bytes(v[8..12].try_into().unwrap()),
                    )
                })
                .collect())
        })
    }

    pub fn balance_for_script(&self, c: u16, script: &[u8]) -> Result<u64> {
        Ok(self.coins_for_script(c, script)?.iter().map(|x| x.2).sum())
    }

    /// Who spent an outpoint: (block number, position, input index).
    pub fn spender(&self, c: u16, op: &OutPoint) -> Result<Option<(u32, u32, u32)>> {
        self.store.read(|r| {
            Ok(r.get(SPENDS, &k_out(c, &op.txid.t12(), op.vout))?
                .map(|v| (be_u32(&v, 0), be_u32(&v, 4), be_u32(&v, 8))))
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
                w.put(HEADERS, &k, &rec.encode())?;
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
    let hk = k_header(c, &hash.0);
    let mut hrec = match w.get(HEADERS, &hk)? {
        Some(v) => HeaderRec::decode(&v)?,
        None => HeaderRec {
            b: 0,
            height,
            status: STATUS_HEADER,
            work: match tip {
                Some(t) => w
                    .get(HEADERS, &k_header(c, &t.hash.0))?
                    .map(|v| HeaderRec::decode(&v))
                    .transpose()?
                    .map(|p| ChainDb::child_work(&p, blk.header.bits))
                    .unwrap_or_else(|| work_from_bits(blk.header.bits)),
                None => work_from_bits(blk.header.bits),
            },
            header: blk.header,
            auxpow: blk.auxpow.as_ref().map(|a| a.raw.clone()).unwrap_or_default(),
        },
    };
    let new_block = hrec.b == 0;
    if new_block {
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
        let t12 = tx.txid.t12();
        let mut bp = b.to_be_bytes().to_vec();
        bp.extend_from_slice(&(p as u32).to_be_bytes());
        l.dup_add(TXS, &k_txs(c, &t12), &bp)?;

        if !tx.is_coinbase() {
            for (vin, inp) in tx.inputs.iter().enumerate() {
                let pt12 = inp.prevout.txid.t12();
                let ok = k_out(c, &pt12, inp.prevout.vout);
                let coin = l.get(OUTPUTS, &ok)?.ok_or_else(|| {
                    other(format!(
                        "height {height}: tx {} spends unknown coin {}:{}",
                        tx.txid, inp.prevout.txid, inp.prevout.vout
                    ))
                })?;
                let coin = Coin::decode(&coin)?;
                let sh = script_hash(&coin.script);
                l.del(OUTPUTS, &ok)?;
                l.del(BY_SCRIPT, &k_by_script(c, &sh, &pt12, inp.prevout.vout))?;
                let mut sp = bp.clone();
                sp.extend_from_slice(&(vin as u32).to_be_bytes());
                l.put(SPENDS, &ok, &sp)?;
                add_hist(&l, c, &sh, height, &t12, 0, coin.value)?;
            }
        }
        // Block 0's outputs are never coins (Bitcoin-family rule).
        if height == 0 {
            continue;
        }
        for (vout, out) in tx.outputs.iter().enumerate() {
            if is_unspendable(out.value, &out.script_pubkey) {
                continue;
            }
            let coin = Coin {
                value: out.value,
                height,
                coinbase: tx.is_coinbase() || tx.is_coinstake(),
                script: out.script_pubkey.clone(),
            };
            let sh = script_hash(&out.script_pubkey);
            l.put(OUTPUTS, &k_out(c, &t12, vout as u32), &coin.encode())?;
            let mut v = out.value.to_le_bytes().to_vec();
            v.extend_from_slice(&height.to_le_bytes());
            l.put(BY_SCRIPT, &k_by_script(c, &sh, &t12, vout as u32), &v)?;
            add_hist(&l, c, &sh, height, &t12, out.value, 0)?;
        }
    }

    hrec.status = STATUS_MAIN;
    hrec.height = height;
    l.put(HEADERS, &hk, &hrec.encode())?;
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

fn add_hist(l: &Logged, c: u16, sh: &[u8; 32], height: u32, t12: &[u8; 12], recv: u64, spent: u64) -> Result<()> {
    let k = k_hist(c, sh, height, t12);
    let (r0, s0) = match l.get(SCRIPT_HIST, &k)? {
        Some(v) => (be_u64(&v, 0), be_u64(&v, 8)),
        None => (0, 0),
    };
    let mut v = (r0 + recv).to_be_bytes().to_vec();
    v.extend_from_slice(&(s0 + spent).to_be_bytes());
    l.put(SCRIPT_HIST, &k, &v)
}
