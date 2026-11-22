//! Staked blocks checked as they connect (a chain with [`ChainParams::staked_from`]: Kotia after
//! its last proof-of-work height). The rules are arcade_chain::kotia_pos (Blackcoin kernel v3,
//! ported from Kotia Core v2.4); the staker's self-check (arcaded staker.rs) was their only user
//! before, on the newest 200 blocks.
//!
//! - Every connected block gets a row in [`STAKE`] (by its local block number, so a fork's
//!   blocks have their own): the stake modifier after it, and a mark: [`MARK_OK`] its stake
//!   was checked and holds, [`MARK_BAD`] it broke a stake rule while the checks were not yet
//!   enforced, [`MARK_NONE`] nothing to check (a mined block) or not checkable here (the staked
//!   coin's transaction is pruned and not held sealed). A row depends only on the block and the
//!   blocks under it, so it is right on any branch; rows written while connecting go through
//!   the undo log and leave with the block on a reorg.
//! - A block's row needs its parent's. A chain synced from block 0 has them all; one synced
//!   before this was built gets them from a one-time pass over the stored blocks
//!   ([`ChainDb::stake_step`], resumable: it keeps how far it got in STATE "stake:pass"), which
//!   may start from a modifier this arcade worked out before (the staker's checkpoints).
//! - The checks are enforced (a block that breaks one is invalid, like any broken rule) only
//!   once the pass has reached the tip and none of the newest [`GATE`] staked blocks broke a
//!   rule (STATE "stake:on"). So a rule that doesn't match the chain can never throw out a
//!   chain synced before: it stays off, and swaps on the chain are refused, saying why.
//!   What the checks found is in [`StakeStatus`].
//! - Checked, the rules are the network's, with two left out: the clock (a block at most 15 s
//!   ahead of now: it depends on when a block is seen, so it can't decide what is stored) and
//!   the coinbase's shape. Where unsure, the check is a little looser than Kotia Core, never
//!   stricter (a block Kotia takes must never be refused here): a staked coin needs 19 blocks
//!   on top here where the network asks 20.

use crate::db::{decode_tip, header_rec, ChainDb, Rd, Tip, STOPPING};
use crate::schema::*;
use arcade_chain::kotia_pos as pos;
use arcade_chain::params::ChainParams;
use arcade_chain::{Block, Hash256, Header, Tx};
use arcade_store::key::be_u32;
use arcade_store::{Result, StoreError, WriteTxn};
use secp256k1::{ecdsa::Signature, Message, PublicKey, SECP256K1};

/// Nothing checked: a mined block, or one whose stake can't be checked here.
pub const MARK_NONE: u8 = 0;
/// Its stake checked and holds.
pub const MARK_OK: u8 = 1;
/// It broke a stake rule (kept: the checks were not enforced yet).
pub const MARK_BAD: u8 = 2;
/// Staked blocks under the tip that must all hold before the checks are enforced (about two
/// weeks of Kotia).
pub const GATE: u32 = 10_000;
pub(crate) const ON: &str = "stake:on";
pub(crate) const PASS: &str = "stake:pass";

/// One block's stake row: the modifier after it and its mark.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    pub modifier: Hash256,
    pub mark: u8,
}

pub(crate) fn encode(r: &Row) -> Vec<u8> {
    let mut v = r.modifier.0.to_vec();
    v.push(r.mark);
    v
}

fn decode(v: &[u8]) -> Option<Row> {
    (v.len() >= 33).then(|| Row { modifier: Hash256(v[..32].try_into().unwrap()), mark: v[32] })
}

pub(crate) fn row_of<R: Rd>(r: &R, c: u16, b: u32) -> Result<Option<Row>> {
    Ok(r.get(STAKE, &k_block(c, b))?.as_deref().and_then(decode))
}

/// The staked coin of a block's coinstake (its first input), as the kernel needs it.
#[derive(Clone, Copy, Debug)]
pub struct Staked {
    pub value: u64,
    /// Height of the block holding the coin.
    pub height: u32,
    /// The time field of the coin's transaction.
    pub tx_time: u32,
}

/// What checking one block's stake found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ok,
    Bad(String),
    /// Not checkable here (what's missing).
    Unknown(String),
}

/// The kernel a block adds to the modifier: its coinstake's first input txid when staked, else
/// its own id.
pub fn kernel_of(b: &Block) -> Hash256 {
    if b.is_proof_of_stake() { b.txs[1].inputs[0].prevout.txid } else { b.hash() }
}

/// A staked block's signature: the block id signed by the key in coinstake output 1 (DER read
/// laxly and the S made low, as Kotia's CPubKey::Verify does).
pub fn block_sig_ok(b: &Block) -> bool {
    let Some(key) = b.txs.get(1).and_then(|cs| cs.outputs.get(1)).and_then(|o| pos::block_signing_key(&o.script_pubkey)) else { return false };
    let (Ok(pk), Ok(mut s)) = (PublicKey::from_slice(key), Signature::from_der_lax(b.sig.as_deref().unwrap_or(&[]))) else { return false };
    s.normalize_s();
    SECP256K1.verify_ecdsa(&Message::from_digest(b.id.0), &s, &pk).is_ok()
}

/// Check a staked block at `height` on top of `prev` (and `prev2` under it, None when that one
/// was mined or is missing: the retarget is then not checked) with the modifier after `prev`.
pub fn check(b: &Block, height: u32, prev_modifier: &Hash256, prev: &Header, prev2: Option<&Header>, staked: Option<Staked>, from: u32) -> Verdict {
    if !b.is_proof_of_stake() {
        return Verdict::Bad("a mined block after the chain's last proof-of-work height".into());
    }
    let cs = &b.txs[1];
    let t = b.header.time;
    if cs.time != Some(t) {
        return Verdict::Bad("its time differs from its coinstake's".into());
    }
    if t & pos::STAKE_TIMESTAMP_MASK != 0 {
        return Verdict::Bad("its time is not a multiple of 16".into());
    }
    if t <= prev.time {
        return Verdict::Bad("its time is not after the previous block's".into());
    }
    // Retarget from the last two staked blocks (the first two staked heights have mined blocks there).
    if let Some(z) = prev2.filter(|_| height >= from.saturating_add(2)) {
        let want = pos::next_bits(prev.bits, prev.time, z.time);
        if b.header.bits != want {
            return Verdict::Bad(format!("bits {:08x}, the retarget gives {want:08x}", b.header.bits));
        }
    }
    if !block_sig_ok(b) {
        return Verdict::Bad("its signature is not the coinstake key's".into());
    }
    let Some(s) = staked else { return Verdict::Unknown("the staked coin's transaction is not held here".into()) };
    if height.saturating_sub(s.height) < pos::COINBASE_MATURITY - 1 {
        return Verdict::Bad(format!("it stakes a coin from block {} (too young)", s.height));
    }
    let i0 = &cs.inputs[0];
    if !pos::check_kernel(prev_modifier, b.header.bits, s.tx_time, &i0.prevout.txid, i0.prevout.vout, s.value, t) {
        return Verdict::Bad("its stake kernel misses the target".into());
    }
    Verdict::Ok
}

/// The time field of a stored transaction's raw bytes (None if pruned or not timed).
fn tx_time_of(raw: &[u8], params: &ChainParams) -> Option<u32> {
    if crate::db::is_pruned(raw) {
        return None;
    }
    Tx::parse_with(raw, &params.format).ok()?.time
}

fn is_on<R: Rd>(r: &R, c: u16) -> Result<bool> {
    Ok(r.get(STATE, &k_state(c, ON))?.is_some())
}

/// Connecting `b` (local number `bn`) at `height` on `tip`: the block's stake row, or None when
/// the parent has none yet (the pass fills it in). `staked`: the block number, position, value
/// and height of the coin its coinstake's first input spends. Err(InvalidBlock) when it breaks
/// a stake rule and the checks are enforced.
pub(crate) fn on_connect(w: &WriteTxn, params: &ChainParams, b: &Block, height: u32, tip: Option<Tip>, staked: Option<(u32, u32, u64, u32)>) -> Result<Option<Row>> {
    let c = params.chain_no;
    let Some(from) = params.staked_from() else { return Ok(None) };
    let Some(t) = tip else { return Ok(Some(Row { modifier: Hash256::ZERO, mark: MARK_NONE })) };
    let Some(prow) = row_of(w, c, t.b)? else { return Ok(None) };
    let modifier = pos::next_modifier(&prow.modifier, &kernel_of(b));
    if height < from {
        return Ok(Some(Row { modifier, mark: MARK_NONE }));
    }
    let prev = header_rec(w, c, &t.hash)?.ok_or(StoreError::Corrupt { table: HDR, why: "tip header missing" })?;
    let prev2 = match w.get(HEIGHT, &k_height(c, height.saturating_sub(2)))? {
        Some(v) if height >= 2 => header_rec(w, c, &Hash256(v[..32].try_into().unwrap()))?.map(|r| r.header),
        _ => None,
    };
    let staked = match staked {
        Some((pb, pp, value, coin_height)) => w.get(BLOCK_TX, &k_block_tx(c, pb, pp))?.and_then(|raw| tx_time_of(&raw, params)).map(|tx_time| Staked { value, height: coin_height, tx_time }),
        None => None,
    };
    let mark = match check(b, height, &prow.modifier, &prev.header, prev2.as_ref(), staked, from) {
        Verdict::Ok => MARK_OK,
        Verdict::Unknown(_) => MARK_NONE,
        Verdict::Bad(why) => {
            if is_on(w, c)? {
                return Err(StoreError::InvalidBlock { block: b.hash().0, why: format!("height {height}: stake: {why}") });
            }
            MARK_BAD
        }
    };
    Ok(Some(Row { modifier, mark }))
}

pub(crate) fn put_row(l: &crate::undo::Logged, c: u16, bn: u32, r: &Row) -> Result<()> {
    l.put(STAKE, &k_block(c, bn), &encode(r))
}

/// Where the checks stand on a chain ([`ChainDb::stake_status`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StakeStatus {
    /// Enforced: a block breaking a stake rule is invalid, and checked blocks confirm.
    pub on: bool,
    /// The one-time pass: the height it has reached (None before it starts).
    pub at: Option<u32>,
    pub tip: u32,
    /// Staked blocks the pass found breaking a rule, and the newest of them.
    pub bad: u32,
    pub last_bad: Option<u32>,
    /// Why the checks are not on yet, in a few words (None when on).
    pub why: Option<String>,
}

/// The pass's place: (height, block id, bad blocks found, newest bad height + 1 (0 = none)).
fn read_pass<R: Rd>(r: &R, c: u16) -> Result<Option<(u32, Hash256, u32, u32)>> {
    Ok(r.get(STATE, &k_state(c, PASS))?.filter(|v| v.len() >= 44).map(|v| (be_u32(&v, 0), Hash256(v[4..36].try_into().unwrap()), be_u32(&v, 36), be_u32(&v, 40))))
}

fn pass_value(height: u32, hash: &Hash256, bad: u32, last_bad: u32) -> Vec<u8> {
    let mut v = height.to_be_bytes().to_vec();
    v.extend_from_slice(&hash.0);
    v.extend_from_slice(&bad.to_be_bytes());
    v.extend_from_slice(&last_bad.to_be_bytes());
    v
}

impl ChainDb {
    /// The stake row of local block `b`.
    pub fn stake_row(&self, c: u16, b: u32) -> Result<Option<Row>> {
        self.store(c).read(|r| row_of(r, c, b))
    }

    /// The stake row of the main chain's block at `height`.
    pub fn stake_row_at(&self, c: u16, height: u32) -> Result<Option<Row>> {
        self.store(c).read(|r| {
            let Some(v) = r.get(HEIGHT, &k_height(c, height))? else { return Ok(None) };
            row_of(r, c, be_u32(&v, 32))
        })
    }

    /// The modifier after the main chain's block at `height` (the staker stakes on the tip's).
    pub fn stake_modifier_at(&self, c: u16, height: u32) -> Result<Option<Hash256>> {
        Ok(self.stake_row_at(c, height)?.map(|r| r.modifier))
    }

    /// Whether stake checks are enforced on this chain (false on a chain that isn't staked).
    pub fn stake_on(&self, params: &ChainParams) -> Result<bool> {
        if params.staked_from().is_none() {
            return Ok(false);
        }
        let c = params.chain_no;
        self.store(c).read(|r| is_on(r, c))
    }

    pub fn stake_status(&self, params: &ChainParams) -> Result<StakeStatus> {
        let c = params.chain_no;
        let tip = self.tip(c)?.map(|t| t.height).unwrap_or(0);
        let (on, pass) = self.store(c).read(|r| Ok((is_on(r, c)?, read_pass(r, c)?)))?;
        let mut s = StakeStatus { on, at: pass.map(|p| p.0), tip, bad: pass.map(|p| p.2).unwrap_or(0), last_bad: pass.and_then(|p| p.3.checked_sub(1)), why: None };
        if !on {
            s.why = Some(match s.at {
                None => "its staked blocks are not checked yet".into(),
                Some(a) if a < tip => format!("checking its staked blocks: block {a} of {tip}"),
                Some(_) => match s.last_bad {
                    Some(h) if h + GATE > tip => format!("block {h} broke a stake rule as checked here; the checks wait for {GATE} good blocks after it"),
                    _ => "too few of its newest staked blocks can be checked here".into(),
                },
            });
        }
        Ok(s)
    }

    /// Start the pass at the main chain's block `height` (id `hash`) with the modifier after it,
    /// worked out before by this arcade (the staker's checkpoints). Only when the pass hasn't
    /// started and the block is on the main chain; true if taken.
    pub fn stake_seed(&self, params: &ChainParams, height: u32, hash: &Hash256, modifier: &Hash256) -> Result<bool> {
        let c = params.chain_no;
        if params.staked_from().is_none_or(|f| height >= f) {
            return Ok(false); // seeds only below the staked stretch: every staked block is checked
        }
        self.store(c).write(|w| {
            if read_pass(w, c)?.is_some() {
                return Ok(false);
            }
            let Some(v) = w.get(HEIGHT, &k_height(c, height))? else { return Ok(false) };
            if v[..32] != hash.0 {
                return Ok(false);
            }
            let bn = be_u32(&v, 32);
            if row_of(w, c, bn)?.is_none() {
                w.put(STAKE, &k_block(c, bn), &encode(&Row { modifier: *modifier, mark: MARK_NONE }))?;
            }
            w.put(STATE, &k_state(c, PASS), &pass_value(height, hash, 0, 0))?;
            Ok(true)
        })
    }

    /// One step of the one-time pass over the stored chain: up to `max` blocks or `budget` of
    /// reading, then one short write. Each block's row from its parent's (rows already there,
    /// written as blocks connected, are taken as they are). At the tip, decides whether the
    /// checks go on. Returns where things stand.
    pub fn stake_step(&self, params: &ChainParams, max: u32, budget: std::time::Duration) -> Result<StakeStatus> {
        let c = params.chain_no;
        let Some(from) = params.staked_from() else { return self.stake_status(params) };
        let Some(tip) = self.tip(c)? else { return self.stake_status(params) };
        // Where the pass is; back down to the main chain after a reorg.
        let mut pass = self.store(c).read(|r| read_pass(r, c))?;
        if let Some((h, hash, bad, lb)) = pass {
            if self.hash_at(c, h)? != Some(hash) {
                let mut at = h.min(tip.height);
                let mut back = None;
                for _ in 0..=crate::db::UNDO_DEPTH + 1 {
                    if let Some(hh) = self.hash_at(c, at)? {
                        let bn = self.header(c, &hh)?.map(|r| r.b).unwrap_or(0);
                        if bn != 0 && self.stake_row(c, bn)?.is_some() {
                            back = Some((at, hh));
                            break;
                        }
                    }
                    if at == 0 {
                        break;
                    }
                    at -= 1;
                }
                pass = back.map(|(a, hh)| (a, hh, bad, lb.min(a + 1)));
                self.store(c).write(|w| match pass {
                    Some((a, hh, b, l)) => w.put(STATE, &k_state(c, PASS), &pass_value(a, &hh, b, l)),
                    None => w.del(STATE, &k_state(c, PASS)).map(|_| ()),
                })?;
            }
        }
        // Not started: from block 0.
        let (mut at, mut at_hash, mut bad, mut last_bad) = match pass {
            Some(p) => p,
            None => {
                let g = self.hash_at(c, 0)?.ok_or_else(|| StoreError::Other("no block 0".into()))?;
                let gb = self.header(c, &g)?.map(|r| r.b).unwrap_or(0);
                self.store(c).write(|w| {
                    if row_of(w, c, gb)?.is_none() {
                        w.put(STAKE, &k_block(c, gb), &encode(&Row { modifier: Hash256::ZERO, mark: MARK_NONE }))?;
                    }
                    w.put(STATE, &k_state(c, PASS), &pass_value(0, &g, 0, 0))
                })?;
                (0, g, 0, 0)
            }
        };
        let rec = self.header(c, &at_hash)?.ok_or_else(|| StoreError::Other("pass block missing".into()))?;
        let mut prev_row = self.stake_row(c, rec.b)?.ok_or_else(|| StoreError::Other("pass block has no stake row".into()))?;
        let mut prev_hdr = rec.header;
        let mut prev2_hdr: Option<Header> = None;
        let mut rows: Vec<(u32, Row)> = Vec::new();
        let t0 = std::time::Instant::now();
        let mut stuck = None;
        while at < tip.height && (rows.len() as u32) < max && t0.elapsed() < budget {
            let h = at + 1;
            let Some(hash) = self.hash_at(c, h)? else { break };
            let Some(hr) = self.header(c, &hash)? else { break };
            if hr.header.prev != at_hash {
                break; // the chain changed under us: next step
            }
            let row = match self.stake_row(c, hr.b)? {
                Some(r) => r,
                None => match self.stake_row_for(params, h, hr.b, &hash, &prev_row, &prev_hdr, prev2_hdr.as_ref(), from) {
                    Ok(r) => r,
                    Err(e) => {
                        stuck = Some(e);
                        break;
                    }
                },
            };
            if h >= from && row.mark == MARK_BAD {
                bad += 1;
                last_bad = h + 1;
            }
            rows.push((hr.b, row));
            prev2_hdr = Some(prev_hdr);
            prev_hdr = hr.header;
            prev_row = row;
            at = h;
            at_hash = hash;
        }
        if !rows.is_empty() && !STOPPING.load(std::sync::atomic::Ordering::SeqCst) {
            let wrote = self.store(c).write(|w| {
                // Only if the stretch is still the main chain (its top is: so is everything under it).
                if w.get(HEIGHT, &k_height(c, at))?.is_none_or(|v| v[..32] != at_hash.0) {
                    return Ok(false);
                }
                for (bn, r) in &rows {
                    if row_of(w, c, *bn)?.is_none() {
                        w.put(STAKE, &k_block(c, *bn), &encode(r))?;
                    }
                }
                w.put(STATE, &k_state(c, PASS), &pass_value(at, &at_hash, bad, last_bad))?;
                Ok(true)
            })?;
            if !wrote {
                return self.stake_status(params);
            }
        }
        if let Some(e) = stuck {
            return Err(e);
        }
        if at >= tip.height {
            self.stake_gate(params, from)?;
        }
        self.stake_status(params)
    }

    /// The row of the main chain's block at `height` (local number `bn`) from its parent's.
    #[allow(clippy::too_many_arguments)]
    fn stake_row_for(&self, params: &ChainParams, height: u32, bn: u32, hash: &Hash256, prev: &Row, prev_hdr: &Header, prev2_hdr: Option<&Header>, from: u32) -> Result<Row> {
        let c = params.chain_no;
        if height < from {
            // Mined: only the kernel (the coinstake's first input, if the block has one).
            let kernel = match self.tx_at(params, bn, 1)? {
                Some(t) if t.is_coinstake() => t.inputs[0].prevout.txid,
                _ => *hash,
            };
            return Ok(Row { modifier: pos::next_modifier(&prev.modifier, &kernel), mark: MARK_NONE });
        }
        let raw = self.block_raw(c, hash)?.ok_or_else(|| StoreError::Other(format!("block {height}'s body is not held here")))?;
        let b = Block::parse_with(&raw, &params.format).map_err(|e| StoreError::Other(format!("block {height}: {e}")))?;
        let modifier = pos::next_modifier(&prev.modifier, &kernel_of(&b));
        let staked = if b.is_proof_of_stake() { self.staked_coin(params, &b)? } else { None };
        let mark = match check(&b, height, &prev.modifier, prev_hdr, prev2_hdr, staked, from) {
            Verdict::Ok => MARK_OK,
            Verdict::Unknown(_) => MARK_NONE,
            Verdict::Bad(_) => MARK_BAD,
        };
        Ok(Row { modifier, mark })
    }

    /// The coin a stored staked block's coinstake stakes, read from the block holding it.
    fn staked_coin(&self, params: &ChainParams, b: &Block) -> Result<Option<Staked>> {
        let op = b.txs[1].inputs[0].prevout;
        let Some((raw, bn)) = self.tx_raw(params, &op.txid)? else { return Ok(None) };
        let Ok(tx) = Tx::parse_with(&raw, &params.format) else { return Ok(None) };
        let (Some(out), Some(tx_time)) = (tx.outputs.get(op.vout as usize), tx.time) else { return Ok(None) };
        let Some(height) = self.block_height_of(params.chain_no, bn)? else { return Ok(None) };
        Ok(Some(Staked { value: out.value, height, tx_time }))
    }

    /// At the tip: turn the checks on when none of the newest GATE staked blocks broke a rule
    /// and most of them were checked.
    fn stake_gate(&self, params: &ChainParams, from: u32) -> Result<()> {
        let c = params.chain_no;
        if self.stake_on(params)? {
            return Ok(());
        }
        let Some(tip) = self.tip(c)? else { return Ok(()) };
        if tip.height < from {
            return Ok(()); // nothing staked yet: on once there is something to check
        }
        let lo = from.max(tip.height.saturating_sub(GATE - 1));
        let (mut ok, mut bad) = (0u32, 0u32);
        let mut last_bad = None;
        self.store(c).read(|r| {
            for h in lo..=tip.height {
                let Some(v) = r.get(HEIGHT, &k_height(c, h))? else { continue };
                match row_of(r, c, be_u32(&v, 32))?.map(|x| x.mark) {
                    Some(MARK_OK) => ok += 1,
                    Some(MARK_BAD) => {
                        bad += 1;
                        last_bad = Some(h);
                    }
                    _ => {}
                }
            }
            Ok(())
        })?;
        let n = tip.height - lo + 1;
        if bad == 0 && ok > 0 && ok * 2 >= n {
            self.store(c).write(|w| {
                // Still the same tip (else the next step decides again).
                if w.get(STATE, &k_state(c, "tip"))?.map(|v| decode_tip(&v).hash) == Some(tip.hash) {
                    w.put(STATE, &k_state(c, ON), b"1")?;
                }
                Ok(())
            })?;
        } else if let Some(h) = last_bad {
            // A bad block connected (checks off) after the pass went by: remembered for the status.
            self.store(c).write(|w| {
                if let Some((a, hh, b, lb)) = read_pass(w, c)? {
                    if lb < h + 1 {
                        w.put(STATE, &k_state(c, PASS), &pass_value(a, &hh, b.max(bad), h + 1))?;
                    }
                }
                Ok(())
            })?;
        }
        Ok(())
    }
}
