//! Swaps of any leg for any leg, across chains or on one, by hash-and-time locks (docs/MARKET.md steps 5 and 7),
//! V1.0 with V1.0. A leg is {chain, kind, asset, amount}:
//!
//!   coin     a chain's own coin, in satoshis           P2SH lock (htlc.rs), the amount plus the claim's fee
//!   dd       DigiDollars on dgb / dgbtest, in cents    Taproot script-only lock and a DGB reserve (ddlock.rs)
//!   token20  a DRC-20 / PRC-20 tick, whole tokens      note 1 on the locker, moved to the lock with note 2's
//!                                                      commit, note 2 revealed onto the lock (token20_swap.py)
//!   token    an arcade ledger property, in units       a ledger send to the lock's P2SH address; the claim
//!                                                      spends the lock's coin with a send to the claimer
//!   ord      one inscription (asset = its id)          its coin moved whole to the lock, a reserve beside it
//!
//! The taker makes the secret and locks its leg first, for longer; the maker's arcade checks that lock on its own
//! copy of the chain and locks its leg; the taker's arcade checks the maker's lock the same way and claims it, which
//! puts the secret on that chain; the maker's arcade reads it there and claims the taker's lock. Each refunds after
//! its own time. Nothing is anybody else's to sign, and a secret is believed only when its SHA-256 is the hash.
//!
//! ARCADE_SWAP_TEST_SECS (tests only): the maker's lock time in seconds (the taker's twice that), so refunds can be
//! tried without waiting hours.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use arcade_chain::{Hash256, OutPoint, Tx};
use arcade_ledger::engine as led;
use arcade_wallet::ddlock::{self, Lock};
use arcade_wallet::htlc::{self, Htlc};
use arcade_wallet::secp256k1::{PublicKey, XOnlyPublicKey, SECP256K1};
use arcade_wallet::tx::{TxOutSpec, Utxo};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;

/// The most a maker fee may be: 5% (the website's Marketplace tab allows 0-5%).
const MAX_FEE_BPS: u64 = 500;
/// A note's coin and an inscription lock's reserve (token20_send.POSTAGE, htlc.DOGINAL_RESERVE).
pub(crate) const POSTAGE: u64 = 100_000;
const RESERVE: u64 = htlc::SPEND_FEE + htlc::DUST;
/// What each inscribing step leaves the network (soft-dust outputs pay their own 0.01).
pub(crate) const STEP_FEE: u64 = 3_000_000;
/// A ledger token lock's coin: the claim's fee and the receiver's output.
const LEDGER_COIN: u64 = htlc::SPEND_FEE + 2 * htlc::DUST;
pub(crate) const CTYPE: &str = "text/plain;charset=utf-8";
const KINDS: &[&str] = &["coin", "dd", "token20", "token", "ord"];
/// How long an order is held for the taker who asked for its terms, to lock its side and say so. Asking again does
/// not hold it longer, and a taker whose hold ran out waits as long again before that arcade may hold the order again
/// (others get their turn).
const HOLD_SECS: u32 = 30 * 60;
/// Open orders here one asking arcade may hold at once, with every taker it carries (a website's visitors).
const HOLDS_PER_PEER: usize = 3;

fn maker_secs() -> u32 {
    std::env::var("ARCADE_SWAP_TEST_SECS").ok().and_then(|v| v.parse().ok()).unwrap_or(4 * 3600)
}
fn now() -> u32 {
    arcade_app::now() as u32
}
fn key(kind: &str, id: &str) -> Vec<u8> {
    format!("{kind}:{id}").into_bytes()
}
fn get(node: &Node, kind: &str, id: &str) -> Option<Value> {
    node.app.store.read(|r| r.get(arcade_app::META, &key(kind, id))).ok().flatten().and_then(|v| serde_json::from_slice(&v).ok())
}
fn put(node: &Node, kind: &str, v: &Value) {
    let _ = node.app.store.write(|w| w.put(arcade_app::META, &key(kind, v["id"].as_str().unwrap_or("")), v.to_string().as_bytes()));
}
fn list(node: &Node, kind: &str) -> Vec<Value> {
    node.app.store.read(|r| r.scan_prefix(arcade_app::META, format!("{kind}:").as_bytes())).unwrap_or_default().into_iter().filter_map(|(_, v)| serde_json::from_slice(&v).ok()).collect()
}
fn chain(node: &Node, k: &str) -> Result<&'static ChainParams, String> {
    let p = crate::chains::find(k).ok_or_else(|| format!("unknown chain {k}"))?;
    if !crate::chains::kept(node, p) {
        return Err(format!("{} is not installed here: a swap's legs are read on this arcade's own copy", p.name));
    }
    Ok(p)
}
pub(crate) fn wallet_key(node: &Node, p: &ChainParams) -> Result<(arcade_wallet::SecretKey, PublicKey), String> {
    let sk = node.app.tag_key(p, 0).map_err(|e| e.to_string())?.1;
    Ok((sk, PublicKey::from_secret_key(SECP256K1, &sk)))
}

/// What a step says when the account a swap or order belongs to is not on this arcade now. Nothing is given up: the
/// swap waits, and its claim or refund goes on once the account is added back.
pub(crate) fn account_gone(id: &str) -> String {
    let who = if id.is_empty() { "this arcade's own wallet".to_string() } else { format!("account {id}") };
    format!("{who}, which this belongs to, is not on this arcade now (signed out?): nothing is signed until it is added back with its words; then its claim or refund goes on")
}

/// The key (tag slot 0, as the wallet's) of account `id` on `p` ("" = this arcade's own wallet), whichever account is
/// active now: a swap or order signs with the keys of the account that placed or took it, never the one switched to
/// since (its locks name that account's key, and its coins are that account's).
pub(crate) fn account_key(node: &Node, p: &ChainParams, id: &str) -> Result<(arcade_wallet::SecretKey, PublicKey), String> {
    match node.app.account_tag_key(id, p, 0) {
        Ok(sk) => Ok((sk, PublicKey::from_secret_key(SECP256K1, &sk))),
        Err(arcade_app::AppError::NoWallet) => Err(account_gone(id)),
        Err(e) => Err(e.to_string()),
    }
}

/// The account an order belongs to: the one active when it was placed (`account`). An order placed before orders
/// named their account belongs to the one active now, as it did then.
pub(crate) fn order_account(node: &Node, o: &Value) -> String {
    o["account"].as_str().map(String::from).unwrap_or_else(|| node.app.active_account())
}

/// The key one side of a swap uses on `p`: an order's own key there (an order placed for somebody else with a key of
/// its own, the website's delegated orders: Martin 2026-11-12, maker side first), else the key of `account` (the
/// account that placed the order or took it).
fn key_of(node: &Node, p: &ChainParams, o: Option<&Value>, account: &str) -> Result<(arcade_wallet::SecretKey, PublicKey), String> {
    if let Some(h) = o.and_then(|o| o["own"]["keys"][p.key].as_str()) {
        let b = hex::decode(h).map_err(|_| "an order's key is 32 bytes of hex")?;
        let sk = arcade_wallet::SecretKey::from_slice(&b).map_err(|_| "an order's key is not a key")?;
        return Ok((sk, PublicKey::from_secret_key(SECP256K1, &sk)));
    }
    account_key(node, p, account)
}

/// The key this side of swap `s` signs with on `p`: [`key_of`] for the account the swap belongs to (`s.account`),
/// whichever account is active now, and only when it is the key the swap's locks name for this side. A swap from
/// before swaps named their account belongs to the account here whose key its locks name (found once, then kept).
fn signer(node: &Node, p: &ChainParams, s: &mut Value, o: Option<&Value>) -> Result<(arcade_wallet::SecretKey, PublicKey), String> {
    let mine = if s["role"] == "taker" { "taker_keys" } else { "maker_keys" };
    let want = s[mine][p.key].as_str().unwrap_or("").to_string();
    let named = |k: &(arcade_wallet::SecretKey, PublicKey)| hex::encode(k.1.serialize()) == want;
    if let Some(a) = s["account"].as_str() {
        let k = key_of(node, p, o, a)?;
        if !named(&k) {
            return Err(format!("this side's key on {} is not the one the swap's locks name: nothing signed", p.name));
        }
        return Ok(k);
    }
    let found = std::iter::once(String::new()).chain(crate::accountsvc::ids(node)).find_map(|a| key_of(node, p, o, &a).ok().filter(|k| named(k)).map(|k| (a, k)));
    let (a, k) = found.ok_or("the account that started this swap is not on this arcade now (signed out?): nothing is signed until it is added back with its words; then its claim or refund goes on")?;
    s["account"] = json!(a);
    Ok(k)
}

/// Where an order's coins and tokens come out on `p`: its payout address there (the person it was placed for), else
/// `own` (this wallet).
fn payout(p: &ChainParams, o: Option<&Value>, own: Vec<u8>) -> Vec<u8> {
    o.and_then(|o| o["own"]["payout"][p.key].as_str()).and_then(|a| arcade_ledger::address::script_for_address(p, a)).unwrap_or(own)
}

/// The maker's operator fee on `p` for `amount` (the order's `maker_fee {bps, to: {chain: address}}`): Martin
/// 2026-11-12, "the maker fee can be in the maker token": a share of what the maker gives, paid with its lock (for an
/// inscription given, of the coins it gets, at its claim). None when there is none, or under the dust line.
fn maker_fee(p: &ChainParams, o: Option<&Value>, amount: u64) -> Option<(u64, Vec<u8>)> {
    let f = &o?["maker_fee"];
    let bps = f["bps"].as_u64().filter(|b| *b > 0 && *b <= MAX_FEE_BPS)?;
    let to = arcade_ledger::address::script_for_address(p, f["to"][p.key].as_str()?)?;
    let fee = (amount as u128 * bps as u128 / 10_000) as u64;
    (fee >= htlc::DUST).then_some((fee, to))
}

/// The maker fee in a token's own units (an arcade-ledger token: no dust line), to the operator's address on `p`.
fn token_fee(o: Option<&Value>, amount: u64, p: &ChainParams) -> Option<(u64, Vec<u8>)> {
    let f = &o?["maker_fee"];
    let bps = f["bps"].as_u64().filter(|b| *b > 0 && *b <= MAX_FEE_BPS)?;
    let to = arcade_ledger::address::script_for_address(p, f["to"][p.key].as_str()?)?;
    let fee = (amount as u128 * bps as u128 / 10_000) as u64;
    (fee > 0).then_some((fee, to))
}

/// A maker fee in whole units, rounded up (Martin 2026-11-12: "DD cannot be fractionalized", fees on it "Whole
/// dollars, rounded up"; DRC-20 / PRC-20 legs are whole tokens too): ceil(whole * bps / 10000), at least 1 when there is
/// a fee at all. `whole` is the leg in its whole units. The operator's address on `p` with it.
fn whole_fee(o: Option<&Value>, whole: u128, p: &ChainParams) -> Option<(u128, String)> {
    let f = &o?["maker_fee"];
    let bps = f["bps"].as_u64().filter(|b| *b > 0 && *b <= MAX_FEE_BPS)? as u128;
    // Checked by the send that pays it (a DigiDollar address is not a coin address).
    let to = f["to"][p.key].as_str()?.to_string();
    (whole > 0).then(|| ((whole * bps).div_ceil(10_000).max(1), to))
}

/// An order as anybody else sees it: never its own keys or payout addresses, and each leg's chain by
/// its key ([`canon_legs`]).
pub fn public_order(o: &Value) -> Value {
    let mut o = o.clone();
    if let Some(m) = o.as_object_mut() {
        m.remove("own");
        m.remove("account");
    }
    canon_legs(&mut o);
    o
}

/// A leg's chain by its key ("peptest"), not its mesh name ("test"): chains::find takes both, but pages
/// and apps name a chain by its key (an order placed with "test" showed in the Market app as "undefined").
pub fn canon_leg(leg: &mut Value) {
    if let Some(p) = leg["chain"].as_str().and_then(crate::chains::find) {
        leg["chain"] = json!(p.key);
    }
}

/// [`canon_leg`] on an order's give and get.
pub fn canon_legs(o: &mut Value) {
    canon_leg(&mut o["give"]);
    canon_leg(&mut o["get"]);
}

pub(crate) fn p2pkh_of(pk: &PublicKey) -> Vec<u8> {
    arcade_ledger::address::p2pkh_script(&arcade_ledger::address::hash160(&pk.serialize()))
}
pub(crate) fn depth(node: &Node, p: &ChainParams, txid: &Hash256) -> u32 {
    // Borrowed (not installed, or catching up): from the block a lender names, checked against our headers.
    if crate::borrow::is_borrowed(p.key) && !crate::arcsvc::runs(p) {
        return crate::borrow::depth(node, p, txid);
    }
    let Ok(Some((b, _))) = node.chain.tx_location(p, txid) else { return 0 };
    let Some((_, h, _)) = node.chain.block_at(p, b).ok().flatten() else { return 0 };
    node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height).unwrap_or(0).saturating_sub(h) + 1
}
/// [`depth`] as it counts for money: where a block's height proves little (DigiByte, Kotia, Wojakcoin: headers
/// whose work isn't checked here, or whose bits aren't tied to the chain's difficulty), only the checked work above
/// the transaction counts (arcade_chaindb::proven). The other side's lock is read with this; this side's own steps
/// use the plain depth.
pub(crate) fn money_depth(node: &Node, p: &ChainParams, txid: &Hash256) -> u32 {
    let Ok(Some((b, _))) = node.chain.tx_location(p, txid) else { return 0 };
    let Some((_, h, _)) = node.chain.block_at(p, b).ok().flatten() else { return 0 };
    node.chain.money_depth(p).map(|m| m.at(h)).unwrap_or(0)
}
/// A leg on a chain where nothing can confirm a lock (no checked work at its tip: Kotia's staked blocks) is
/// refused before anything is locked: the other side's lock there could never be seen as mined.
fn confirmable(node: &Node, leg: &Value) -> Result<(), String> {
    match crate::chains::find(leg["chain"].as_str().unwrap_or("")) {
        Some(p) => confirmable_on(node, p),
        None => Ok(()),
    }
}
fn confirmable_on(node: &Node, p: &ChainParams) -> Result<(), String> {
    let Ok(m) = node.chain.money_depth(p) else { return Ok(()) };
    if p.confirms_by_proven_work() && m.tip() > 0 && !m.confirms() {
        let why = if p.work_checkable_at(m.tip() + 1) { "" } else { " (they are proof of stake, which is not checked yet)" };
        return Err(format!("{}'s blocks here carry no work this arcade can check{why}: a lock on it can't be confirmed, so it can't be a swap leg", p.name));
    }
    Ok(())
}
fn tx_of(node: &Node, p: &ChainParams, txid: &Hash256) -> Option<Tx> {
    if let Ok(Some((raw, _))) = node.chain.tx_raw(p, txid) {
        return Tx::parse_with(&raw, &p.format).ok();
    }
    if crate::arcsvc::runs(p) {
        return crate::arcsvc::waiting(usize::MAX).into_iter().filter_map(|raw| Tx::parse_with(&raw, &p.format).ok()).find(|t| &t.txid == txid);
    }
    node.chain.mempool(p.chain_no).ok()?.into_iter().filter_map(|(_, raw)| Tx::parse_with(&raw, &p.format).ok()).find(|t| &t.txid == txid)
}
/// The waiting transaction spending `op`, in the chain's own pool.
pub(crate) fn pool_spender(node: &Node, p: &ChainParams, op: &OutPoint) -> Option<Hash256> {
    if crate::arcsvc::runs(p) {
        return crate::arcsvc::spender(op);
    }
    // Borrowed: what this arcade sent through the lenders and still waits.
    if crate::borrow::is_borrowed(p.key) {
        return crate::borrow::pool_spender(p, op);
    }
    node.chain.mempool_spender(p.chain_no, op).ok().flatten()
}
fn spender(node: &Node, p: &ChainParams, op: &OutPoint) -> Option<Tx> {
    if let Ok(Some((b, pos, _))) = node.chain.spender(p, op) {
        return tx_of(node, p, &node.chain.txid_at(p, b, pos).ok().flatten()?);
    }
    tx_of(node, p, &pool_spender(node, p, op)?)
}
fn h256(v: &Value) -> Result<Hash256, String> {
    Hash256::from_hex(v.as_str().unwrap_or("")).map_err(|_| "bad txid".to_string())
}

// ---------------- the ARCADE chain ----------------
//
// The ARCADE chain (crates/arcade-arc) runs its own pool, and its time locks are block heights only (it has no
// clock-time locks: arcade-arc script.rs, OP_CHECKLOCKTIMEVERIFY). So a lock on it ends at a height, worked out
// from the clock time the swap gives that lock and the chain's own recent pace, with margins either way:
// a taker's lock (the long one) is made 4 times the order's time and the maker accepts it at 3 times, counting
// blocks twice as fast as lately; a maker's lock is 1 time, while the taker's clock lock is 4, so the maker's
// height comes first unless blocks run 3 times slower than lately. Its signatures are V1.0's legacy ones, so
// the hash lock (htlc.rs) is spent as on the Dogecoin family.

/// A chain whose locks are block heights (the ARCADE chain).
fn by_height(p: &ChainParams) -> bool {
    arcade_chain::params::ARCADE_CHAINS.iter().any(|(_, k)| *k == p.key)
}
/// How many times the order's time a taker's lock lasts: 4 when a leg is on the ARCADE chain (see above), else 2.
fn taker_factor(o: &Value) -> u32 {
    let h = |l: &Value| crate::chains::find(l["chain"].as_str().unwrap_or("")).is_some_and(by_height);
    if h(&o["give"]) || h(&o["get"]) { 4 } else { 2 }
}
fn tip_height(node: &Node, p: &ChainParams) -> u32 {
    node.chain.tip(p.chain_no).ok().flatten().map(|t| t.height).unwrap_or(0)
}
/// Seconds per block over the last 60 blocks here (at least 1), else the ARCADE chain's 150.
fn block_secs(node: &Node, p: &ChainParams) -> u32 {
    let tip = tip_height(node, p);
    let time = |h: u32| node.chain.hash_at(p.chain_no, h).ok().flatten().and_then(|id| node.chain.header(p.chain_no, &id).ok().flatten()).map(|r| r.header.time);
    if tip > 60 {
        if let (Some(a), Some(b)) = (time(tip - 60), time(tip)) {
            return (b.saturating_sub(a) / 60).max(1);
        }
    }
    arcade_arc::params::BLOCK_SECONDS as u32
}
/// The height a lock on `p` ends at for `secs` from now.
fn height_for(node: &Node, p: &ChainParams, secs: u32) -> u32 {
    tip_height(node, p) + secs.div_ceil(block_secs(node, p)).max(6)
}
/// Whether a maker's lock ending at `end` (a height when `by_height`, else a clock time) still leaves the taker time
/// to claim it: half the order's lock time from `at` (the tip's height, or now), and never under 120 s / 3 blocks.
/// The claim puts the secret in the pool; a maker whose lock ends first could refund it and claim the taker's lock
/// with that secret too. An honest maker's lock runs `maker_secs` from when it was told (maker_begin), and the taker
/// sees it within a few blocks, so half of it is a wide margin that an honest swap always has; `secs` is this
/// arcade's own lock time (the taker's constant, not the maker's word). Not claimed, nothing is lost: the taker's own
/// lock refunds after its time.
fn claim_time_left(by_height: bool, end: u32, at: u32, secs: u32, block_secs: u32) -> bool {
    if by_height {
        end > at + (secs / 2).div_ceil(block_secs.max(1)).max(3)
    } else {
        end >= at + (secs / 2).max(120)
    }
}

/// Whether a lock's end has come: its height is mined over (a refund can go in the next block), or its time is past.
fn ended(node: &Node, p: &ChainParams, spec_time: u32) -> bool {
    if by_height(p) {
        tip_height(node, p) >= spec_time
    } else {
        now() > spec_time + 600
    }
}

/// Send a transaction on a chain: through this arcade's own pool on its home chain, else to the chain's peers. It is
/// remembered and sent again until it is in a block (relay): a peer drops a transaction whose parent it hasn't seen
/// yet, and nothing would ask again (2026-11-10: three swap locks never reached the network).
pub(crate) fn send(node: &Node, p: &ChainParams, tx: &Tx) -> Result<String, String> {
    let txid = relay_once(node, p, tx)?;
    let _ = node.app.store.write(|w| w.put(arcade_app::META, format!("swapsend:{}:{txid}", p.key).as_bytes(), &[now().to_le_bytes().as_slice(), &tx.raw].concat()));
    Ok(txid)
}

/// Every RELAY_EVERY: send again what a swap sent and no block holds yet; forget what is mined (or a day old).
const RELAY_EVERY: u32 = 180;
fn relay(node: &Node) {
    let rows = node.app.store.read(|r| r.scan_prefix(arcade_app::META, b"swapsend:")).unwrap_or_default();
    for (k, v) in rows {
        let name = String::from_utf8_lossy(&k[9..]).to_string();
        let Some((chain, txid)) = name.split_once(':') else { continue };
        let (Some(p), Ok(h)) = (crate::chains::find(chain), Hash256::from_hex(txid)) else { continue };
        let at = u32::from_le_bytes(v[..4].try_into().unwrap_or([0; 4]));
        if depth(node, p, &h) >= 1 || now() > at + 86_400 {
            let _ = node.app.store.write(|w| w.del(arcade_app::META, &k));
            continue;
        }
        if now() < at + RELAY_EVERY {
            continue;
        }
        if let Ok(tx) = Tx::parse_with(&v[4..], &p.format) {
            if crate::arcsvc::runs(p) {
                // Its own pool hands it on; one it holds already is refused, which is fine.
                let _ = crate::ownwait::send_arc(node, &tx);
            } else if crate::borrow::is_borrowed(p.key) {
                // Borrowed: its lenders hold its parents already (they were sent through them too). One still waiting
                // there is left be; one that dropped out is sent again (crate::ownwait puts it back too).
                if !crate::borrow::pooled(p, &h) {
                    let _ = crate::ownwait::send_own(node, p, &tx);
                }
            } else if p.chain_no == node.params.chain_no {
                // Its unconfirmed parents first, oldest first: a peer drops a child whose parent it hasn't seen.
                for parent in ancestors(node, p, &tx) {
                    node.tx_outbox.lock().unwrap().push(parent);
                }
                let _ = crate::mempoolshare::add(node, p, &tx);
                node.tx_outbox.lock().unwrap().push(tx.raw.clone());
            } else {
                let _ = crate::sitesvc::broadcast(node, &json!({"hex": hex::encode(&tx.raw), "chain": p.key}));
            }
            let _ = node.app.store.write(|w| w.put(arcade_app::META, &k, &[now().to_le_bytes().as_slice(), &tx.raw].concat()));
        }
    }
}

/// The unconfirmed transactions in this arcade's own pool that `tx` spends from, oldest first (up to 25).
fn ancestors(node: &Node, p: &ChainParams, tx: &Tx) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut todo: Vec<Hash256> = tx.inputs.iter().map(|i| i.prevout.txid).collect();
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = todo.pop() {
        if out.len() >= 25 || !seen.insert(id) {
            continue;
        }
        let Ok(Some(raw)) = node.chain.mempool_tx(p.chain_no, &id) else { continue };
        if let Ok(t) = Tx::parse_with(&raw, &p.format) {
            todo.extend(t.inputs.iter().map(|i| i.prevout.txid));
        }
        out.push(raw);
    }
    out.reverse();
    out
}

fn relay_once(node: &Node, p: &ChainParams, tx: &Tx) -> Result<String, String> {
    if crate::arcsvc::runs(p) {
        return Ok(crate::ownwait::send_arc(node, &tx)?.0.to_hex());
    }
    // Borrowed, the home chain or another (borrowed = installed, batch 6): through the lenders, remembered until a
    // block has it (crate::ownwait puts it back if it drops out).
    if crate::borrow::is_borrowed(p.key) {
        let p = crate::chains::find(p.key).ok_or_else(|| format!("unknown chain {}", p.key))?;
        crate::ownwait::send_own(node, p, tx).map_err(|e| e.replace("the transaction was refused", "refused"))?;
        return Ok(tx.txid.to_hex());
    }
    if p.chain_no == node.params.chain_no {
        crate::ownwait::send_own(node, node.params, tx).map_err(|e| e.replace("the transaction was refused", "refused"))?;
        return Ok(tx.txid.to_hex());
    }
    let v = crate::sitesvc::broadcast(node, &json!({"hex": hex::encode(&tx.raw), "chain": p.key}))?;
    Ok(v["txid"].as_str().unwrap_or("").to_string())
}
pub(crate) fn send_raw(node: &Node, p: &ChainParams, raw: &[u8]) -> Result<String, String> {
    send(node, p, &Tx::parse_with(raw, &p.format).map_err(|e| e.to_string())?)
}

/// This wallet's spendable coins on a chain: its key's P2PKH (and P2WPKH where it receives SegWit), none spent in
/// the pool and none carrying an inscription (a fee must never spend an inscribed coin).
pub(crate) fn coins(node: &Node, p: &'static ChainParams, pk: &PublicKey) -> Vec<Utxo> {
    let mut out = Vec::new();
    for (_, script) in crate::coinsend::key_scripts(p, pk) {
        let list: Vec<Utxo> = if crate::borrow::is_borrowed(p.key) && !crate::arcsvc::runs(p) {
            // Borrowed (borrowed = installed, batch 6): the coins a lender's address index names, each checked
            // against its block, with the ones followed here and our own waiting change.
            let mut l = crate::borrow::own_coins(node, p, &script);
            l.retain(|u| !node.pending_spent.lock().unwrap().contains(&u.outpoint));
            l
        } else if p.chain_no == node.params.chain_no && script == p2pkh_of(pk) {
            crate::claims::spendable(node, &script)
        } else {
            node.chain
                .coins_for_script(p, &script)
                .unwrap_or_default()
                .into_iter()
                .filter(|(op, _, _)| pool_spender(node, p, op).is_none())
                .filter(|(op, _, _)| !node.chain.coin(p, op).ok().flatten().is_some_and(|c| c.coinbase && c.height + p.coinbase_maturity > tip_height(node, p)))
                .map(|(op, value, _)| Utxo { outpoint: op, value, script: script.clone() })
                .collect()
        };
        out.extend(list);
    }
    // Coins carrying an inscription stay out (crate::ordsafe: this arcade's ord index, else the mesh's, else
    // small coins nobody can read), as in every other send.
    crate::ordsafe::keep_free(node, p, out)
}

fn fee_kb(node: &Node, p: &ChainParams) -> (u64, u64) {
    if p.chain_no == node.params.chain_no {
        return (crate::claims::FEE_PER_KB, crate::claims::DUST);
    }
    let f = crate::coinsend::fee_policy(p);
    (f.fallback_per_kb.max(f.floor_per_kb), f.dust)
}

/// `outs`, then enough of `coins` for them and the fee, change back to `change`: a key-hash transaction signed by
/// `sk`. `first` are inputs that must come first, in order (an inscription's coin as input 0).
pub(crate) fn build(node: &Node, p: &ChainParams, sk: &arcade_wallet::SecretKey, first: Vec<Utxo>, coins: Vec<Utxo>, outs: Vec<TxOutSpec>, change: &[u8]) -> Result<Tx, String> {
    let (per_kb, dust) = fee_kb(node, p);
    let need: u64 = outs.iter().map(|o| o.value).sum();
    let mut ins = first;
    let mut have: u64 = ins.iter().map(|u| u.value).sum();
    let mut sorted = coins;
    sorted.sort_by(|a, b| b.value.cmp(&a.value));
    let size = |n: usize| (10 + 148 * n + outs.iter().map(|o| 9 + o.script.len()).sum::<usize>() + 34) as u64;
    // The Dogecoin family's soft dust: each output under 0.01 adds 0.01 to the fee a peer asks before it relays the
    // transaction (the node's own RPC doesn't ask, so a short fee looks fine there and is dropped on the network).
    let soft = if soft_dust(p) { outs.iter().filter(|o| o.value > 0 && o.value < SOFT_DUST).count() as u64 * SOFT_DUST } else { 0 };
    let fee = |n: usize| arcade_wallet::tx::fee_for_size(p, size(n), per_kb).max(STEP_FEE / 3) + soft;
    for c in sorted {
        if have >= need + fee(ins.len()) + dust {
            break;
        }
        have += c.value;
        ins.push(c);
    }
    let f = fee(ins.len());
    if have < need + f {
        return Err(format!("not enough coins on {}: {have} for {}", p.name, need + f));
    }
    let mut outs = outs;
    if have - need - f >= dust {
        outs.push(TxOutSpec { value: have - need - f, script: change.to_vec() });
    }
    let version = if p.format.segwit { 2 } else { 1 };
    let tx = arcade_wallet::tx::sign_keyhash(p, version, &ins, &outs, sk, now())?;
    if !arcade_wallet::tx::verify_keyhash(p, &tx, &ins) {
        return Err("the transaction didn't verify".into());
    }
    Ok(tx)
}

pub(crate) const SOFT_DUST: u64 = 1_000_000;
/// Chains with the Dogecoin family's soft-dust fee rule.
pub(crate) fn soft_dust(p: &ChainParams) -> bool {
    arcade_wallet::tx::per_started_kb(p)
}

// ---------------- legs ----------------

/// A lock's terms: the hash, who claims, who refunds (compressed keys on the leg's chain), and when.
#[derive(Clone)]
struct Spec {
    hash: [u8; 32],
    claimer: Vec<u8>,
    refunder: Vec<u8>,
    time: u32,
    /// The claim takes only a 32-byte secret (htlc::Htlc::sized): the maker's lock, when the maker said so.
    sized: bool,
}

impl Spec {
    fn htlc(&self) -> Htlc {
        Htlc { hash: self.hash, claimer: self.claimer.clone(), refunder: self.refunder.clone(), time: self.time, sized: self.sized }
    }
    fn dd(&self) -> Result<Lock, String> {
        let x = |k: &[u8]| XOnlyPublicKey::from_slice(k.get(1..33).ok_or("bad key")?).map_err(|_| "bad key".to_string());
        Ok(Lock { hash: self.hash, claimer: x(&self.claimer)?, refunder: x(&self.refunder)?, time: self.time, sized: self.sized })
    }
    fn script(&self, kind: &str) -> Result<Vec<u8>, String> {
        Ok(if kind == "dd" { self.dd()?.script() } else { self.htlc().script() })
    }
}

fn leg_ok(node: &Node, leg: &Value) -> Result<&'static ChainParams, String> {
    let p = chain(node, leg["chain"].as_str().unwrap_or(""))?;
    let kind = leg["kind"].as_str().unwrap_or("");
    if !KINDS.contains(&kind) {
        return Err(format!("a leg is one of {KINDS:?}"));
    }
    if by_height(p) && kind != "coin" {
        return Err("on the ARCADE chain a leg is its coin".into());
    }
    if kind == "dd" && !matches!(p.key, "dgb" | "dgbtest") {
        return Err("DigiDollars are on dgb or dgbtest".into());
    }
    if kind == "token" && p.chain_no != node.params.chain_no {
        return Err("an arcade-ledger token leg is on this arcade's home chain (its ledger)".into());
    }
    // A DRC-20 / PRC-20 leg on any chain kept here with its tokens index (2026-11-12; it was the home chain only).
    if kind == "token20" && !token20_kept(node, p) {
        return Err(format!("{} has no tokens index here: a DRC-20 / PRC-20 leg is read on this arcade's own index", p.name));
    }
    if leg["amount"].as_str().unwrap_or("").is_empty() {
        return Err("a leg names its amount, as a string".into());
    }
    Ok(p)
}

fn token20_kept(node: &Node, p: &ChainParams) -> bool {
    arcade_ledger::tokens::protocol(p).is_some()
        && if p.chain_no == node.params.chain_no {
            crate::chains::home_indexes(node).iter().any(|i| i == "tokens")
        } else {
            node.extra.lock().unwrap().get(p.key).is_some_and(|r| r.indexes.iter().any(|i| i == "tokens"))
        }
}

fn units(leg: &Value) -> u64 {
    leg["amount"].as_str().and_then(|a| a.parse().ok()).unwrap_or(0)
}

/// One step of locking `leg` under `spec` with `key` (this side's, [`signer`]); `st` is the lock's own record. Done
/// when st.step is "locked".
fn lock_step(node: &Node, leg: &Value, spec: &Spec, st: &mut Value, o: Option<&Value>, key: &(arcade_wallet::SecretKey, PublicKey)) -> Result<(), String> {
    let p = leg_ok(node, leg)?;
    let (sk, pk) = *key;
    let own = crate::coinsend::key_scripts(p, &pk)[0].1.clone();
    let kind = leg["kind"].as_str().unwrap_or("");
    let script = spec.script(kind)?;
    let ready = |k: &str| st[k].as_str().and_then(|x| Hash256::from_hex(x).ok()).is_some_and(|h| depth(node, p, &h) >= 1);
    match (kind, st["step"].as_str().unwrap_or("start")) {
        ("coin", "start") => {
            let mut outs = vec![TxOutSpec { value: units(leg) + htlc::SPEND_FEE, script: script.clone() }];
            if let Some((fee, to)) = maker_fee(p, o, units(leg)) {
                outs.push(TxOutSpec { value: fee, script: to });
            }
            let tx = build(node, p, &sk, vec![], coins(node, p, &pk), outs, &own)?;
            st["txid"] = json!(send(node, p, &tx)?);
            st["vout"] = json!(0);
            st["step"] = json!("locked");
        }
        ("dd", "start") => {
            // This side's key's dollars and DGB (the order's own key, or the swap's account's), never the active account's.
            let own_key = Some(sk);
            let (dsk, held, dgb, _, change) = crate::ddsvc::trade_coins_for(node, p, own_key.as_ref())?;
            let tx = ddlock::build_lock(p, &dsk, &held, &dgb, &change, &spec.dd()?, units(leg) as i64)?;
            st["txid"] = json!(send(node, p, &tx)?);
            st["vout"] = json!(0);
            // The maker fee in DigiDollars, whole dollars rounded up: a DD send to the operator, made with the lock.
            if let Some((dollars, to)) = whole_fee(o, (units(leg) / 100) as u128, p) {
                match crate::ddsvc::send_with(node, Some(p.key), &to, (dollars * 100) as i64, true, own_key.as_ref()) {
                    Ok(v) => st["fee_tx"] = v["txid"].clone(),
                    Err(e) => st["fee_error"] = json!(e),
                }
            }
            st["step"] = json!("locked");
        }
        ("token", "start") => {
            let pid: u32 = leg["asset"].as_str().and_then(|a| a.parse().ok()).ok_or("a token leg's asset is its property id")?;
            let mut payload = vec![0, 0, 0, 0];
            payload.extend_from_slice(&pid.to_be_bytes());
            payload.extend_from_slice(&units(leg).to_be_bytes());
            let tx = arcade_wallet::tx::build_arcade(p, &payload, vec![TxOutSpec { value: LEDGER_COIN, script: script.clone() }], &sk, &coins(node, p, &pk), crate::claims::FEE_PER_KB, crate::claims::DUST, now()).map_err(|e| e.to_string())?;
            st["vout"] = json!(tx.outputs.iter().position(|x| x.script_pubkey == script).ok_or("no lock output")?);
            st["txid"] = json!(send(node, p, &tx)?);
            // The maker fee in the token itself: a ledger send of it to the operator, made with the lock.
            if let Some((fee, to)) = token_fee(o, units(leg), p) {
                let mut fp = vec![0, 0, 0, 0];
                fp.extend_from_slice(&pid.to_be_bytes());
                fp.extend_from_slice(&fee.to_be_bytes());
                let left: Vec<Utxo> = coins(node, p, &pk).into_iter().filter(|c| !tx.inputs.iter().any(|i| i.prevout == c.outpoint)).collect();
                match arcade_wallet::tx::build_arcade(p, &fp, vec![TxOutSpec { value: LEDGER_COIN, script: to }], &sk, &left, crate::claims::FEE_PER_KB, crate::claims::DUST, now()) {
                    Ok(t) => st["fee_tx"] = json!(send(node, p, &t)?),
                    Err(e) => st["fee_error"] = json!(e.to_string()),
                }
            }
            st["step"] = json!("locked");
        }
        ("ord", "start") => {
            let id = leg["asset"].as_str().unwrap_or("");
            let (at, off) = arcade_ledger::ord::location(&node.chain, p, id).ok_or("no such inscription here")?;
            if off != 0 {
                return Err("that inscription isn't at its coin's first unit".into());
            }
            let coin = node.chain.coin(p, &at).ok().flatten().ok_or("its coin is spent")?;
            if coin.script != own && coin.script != p2pkh_of(&pk) {
                return Err("that inscription is not this wallet's".into());
            }
            let first = vec![Utxo { outpoint: at, value: coin.value, script: coin.script.clone() }];
            let tx = build(node, p, &sk, first, coins(node, p, &pk), vec![TxOutSpec { value: coin.value, script: script.clone() }, TxOutSpec { value: RESERVE, script: script.clone() }], &own)?;
            st["txid"] = json!(send(node, p, &tx)?);
            st["vout"] = json!(0);
            st["step"] = json!("locked");
        }
        ("token20", step) => {
            let proto = arcade_ledger::tokens::protocol(p).ok_or("no DRC-20 / PRC-20 here")?.0;
            let note = format!(r#"{{"p":"{proto}","op":"transfer","tick":"{}","amt":"{}"}}"#, leg["asset"].as_str().unwrap_or(""), leg["amount"].as_str().unwrap_or(""));
            let own = p2pkh_of(&pk);
            match step {
                "start" => {
                    let i = arcade_wallet::ordwrite::inscribe(p, &sk, &coins(node, p, &pk), CTYPE, note.as_bytes(), &own, POSTAGE, STEP_FEE, crate::claims::FEE_PER_KB, crate::claims::DUST, now())?;
                    send(node, p, &i.commit)?;
                    st["note1"] = json!(send_raw(node, p, &i.reveal)?);
                    st["step"] = json!("note1");
                    // The maker fee in the token, whole tokens rounded up: a token send to the operator (its own transfer
                    // note, then the note's coin sent: tokensend's job), started with the lock.
                    let whole = leg["amount"].as_str().and_then(|a| a.parse::<u128>().ok()).unwrap_or(0);
                    if let Some((fee, to)) = whole_fee(o, whole, p) {
                        let key = hex::encode(sk.secret_bytes());
                        match crate::tokensend::t20_send_with(node, p.key, leg["asset"].as_str().unwrap_or(""), &fee.to_string(), &to, true, Some(&key)) {
                            Ok(v) => st["fee_job"] = v["id"].clone(),
                            Err(e) => st["fee_error"] = json!(e),
                        }
                    }
                }
                "note1" if ready("note1") => {
                    let note1 = OutPoint { txid: h256(&st["note1"])?, vout: 0 };
                    let (commit, _) = arcade_wallet::ordwrite::commit_lock(&pk, CTYPE, note.as_bytes());
                    let first = vec![Utxo { outpoint: note1, value: POSTAGE, script: own.clone() }];
                    let tx = build(node, p, &sk, first, coins(node, p, &pk), vec![TxOutSpec { value: POSTAGE, script: script.clone() }, TxOutSpec { value: POSTAGE + RESERVE + STEP_FEE, script: commit }], &own)?;
                    st["fund"] = json!(send(node, p, &tx)?);
                    st["step"] = json!("funded");
                }
                "funded" if ready("fund") => {
                    let commit = OutPoint { txid: h256(&st["fund"])?, vout: 1 };
                    let raw = arcade_wallet::ordwrite::reveal(p, &sk, commit, CTYPE, note.as_bytes(), vec![(POSTAGE, script.clone()), (RESERVE, script.clone())], now())?;
                    st["txid"] = json!(send_raw(node, p, &raw)?);
                    st["vout"] = json!(0);
                    st["step"] = json!("locked");
                }
                _ => {}
            }
        }
        _ => {}
    }
    Ok(())
}

/// Whether the other side's lock of `leg` under `spec` is there as agreed, read on this arcade's own copy of the
/// chain: Ok(true) good and mined, Ok(false) not yet, Err wrong (never claimed against).
fn seen(node: &Node, leg: &Value, spec: &Spec, st: &Value) -> Result<bool, String> {
    let p = leg_ok(node, leg)?;
    let kind = leg["kind"].as_str().unwrap_or("");
    let Ok(txid) = h256(&st["txid"]) else { return Ok(false) };
    let Some(tx) = tx_of(node, p, &txid) else { return Ok(false) };
    // Mined under work this arcade checked, not just in a block (a made-up block costs nothing on some chains).
    if money_depth(node, p, &txid) < 1 {
        return Ok(false);
    }
    let script = spec.script(kind)?;
    let vout = st["vout"].as_u64().unwrap_or(0) as usize;
    if tx.outputs.get(vout).map(|o| &o.script_pubkey) != Some(&script) {
        return Err("that transaction doesn't pay the lock computed here".into());
    }
    match kind {
        "coin" => {
            if tx.outputs[vout].value < units(leg) + htlc::SPEND_FEE {
                return Err("the lock holds less than agreed".into());
            }
        }
        "dd" => {
            let rec = arcade_ledger::digidollar::read(&tx).ok_or("not a DigiDollar transaction")?;
            let cents: i64 = rec.outputs.iter().filter(|o| o.0 as usize == vout).map(|o| o.2).sum();
            if cents != units(leg) as i64 || !tx.outputs.iter().any(|o| o.value > 0 && o.script_pubkey == script) {
                return Err(format!("the dollar lock holds {cents} cents, not {}", units(leg)));
            }
        }
        "token" => {
            let pid: u32 = leg["asset"].as_str().and_then(|a| a.parse().ok()).ok_or("bad property")?;
            let addr = arcade_ledger::address::sender_address_of(&script, p).ok_or("bad lock")?;
            let held = led::holdings(&node.chain, p.chain_no, &addr).into_iter().find(|(x, _)| *x == pid).map(|x| x.1).unwrap_or(0);
            if held < units(leg) {
                return Ok(false); // the ledger reads the block after the coins: wait for it
            }
        }
        "ord" => {
            let (at, off) = arcade_ledger::ord::location(&node.chain, p, leg["asset"].as_str().unwrap_or("")).ok_or("no such inscription here")?;
            if at != (OutPoint { txid, vout: 0 }) || off != 0 || tx.outputs.get(1).map(|o| &o.script_pubkey) != Some(&script) {
                return Err("the inscription is not at the lock's first coin".into());
            }
        }
        "token20" => {
            let note = arcade_ledger::tokens::note(&node.chain, p.chain_no, &arcade_ledger::ord::iid_bytes(&txid, 0));
            let want = arcade_ledger::tokens::num(leg["amount"].as_str().unwrap_or("")).and_then(|n| n.units(18)).map(|u| u.to_string());
            let addr = spec.htlc().address(p);
            let ok = note.as_ref().is_some_and(|n| n["tick"] == leg["asset"].as_str().unwrap_or("").to_lowercase() && n["owner"] == addr.as_str() && n["amt"].as_str().map(String::from) == want);
            if !ok || tx.outputs.get(1).map(|o| &o.script_pubkey) != Some(&script) {
                return Ok(false); // the token index may not have read it yet
            }
        }
        _ => {}
    }
    Ok(true)
}

/// The lock's coins, in the order a claim or refund spends them.
fn lock_coins(node: &Node, leg: &Value, st: &Value) -> Result<(Tx, Vec<(OutPoint, u64)>), String> {
    let p = leg_ok(node, leg)?;
    let txid = h256(&st["txid"])?;
    let tx = tx_of(node, p, &txid).ok_or("the lock isn't here")?;
    let vout = st["vout"].as_u64().unwrap_or(0) as u32;
    let mut v = vec![(OutPoint { txid, vout }, tx.outputs.get(vout as usize).ok_or("no lock output")?.value)];
    if matches!(leg["kind"].as_str(), Some("token20" | "ord")) {
        v.push((OutPoint { txid, vout: 1 }, tx.outputs.get(1).ok_or("no reserve")?.value));
    }
    Ok((tx, v))
}

/// Claim (with `secret`) or refund (None) a lock of `leg` with `key` (this side's, [`signer`]): the coins to that key.
fn spend(node: &Node, leg: &Value, spec: &Spec, st: &Value, secret: Option<&[u8]>, o: Option<&Value>, key: &(arcade_wallet::SecretKey, PublicKey)) -> Result<String, String> {
    let p = leg_ok(node, leg)?;
    let (sk, pk) = *key;
    let kind = leg["kind"].as_str().unwrap_or("");
    let (tx, cs) = lock_coins(node, leg, st)?;
    // Built only when the coin is held by exactly the lock computed here (2026-11-10, c3c6e1b3).
    if tx.outputs[cs[0].0.vout as usize].script_pubkey != spec.script(kind)? {
        return Err("that coin is not held by the lock as computed here: nothing sent".into());
    }
    let own = crate::coinsend::key_scripts(p, &pk)[0].1.clone();
    if kind == "dd" {
        let lock = spec.dd()?;
        // An own-key order's dollars go to its payout's DigiDollar address (`payout` names it on this chain as "dd").
        let mine = arcade_wallet::taproot::p2tr_script(&arcade_wallet::taproot::output_key(&pk).ok_or("tweak")?);
        let to = o.and_then(|o| o["own"]["payout_dd"][p.key].as_str()).and_then(|a| crate::ddsvc::dd_script_of(p, a)).unwrap_or(mine);
        let reserve = tx.outputs.iter().position(|o| o.value > 0 && o.script_pubkey == lock.script()).ok_or("no reserve")? as u32;
        let d = Utxo { outpoint: cs[0].0, value: 0, script: lock.script() };
        let r = Utxo { outpoint: OutPoint { txid: tx.txid, vout: reserve }, value: tx.outputs[reserve as usize].value, script: lock.script() };
        let secret: Option<[u8; 32]> = match secret {
            Some(s) => Some(s.try_into().map_err(|_| "a DigiDollar lock's secret is 32 bytes")?),
            None => None,
        };
        let t = ddlock::build_spend(p, &sk, &lock, &d, units(leg) as i64, &r, &to, &arcade_ledger::address::p2wpkh_script(&arcade_ledger::address::hash160(&pk.serialize())), secret.as_ref())?;
        return send(node, p, &t);
    }
    let lock = spec.htlc();
    let outs = match kind {
        "token" => {
            // A ledger send out of the lock: the lock is the sender (input 0), the receiver the last output.
            let pid: u32 = leg["asset"].as_str().and_then(|a| a.parse().ok()).ok_or("bad property")?;
            let mut d = arcade_ledger::extract::MARKER.to_vec();
            d.extend_from_slice(&[0, 0, 0, 0]);
            d.extend_from_slice(&pid.to_be_bytes());
            d.extend_from_slice(&units(leg).to_be_bytes());
            vec![(0, arcade_wallet::tx::op_return(&d)), (htlc::DUST, payout(p, o, p2pkh_of(&pk)))]
        }
        // The inscribed coin whole to this wallet (or the order's payout address) as output 0; the reserve is the fee.
        "token20" | "ord" => vec![(cs[0].1, payout(p, o, p2pkh_of(&pk)))],
        _ => {
            let mut outs = htlc::pay_out(&cs, &payout(p, o, own.clone()), htlc::SPEND_FEE)?;
            // A maker that gave an inscription pays its fee from the coins it gets, as an output of this claim.
            if secret.is_some() && o.is_some_and(|o| o["give"]["kind"] == "ord") {
                if let Some((fee, to)) = maker_fee(p, o, units(leg)) {
                    if outs[0].0 >= fee + htlc::DUST {
                        outs[0].0 -= fee;
                        outs.push((fee, to));
                    }
                }
            }
            outs
        }
    };
    let raw = match secret {
        Some(s) => htlc::claim(p, &lock, &cs, outs, s, &sk)?,
        None => htlc::refund(p, &lock, &cs, outs, &sk)?,
    };
    send_raw(node, p, &raw)
}

/// Whether the lock was spent, and the secret if a claim spent it: None unspent, Some(None) refunded.
fn spent_secret(node: &Node, leg: &Value, spec: &Spec, st: &Value) -> Result<Option<Option<Vec<u8>>>, String> {
    let p = leg_ok(node, leg)?;
    let txid = h256(&st["txid"])?;
    let op = OutPoint { txid, vout: st["vout"].as_u64().unwrap_or(0) as u32 };
    let Some(t) = spender(node, p, &op) else { return Ok(None) };
    let found = t.inputs.iter().filter(|i| i.prevout == op).find_map(|i| {
        let items: Vec<Vec<u8>> = if leg["kind"] == "dd" { i.witness.clone() } else { return htlc::secret_from_script_sig(&i.script_sig, &spec.htlc().redeem()) };
        items.into_iter().find(|w| <[u8; 32]>::from(Sha256::digest(w)) == spec.hash)
    });
    Ok(Some(found))
}

// ---------------- orders, the maker's answers ----------------

/// Whether the maker's lock of `give` takes only a 32-byte secret: everywhere but the ARCADE chain, whose script
/// has no OP_SIZE.
fn sized_on(node: &Node, give: &Value) -> bool {
    leg_ok(node, give).is_ok_and(|p| !by_height(p))
}

/// A pair the maker can't be stranded in: the taker makes the secret, and a lock of the maker's that takes any
/// length (the ARCADE chain's) can't be paired with one of the taker's that takes 32 bytes only (DigiDollars).
fn pair_ok(node: &Node, give: &Value, get_leg: &Value) -> Result<(), String> {
    if !sized_on(node, give) && get_leg["kind"] == "dd" {
        return Err("an ARCADE coin leg can't be swapped for DigiDollars yet".into());
    }
    confirmable(node, give)?;
    confirmable(node, get_leg)
}

/// Place an order: give `give`, get `get` (whole, one fill). `opts` (all optional): `key` {chain: 32-byte secret hex}
/// and `payout` {chain: address}, an order run with its own key for somebody else, its result paid to them (the
/// website's delegated orders; kept here, never in what is shared); `maker_fee` {bps (at most 500), to: {chain:
/// address}}, the maker's operator's fee, paid in what the maker gives.
pub fn place(node: &Node, give: &Value, get_leg: &Value, opts: &Value) -> Result<Value, String> {
    let gp = leg_ok(node, give)?;
    let wp = leg_ok(node, get_leg)?;
    pair_ok(node, give, get_leg)?;
    let id = arcade_mesh::random::hex(8);
    // The account placing it: its keys and coins run the order whichever account is active later.
    let mut o = json!({"id": id, "give": give, "get": get_leg, "status": "open", "created": arcade_app::now(), "node": node.id_hex(),
                       "account": node.app.active_account()});
    // Kept with each chain by its key, whatever name the order came with.
    canon_legs(&mut o);
    if opts["key"].is_object() || opts["payout"].is_object() {
        for p in [gp, wp] {
            let k = opts["key"][p.key].as_str().ok_or_else(|| format!("an order with its own key names its key on {}", p.name))?;
            let b = hex::decode(k).map_err(|_| "an order's key is 32 bytes of hex")?;
            arcade_wallet::SecretKey::from_slice(&b).map_err(|_| "an order's key is not a key")?;
            let a = opts["payout"][p.key].as_str().ok_or_else(|| format!("an order with its own key names where its {} goes", p.name))?;
            arcade_ledger::address::script_for_address(p, a).ok_or_else(|| format!("{a} is not a {} address", p.name))?;
        }
        // DigiDollars come out to a DigiDollar address: `payout_dd` {chain: DD... / TD...} for a DD leg's chain.
        for (leg, p) in [(give, gp), (get_leg, wp)] {
            if leg["kind"] == "dd" {
                let a = opts["payout_dd"][p.key].as_str().ok_or("an order with its own key that gets or gives DigiDollars names its DigiDollar payout address")?;
                crate::ddsvc::dd_script_of(p, a).ok_or_else(|| format!("{a} is not a DigiDollar address on {}", p.name))?;
            }
        }
        o["own"] = json!({"keys": opts["key"], "payout": opts["payout"], "payout_dd": opts["payout_dd"]});
    }
    if opts["maker_fee"].is_object() {
        let bps = opts["maker_fee"]["bps"].as_u64().ok_or("a maker fee names its bps")?;
        if bps > MAX_FEE_BPS {
            return Err("a maker fee is at most 5%".into());
        }
        // Paid on the chain of what the maker gives (for an inscription given, of the coins it gets).
        let fp = if give["kind"] == "ord" { wp } else { gp };
        let a = opts["maker_fee"]["to"][fp.key].as_str().ok_or_else(|| format!("a maker fee names its address on {}", fp.name))?;
        // A DigiDollar fee goes to a DigiDollar address, which the DD send checks; any other to a coin address.
        if give["kind"] != "dd" {
            arcade_ledger::address::script_for_address(fp, a).ok_or_else(|| format!("{a} is not a {} address", fp.name))?;
        }
        o["maker_fee"] = json!({"bps": bps, "to": opts["maker_fee"]["to"]});
    }
    put(node, "sorder", &o);
    Ok(public_order(&o))
}

/// Give back an open order placed with its own key (`swap.release`): cancelled, and its key and payout addresses
/// forgotten here. One being taken is refused: its swap finishes or refunds by itself.
pub fn release(node: &Node, id: &str) -> Result<Value, String> {
    let mut o = get(node, "sorder", id).ok_or("no such order here")?;
    if o["status"] != "open" {
        return Err(format!("that order is {}: a running swap finishes or refunds by itself", o["status"].as_str().unwrap_or("closed")));
    }
    // What the order's key holds goes back to the payout addresses first (the website's visitor gave it to this key to
    // place the order): an inscription given, whole and first; an arcade token given, by a ledger send; then the coins.
    let mut back = Vec::new();
    if o["own"].is_object() {
        o["status"] = json!("releasing");
        put(node, "sorder", &o);
        for leg in [o["give"].clone(), o["get"].clone()] {
            let p = leg_ok(node, &leg)?;
            if back.iter().any(|b: &Value| b["chain"] == p.key) {
                continue;
            }
            match sweep(node, p, &o, if leg == o["give"] { Some(&leg) } else { None }) {
                Ok(txids) => back.push(json!({"chain": p.key, "txids": txids})),
                Err(e) => back.push(json!({"chain": p.key, "error": e})),
            }
        }
    }
    o["status"] = json!("released");
    o["returned"] = json!(back);
    if let Some(m) = o.as_object_mut() {
        m.remove("own");
    }
    put(node, "sorder", &o);
    Ok(o)
}

/// Everything an own-key order's key holds on `p` back to its payout address there: `give` is the order's own leg on
/// that chain (its inscription or arcade token first), then every coin. The txids sent.
fn sweep(node: &Node, p: &'static ChainParams, o: &Value, give: Option<&Value>) -> Result<Vec<String>, String> {
    let (sk, pk) = key_of(node, p, Some(o), &order_account(node, o))?;
    let own = crate::coinsend::key_scripts(p, &pk)[0].1.clone();
    let to = payout(p, Some(o), own.clone());
    if to == own {
        return Err("no payout address to send it back to".into());
    }
    let mut sent = Vec::new();
    match give.and_then(|g| g["kind"].as_str()) {
        Some("ord") => {
            let id = give.unwrap()["asset"].as_str().unwrap_or("");
            if let Some((at, 0)) = arcade_ledger::ord::location(&node.chain, p, id) {
                if let Some(coin) = node.chain.coin(p, &at).ok().flatten().filter(|c| c.script == own || c.script == p2pkh_of(&pk)) {
                    let first = vec![Utxo { outpoint: at, value: coin.value, script: coin.script.clone() }];
                    let tx = build(node, p, &sk, first, coins(node, p, &pk), vec![TxOutSpec { value: coin.value, script: to.clone() }], &to)?;
                    sent.push(send(node, p, &tx)?);
                    return Ok(sent); // its fee coins went with it, change to the payout address
                }
            }
        }
        Some("dd") => {
            // The dollars back to the payout's DigiDollar address by a DD send from the key (its DGB pays the fee and its
            // change comes back to the key, swept with the coins after).
            let a = o["own"]["payout_dd"][p.key].as_str().ok_or("no DigiDollar payout address")?;
            let cents: i64 = crate::ddsvc::coins_of(node, Some(p.key), &hex::encode(pk.serialize()))?["cents"].as_i64().unwrap_or(0);
            if cents > 0 {
                let v = crate::ddsvc::send_with(node, Some(p.key), a, cents, true, Some(&sk))?;
                sent.push(v["txid"].as_str().unwrap_or("").to_string());
            }
            // Then the key's DGB (what the DD send didn't spend) with the coins below.
        }
        Some("token20") => {
            // The tokens back by a token send from the key (a transfer note, then its coin: tokensend's job, which pays
            // its fees from the key's coins, so those stay with it here).
            let g = give.unwrap();
            let tick = g["asset"].as_str().unwrap_or("");
            let addr = arcade_ledger::address::address_of(&own, p).ok_or("no address")?;
            let held = crate::explorer::handle(node, "explorer.address", &json!({"chain": p.key, "address": addr})).map_err(|e| e.to_string())?;
            let dec = crate::explorer::handle(node, "explorer.token", &json!({"chain": p.key, "tick": tick})).ok().and_then(|v| v["dec"].as_u64()).unwrap_or(18) as u32;
            let row = held["tokens"].as_array().into_iter().flatten().find(|t| t["tick"].as_str().is_some_and(|x| x.eq_ignore_ascii_case(tick)));
            let big = |k: &str| row.and_then(|t| t[k].as_str()).and_then(|x| x.parse::<u128>().ok()).unwrap_or(0);
            let whole = big("overall").saturating_sub(big("transferable")) / 10u128.pow(dec);
            if whole > 0 {
                let a = arcade_ledger::address::address_of(&to, p).ok_or("bad payout")?;
                let v = crate::tokensend::t20_send_with(node, p.key, tick, &whole.to_string(), &a, true, o["own"]["keys"][p.key].as_str())?;
                sent.push(v["note_tx"].as_str().unwrap_or("").to_string());
            }
            return Ok(sent);
        }
        Some("token") => {
            let g = give.unwrap();
            let pid: u32 = g["asset"].as_str().and_then(|a| a.parse().ok()).ok_or("bad property")?;
            let addr = arcade_ledger::address::address_of(&own, p).ok_or("no address")?;
            let held = led::holdings(&node.chain, p.chain_no, &addr).into_iter().find(|(x, _)| *x == pid).map(|x| x.1).unwrap_or(0);
            if held > 0 {
                let mut payload = vec![0, 0, 0, 0];
                payload.extend_from_slice(&pid.to_be_bytes());
                payload.extend_from_slice(&held.to_be_bytes());
                let t = arcade_wallet::tx::build_arcade(p, &payload, vec![TxOutSpec { value: LEDGER_COIN, script: to.clone() }], &sk, &coins(node, p, &pk), crate::claims::FEE_PER_KB, crate::claims::DUST, now()).map_err(|e| e.to_string())?;
                sent.push(send(node, p, &t)?);
            }
        }
        _ => {}
    }
    let left = coins(node, p, &pk);
    if !left.is_empty() {
        let tx = build(node, p, &sk, left, vec![], vec![], &to)?;
        sent.push(send(node, p, &tx)?);
    }
    Ok(sent)
}

/// Take back an open order of this arcade's (a running swap finishes or refunds by itself).
pub fn cancel(node: &Node, id: &str) -> Result<Value, String> {
    let mut o = get(node, "sorder", id).filter(|o| o["status"] == "open").ok_or("that is not an open swap order of this arcade's")?;
    o["status"] = json!("cancelled");
    put(node, "sorder", &o);
    Ok(public_order(&o))
}

/// This arcade's own open orders.
pub fn own_orders(node: &Node) -> Vec<Value> {
    list(node, "sorder").into_iter().filter(|o| o["status"] == "open").map(|o| public_order(&o)).collect()
}

/// Linked arcades' open orders as each last answered: peer -> (when, orders).
static PEER_BOOKS: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, (u32, Vec<Value>)>>> =
    std::sync::LazyLock::new(Default::default);
/// Peers asked now (an ask waits up to 30 s): never two asks of one peer at once.
static ASKING: std::sync::LazyLock<std::sync::Mutex<std::collections::HashSet<String>>> = std::sync::LazyLock::new(Default::default);
/// How long the book waits for linked arcades before answering with what it has.
const BOOK_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
/// A peer's last answer is shown this long while it doesn't answer again.
const BOOK_KEEP: u32 = 600;

/// The open swap orders here and on every linked arcade. Linked arcades are asked all at once and
/// the book answers within BOOK_WAIT with each one's newest answer: one asked in turn, each up to
/// 30 s, held a page's book for minutes on an arcade with a dozen links (some never answer).
pub fn book(node: &Node) -> Vec<Value> {
    let mut all: Vec<Value> = list(node, "sorder").into_iter().filter(|o| o["status"] == "open").map(|o| public_order(&o)).collect();
    let peers = crate::shares::linked_ids(node);
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    let mut asked = 0;
    for peer in &peers {
        if !ASKING.lock().unwrap().insert(peer.clone()) {
            continue;
        }
        asked += 1;
        let (peer, tx, net) = (peer.clone(), tx.clone(), node.params.net);
        std::thread::spawn(move || {
            let q = json!({"what": "mk", "net": net, "arg": json!({"op": "sbook", "chain": "dgbtest"}).to_string()});
            if let Ok(a) = crate::shares::ask_linked(&peer, q) {
                PEER_BOOKS.lock().unwrap().insert(peer.clone(), (now(), a["orders"].as_array().cloned().unwrap_or_default()));
            }
            ASKING.lock().unwrap().remove(&peer);
            let _ = tx.send(());
        });
    }
    drop(tx);
    let until = std::time::Instant::now() + BOOK_WAIT;
    for _ in 0..asked {
        if rx.recv_timeout(until.saturating_duration_since(std::time::Instant::now())).is_err() {
            break;
        }
    }
    let books = PEER_BOOKS.lock().unwrap();
    for peer in &peers {
        if let Some((_, orders)) = books.get(peer).filter(|(at, _)| now().saturating_sub(*at) <= BOOK_KEEP) {
            for o in orders {
                let mut o = o.clone();
                o["node"] = json!(peer);
                // An older arcade's order may name a chain by its mesh name.
                canon_legs(&mut o);
                all.push(o);
            }
        }
    }
    all
}

fn keys_for(node: &Node, legs: &[&Value], o: Option<&Value>, account: &str) -> Result<Value, String> {
    let mut m = serde_json::Map::new();
    for leg in legs {
        let p = leg_ok(node, leg)?;
        m.insert(p.key.to_string(), json!(hex::encode(key_of(node, p, o, account)?.1.serialize())));
    }
    Ok(Value::Object(m))
}

/// The arcade a taker name belongs to: the name itself, or the part before ':' (a website's visitor, `outside`).
fn peer_of(who: &str) -> &str {
    who.split(':').next().unwrap_or("")
}

/// Hold open order `o` for taker `who`, asked by arcade `peer` (from the link, never the question's word): one taker
/// at a time, a hold never stretched by asking again, at most HOLDS_PER_PEER orders per arcade at once, and an arcade
/// whose hold ran out without a lock waits HOLD_SECS before holding that order again.
fn hold(node: &Node, o: &mut Value, peer: &str, who: &str) -> Result<(), String> {
    if peer.is_empty() || peer_of(who) != peer {
        return Err("a taker asks for itself (or for its own website's visitors)".into());
    }
    let t = now();
    let held = o["held_by"].as_str().unwrap_or("").to_string();
    let until = o["held_until"].as_u64().unwrap_or(0) as u32;
    if until > t {
        if held == who {
            return Ok(()); // the same taker asking again: the hold it has, no longer
        }
        if !held.is_empty() {
            return Err("somebody is taking that order right now; it comes back if they don't lock in time".into());
        }
    } else if !held.is_empty() && peer_of(&held) == peer && t < until + HOLD_SECS {
        return Err(format!("your hold on that order ran out without a lock; it is open to others for {} more minutes", (until + HOLD_SECS - t).div_ceil(60)));
    }
    let id = o["id"].as_str().unwrap_or("");
    let holding = list(node, "sorder").iter().filter(|x| x["id"] != id && x["status"] == "open" && x["held_until"].as_u64().unwrap_or(0) as u32 > t && x["held_by"].as_str().is_some_and(|h| peer_of(h) == peer)).count();
    if holding >= HOLDS_PER_PEER {
        return Err(format!("your arcade holds {holding} orders here already: lock one or let it go first"));
    }
    o["held_by"] = json!(who);
    o["held_until"] = json!(t + HOLD_SECS);
    Ok(())
}

/// A swap question from arcade `peer` (the link's node id; this arcade's own for its own asks).
pub fn answer(node: &Node, peer: &str, q: &Value) -> Result<Value, String> {
    match q["op"].as_str().unwrap_or("") {
        "sbook" => Ok(json!({"orders": list(node, "sorder").into_iter().filter(|o| o["status"] == "open").map(|o| {
            let mut o = public_order(&o);
            o["taking"] = json!(o["held_until"].as_u64().unwrap_or(0) as u32 > now());
            o
        }).collect::<Vec<_>>()})),
        "squote" => {
            // One taker at a time (Martin, 2026-11-11: an order must never be filled by two at once): the order is
            // held for the taker asking now, before anybody locks anything; another taker is told it is being taken.
            let mut o = get(node, "sorder", q["order"].as_str().unwrap_or("")).filter(|o| o["status"] == "open").ok_or("that order is not open")?;
            hold(node, &mut o, peer, q["taker"].as_str().unwrap_or(""))?;
            put(node, "sorder", &o);
            pair_ok(node, &o["give"], &o["get"])?;
            // "sized": this maker's lock takes only a 32-byte secret (an older taker doesn't know to look for it:
            // it never finds this side's lock, and both sides refund).
            Ok(json!({"order": public_order(&o), "maker_keys": keys_for(node, &[&o["give"], &o["get"]], Some(&o), &order_account(node, &o))?, "maker_secs": maker_secs(), "sized": sized_on(node, &o["give"])}))
        }
        "slocked" => {
            // A taker tells of its own lock only (or its own website's visitors').
            if peer_of(q["taker"].as_str().unwrap_or("")) != peer {
                return Err("a taker tells of its own lock only".into());
            }
            maker_begin(node, q)
        }
        "sstatus" => {
            let s = get(node, "swap", q["hash"].as_str().unwrap_or("")).ok_or("no such swap here")?;
            Ok(json!({"step": s["step"], "maker_lock": s["maker_lock"], "last": s["last"]}))
        }
        _ => Err("unknown swap question".into()),
    }
}

/// Both locks' terms for a swap record.
fn specs(s: &Value) -> Result<(Spec, Spec), String> {
    let hash: [u8; 32] = hex::decode(s["id"].as_str().unwrap_or("")).ok().and_then(|b| b.try_into().ok()).ok_or("bad id")?;
    let k = |who: &str, leg: &str| -> Result<Vec<u8>, String> {
        let ch = s["order"][leg]["chain"].as_str().unwrap_or("");
        hex::decode(s[who][ch].as_str().unwrap_or("")).map_err(|_| format!("no {who} key on {ch}"))
    };
    // The taker locks the order's `get`: the maker claims it. The maker locks `give`: the taker claims it.
    // A lock on the ARCADE chain ends at its height, any other at its clock time.
    let end = |leg: &str, clock: &str, height: &str| {
        let h = crate::chains::find(s["order"][leg]["chain"].as_str().unwrap_or("")).is_some_and(by_height);
        s[if h { height } else { clock }].as_u64().unwrap_or(0) as u32
    };
    let taker = Spec { hash, claimer: k("maker_keys", "get")?, refunder: k("taker_keys", "get")?, time: end("get", "taker_time", "taker_height"), sized: false };
    let maker = Spec { hash, claimer: k("taker_keys", "give")?, refunder: k("maker_keys", "give")?, time: end("give", "maker_time", "maker_height"), sized: s["sized"] == true };
    Ok((taker, maker))
}

/// The taker's lock is there: check it on this arcade's copy, then start locking the maker's leg.
fn maker_begin(node: &Node, q: &Value) -> Result<Value, String> {
    let id = q["hash"].as_str().unwrap_or("");
    if let Some(s) = get(node, "swap", id) {
        return Ok(json!({"step": s["step"]}));
    }
    let mut o = get(node, "sorder", q["order"].as_str().unwrap_or("")).filter(|o| o["status"] == "open").ok_or("that order is not open")?;
    // Only the taker it is held for; a hold that lapsed is taken back only if nobody else holds it now.
    let held = o["held_by"].as_str().unwrap_or("");
    if held != q["taker"].as_str().unwrap_or("") && (!held.is_empty() && o["held_until"].as_u64().unwrap_or(0) as u32 > now()) {
        return Err("that order is held for somebody else".into());
    }
    pair_ok(node, &o["give"], &o["get"])?;
    let taker_time = q["taker_time"].as_u64().ok_or("give the lock time")? as u32;
    if taker_time < now() + maker_secs() * 3 / 2 {
        return Err("that lock ends too soon".into());
    }
    // The swap belongs to the order's account: every later lock, claim and refund signs with its keys.
    let account = order_account(node, &o);
    let mut s = json!({"id": id, "role": "maker", "order": public_order(&o), "peer": q["taker"], "maker_keys": keys_for(node, &[&o["give"], &o["get"]], Some(&o), &account)?, "account": account,
                       "taker_keys": q["taker_keys"], "taker_time": taker_time, "maker_time": now() + maker_secs(),
                       "taker_lock": q["taker_lock"], "maker_lock": {"step": "start"}, "step": "locking", "at": arcade_app::now(),
                       "sized": sized_on(node, &o["give"])});
    // On the ARCADE chain: the taker's height must last 3 times the order's time even with blocks twice as fast as
    // lately; this side's own lock ends at the height of 1 time (see by_height).
    let gp = leg_ok(node, &o["get"])?;
    if by_height(gp) {
        let h = q["taker_height"].as_u64().ok_or("give the lock height")? as u32;
        if h < tip_height(node, gp) + (3 * maker_secs()).div_ceil(block_secs(node, gp)) {
            return Err("that lock ends too soon".into());
        }
        s["taker_height"] = json!(h);
    }
    let gv = leg_ok(node, &o["give"])?;
    if by_height(gv) {
        s["maker_height"] = json!(height_for(node, gv, maker_secs()));
    }
    let (tspec, _) = specs(&s)?;
    match seen(node, &o["get"], &tspec, &s["taker_lock"]) {
        Ok(true) => {}
        Ok(false) => return Err("wait: the taker's lock is not mined (or read) here yet".into()),
        Err(e) => return Err(e),
    }
    o["status"] = json!("taken");
    put(node, "sorder", &o);
    s["order"] = public_order(&o);
    put(node, "swap", &s);
    Ok(json!({"step": "locking"}))
}

// ---------------- the taker ----------------

fn ask(node: &Node, peer: &str, q: Value) -> Result<Value, String> {
    if peer == node.id_hex() {
        return answer(node, peer, &q);
    }
    crate::shares::ask_linked(peer, json!({"what": "mk", "net": node.params.net, "arg": q.to_string()}))
}

/// Take an order whole: make the secret, start locking the order's `get` leg; the rest runs by itself.
/// `shown`: the order's legs as the person saw them ({give, get}); the maker's terms now must be exactly those, or
/// nothing is locked (a maker can't swap in other terms between the card and the Approve).
pub fn take(node: &Node, maker: &str, order: &str, shown: Option<&Value>) -> Result<Value, String> {
    let quote = ask(node, maker, json!({"op": "squote", "chain": "dgbtest", "order": order, "taker": node.id_hex()}))?;
    let o = quote["order"].clone();
    if let Some(sh) = shown {
        if !same_terms(&o, sh) {
            return Err("that order's terms changed since you saw them: nothing was locked".into());
        }
    }
    leg_ok(node, &o["give"])?;
    leg_ok(node, &o["get"])?;
    confirmable(node, &o["give"])?;
    confirmable(node, &o["get"])?;
    let secret: [u8; 32] = arcade_mesh::random::bytes();
    let id = hex::encode(Sha256::digest(secret));
    let secs = quote["maker_secs"].as_u64().unwrap_or(4 * 3600) as u32;
    let f = taker_factor(&o);
    // The swap belongs to the account taking it now: every later lock, claim and refund signs with its keys.
    let account = node.app.active_account();
    let mut s = json!({"id": id, "role": "taker", "order": o, "peer": maker, "secret": hex::encode(secret), "maker_keys": quote["maker_keys"],
                   "taker_keys": keys_for(node, &[&o["give"], &o["get"]], None, &account)?, "account": account, "taker_time": now() + f * secs,
                   "taker_lock": {"step": "start"}, "step": "locking", "at": arcade_app::now(), "sized": quote["sized"] == true});
    let gp = leg_ok(node, &o["get"])?;
    if by_height(gp) {
        s["taker_height"] = json!(height_for(node, gp, f * secs));
    }
    put(node, "swap", &s);
    Ok(public(&s))
}

/// A taker outside this arcade (the website's visitor, whose browser holds the keys and signs every lock, claim and
/// refund; docs/MARKETPLACE-PLAN.md in arcade-site): this arcade only carries the questions to the maker's arcade,
/// under a taker name of its own plus the visitor's reference, so one website's visitors are held for one at a time
/// like any taker. `op`: quote (the order held for this taker: its terms, the maker's keys and time, sized), told
/// (the taker's lock is on the chain: the maker checks it and locks), status (the maker's side of the swap).
pub fn outside(node: &Node, op: &str, req: &Value) -> Result<Value, String> {
    let maker = req["node"].as_str().filter(|s| !s.is_empty()).ok_or("node required (the maker's arcade)")?;
    let r = req["taker"].as_str().unwrap_or("");
    if r.is_empty() || r.len() > 64 || !r.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("taker required: the visitor's reference (letters, digits, - and _, up to 64)".into());
    }
    let taker = format!("{}:{r}", node.id_hex());
    match op {
        "quote" => ask(node, maker, json!({"op": "squote", "chain": "dgbtest", "order": req["order"], "taker": taker})),
        "told" => {
            let mut q = json!({"op": "slocked", "chain": "dgbtest", "order": req["order"], "hash": req["hash"], "taker": taker,
                               "taker_keys": req["taker_keys"], "taker_time": req["taker_time"], "taker_lock": req["taker_lock"]});
            if !req["taker_height"].is_null() {
                q["taker_height"] = req["taker_height"].clone();
            }
            ask(node, maker, q)
        }
        "status" => ask(node, maker, json!({"op": "sstatus", "chain": "dgbtest", "hash": req["hash"]})),
        _ => Err("unknown outside-taker question".into()),
    }
}

/// Whether an order's legs are exactly the ones shown (chain, kind, asset and amount of each).
pub fn same_terms(o: &Value, shown: &Value) -> bool {
    let leg = |v: &Value| ["chain", "kind", "asset", "amount"].map(|k| v[k].clone());
    shown["give"].is_object() && shown["get"].is_object() && leg(&o["give"]) == leg(&shown["give"]) && leg(&o["get"]) == leg(&shown["get"])
}

pub fn public(s: &Value) -> Value {
    let mut v = s.clone();
    if let Some(m) = v.as_object_mut() {
        m.remove("secret");
    }
    v
}

pub fn swaps(node: &Node) -> Vec<Value> {
    list(node, "swap").iter().map(public).collect()
}

/// How many swaps still running and open orders belong to `account`.
pub(crate) fn running_for(node: &Node, account: &str) -> usize {
    let swaps = list(node, "swap").into_iter().filter(|s| s["account"] == account && !matches!(s["step"].as_str(), Some("claimed" | "refunded" | "failed"))).count();
    swaps + list(node, "sorder").into_iter().filter(|o| o["account"] == account && o["status"] == "open").count()
}

fn step(node: &Node, s: &mut Value) -> Result<(), String> {
    let (tspec, mspec) = specs(s)?;
    let (give, get_leg) = (s["order"]["give"].clone(), s["order"]["get"].clone());
    let t = now();
    if s["role"] == "taker" {
        // The taker's own lock comes back after its time if the swap never finished.
        if s["step"] != "claimed" && s["taker_lock"]["step"] == "locked" && ended(node, leg_ok(node, &get_leg)?, tspec.time) {
            if spent_secret(node, &get_leg, &tspec, &s["taker_lock"])?.is_none() {
                let k = signer(node, leg_ok(node, &get_leg)?, s, None)?;
                s["refund_tx"] = json!(spend(node, &get_leg, &tspec, &s["taker_lock"], None, None, &k)?);
                s["step"] = json!("refunded");
            }
            return Ok(());
        }
        match s["step"].as_str().unwrap_or("") {
            "locking" => {
                let k = signer(node, leg_ok(node, &get_leg)?, s, None)?;
                let mut st = s["taker_lock"].clone();
                lock_step(node, &get_leg, &tspec, &mut st, None, &k)?;
                s["taker_lock"] = st;
                if s["taker_lock"]["step"] == "locked" {
                    s["step"] = json!("telling");
                }
            }
            "telling" => {
                if !seen(node, &get_leg, &tspec, &s["taker_lock"]).unwrap_or(false) {
                    return Ok(());
                }
                let q = json!({"op": "slocked", "chain": "dgbtest", "order": s["order"]["id"], "hash": s["id"], "taker": node.id_hex(),
                               "taker_keys": s["taker_keys"], "taker_time": s["taker_time"], "taker_height": s["taker_height"], "taker_lock": s["taker_lock"]});
                match ask(node, s["peer"].as_str().unwrap_or(""), q) {
                    Ok(_) => s["step"] = json!("waiting"),
                    Err(e) => s["last"] = json!(e),
                }
            }
            "waiting" => {
                let a = ask(node, s["peer"].as_str().unwrap_or(""), json!({"op": "sstatus", "chain": "dgbtest", "hash": s["id"]}))?;
                s["maker_lock"] = a["maker_lock"].clone();
                if s["maker_lock"]["step"] != "locked" {
                    return Ok(());
                }
                // The maker's lock time comes from its own record here; never claim too close to it.
                let mut mspec = mspec;
                mspec.time = a["maker_lock"]["time"].as_u64().unwrap_or(0) as u32;
                let gv = leg_ok(node, &give)?;
                s[if by_height(gv) { "maker_height" } else { "maker_time" }] = json!(mspec.time);
                if !claim_time_left(by_height(gv), mspec.time, if by_height(gv) { tip_height(node, gv) } else { t }, maker_secs(), block_secs(node, gv)) {
                    s["last"] = json!("the maker's lock ends too soon to claim safely: not claimed (this side's own lock refunds after its time)");
                    return Ok(());
                }
                match seen(node, &give, &mspec, &s["maker_lock"]) {
                    Ok(true) => {
                        let secret = hex::decode(s["secret"].as_str().unwrap_or("")).map_err(|_| "no secret")?;
                        let k = signer(node, gv, s, None)?;
                        s["claim_tx"] = json!(spend(node, &give, &mspec, &s["maker_lock"], Some(&secret), None, &k)?);
                        s["step"] = json!("claimed");
                    }
                    Ok(false) => {}
                    Err(e) => s["last"] = json!(format!("the maker's lock is wrong, not claimed: {e}")),
                }
            }
            _ => {}
        }
        return Ok(());
    }
    // The maker: with the order's own key and payout addresses when it has them (own_order).
    let order = own_order(node, s);
    let o = order.as_ref();
    match s["step"].as_str().unwrap_or("") {
        // Its lock never started and its time is up: nothing of this wallet's is out; the taker refunds its own.
        "locking" if s["maker_lock"]["step"] == "start" && t > s["maker_time"].as_u64().unwrap_or(0) as u32 => {
            s["step"] = json!("failed");
            s["last"] = json!("this side never locked before its time; nothing was sent");
        }
        "locking" => {
            let k = signer(node, leg_ok(node, &give)?, s, o)?;
            let mut st = s["maker_lock"].clone();
            lock_step(node, &give, &mspec, &mut st, o, &k)?;
            if st["step"] == "locked" {
                // The lock's end as its script has it: a height on the ARCADE chain, else a clock time.
                st["time"] = json!(mspec.time);
                s["step"] = json!("locked");
            }
            s["maker_lock"] = st;
        }
        "locked" => match spent_secret(node, &give, &mspec, &s["maker_lock"])? {
            Some(Some(secret)) => {
                let k = signer(node, leg_ok(node, &get_leg)?, s, o)?;
                s["claim_tx"] = json!(spend(node, &get_leg, &tspec, &s["taker_lock"], Some(&secret), o, &k)?);
                s["step"] = json!("claimed");
            }
            Some(None) => s["step"] = json!("refunded"),
            None if ended(node, leg_ok(node, &give)?, mspec.time) => {
                let k = signer(node, leg_ok(node, &give)?, s, o)?;
                s["refund_tx"] = json!(spend(node, &give, &mspec, &s["maker_lock"], None, o, &k)?);
                s["step"] = json!("refunded");
            }
            None => {}
        },
        _ => {}
    }
    Ok(())
}

/// The order a maker's swap runs, with its own keys and payout addresses (kept in this arcade's order record only; the
/// swap record carries the order as the taker saw it).
fn own_order(node: &Node, s: &Value) -> Option<Value> {
    get(node, "sorder", s["order"]["id"].as_str()?)
}

pub fn start(node: Arc<Node>) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_secs(10));
        relay(&node);
        for mut s in list(&node, "swap") {
            if matches!(s["step"].as_str(), Some("claimed" | "refunded" | "failed")) {
                continue;
            }
            let before = s.clone();
            if let Err(e) = step(&node, &mut s) {
                s["last"] = json!(e);
            }
            if s != before {
                put(&node, "swap", &s);
            }
        }
    });
}

#[cfg(test)]
mod arcade_lock_tests {
    use arcade_chain::{Hash256, OutPoint, Tx};
    use arcade_wallet::htlc::{self, Htlc};
    use arcade_wallet::secp256k1::{PublicKey, SecretKey, SECP256K1};
    use sha2::{Digest, Sha256};

    /// The hash lock as swaps make it, spent on the ARCADE chain: its claim and its refund (a height lock) pass
    /// ARCADE's own script rules (arcade-arc script.rs), and a refund before its height doesn't.
    #[test]
    fn hash_lock_spends_by_arcade_rules() {
        let p = arcade_arc::store::chain_params(&arcade_arc::params::TESTNET, &Hash256::ZERO);
        let (a, b) = (SecretKey::from_slice(&[3u8; 32]).unwrap(), SecretKey::from_slice(&[4u8; 32]).unwrap());
        let pk = |k: &SecretKey| PublicKey::from_secret_key(SECP256K1, k).serialize().to_vec();
        let secret = [9u8; 32];
        let lock = Htlc { hash: Sha256::digest(secret).into(), claimer: pk(&a), refunder: pk(&b), time: 1_000, sized: false };
        let coin = [(OutPoint { txid: Hash256([7; 32]), vout: 0 }, 50_000_000u64)];
        let to = arcade_ledger::address::p2pkh_script(&arcade_ledger::address::hash160(&pk(&a)));
        let outs = htlc::pay_out(&coin, &to, htlc::SPEND_FEE).unwrap();
        let check = |raw: &[u8]| {
            let tx = Tx::parse_with(raw, &p.format).unwrap();
            let mut n = 0;
            (arcade_arc::script::verify_input(&tx, 0, &lock.script(), &mut n), tx)
        };
        let (ok, _) = check(&htlc::claim(p, &lock, &coin, outs.clone(), &secret, &a).unwrap());
        assert!(ok.is_ok(), "claim: {ok:?}");
        let (ok, tx) = check(&htlc::refund(p, &lock, &coin, outs.clone(), &b).unwrap());
        assert!(ok.is_ok(), "refund: {ok:?}");
        assert!(!arcade_arc::coins::unlocked(&tx, 1_000) && arcade_arc::coins::unlocked(&tx, 1_001));
        let early = Htlc { time: 999, ..lock.clone() };
        let (ok, _) = check(&htlc::refund(p, &early, &coin, outs, &b).unwrap());
        assert!(ok.is_err(), "a refund signed for an earlier height must not spend this lock");
    }
}

/// The other side's lock counts as mined only under work this arcade checked; a chain where nothing is checked any
/// more (Kotia's staked blocks) is refused as a leg before anything is locked.
#[cfg(test)]
#[test]
fn a_lock_counts_only_under_checked_work() {
    use crate::sitesvc::tests::{setup, setup_with};
    let t = setup_with(|p| {
        p.max_easier = false;
        p.digishield_from = None;
    });
    assert_eq!(depth(&t.node, t.p, &t.spend.txid), 1);
    assert_eq!(money_depth(&t.node, t.p, &t.spend.txid), 0, "in a block, under no checked work");
    let why = confirmable_on(&t.node, t.p).unwrap_err();
    assert!(why.contains("no work this arcade can check") && !why.contains("stake"), "{why}");

    let t = setup();
    assert_eq!(money_depth(&t.node, t.p, &t.spend.txid), 1);
    assert!(confirmable_on(&t.node, t.p).is_ok());
    assert!(confirmable(&t.node, &json!({"chain": "peptest"})).is_ok());
    assert!(confirmable(&t.node, &json!({"chain": "kot"})).is_ok(), "not installed: refused elsewhere, not here");
    // A staked chain (Kotia after its last PoW height): refused, saying why.
    let t = setup_with(|p| {
        p.max_easier = false;
        p.digishield_from = None;
        p.pow = arcade_chain::pow::PowRule::PowThenStake { algo: arcade_chain::pow::PowAlgo::Scrypt, last_pow_height: 0 };
    });
    let why = confirmable_on(&t.node, t.p).unwrap_err();
    assert!(why.contains("proof of stake"), "{why}");
}

#[cfg(test)]
#[test]
fn a_take_goes_ahead_only_on_the_terms_shown() {
    let shown = json!({"give": {"chain": "dgbtest", "kind": "coin", "asset": null, "amount": "100"}, "get": {"chain": "peptest", "kind": "coin", "asset": null, "amount": "5"}});
    let mut o = shown.clone();
    o["id"] = json!("x");
    o["status"] = json!("open");
    assert!(same_terms(&o, &shown));
    o["get"]["amount"] = json!("5000");
    assert!(!same_terms(&o, &shown), "a bigger price");
    let mut o2 = shown.clone();
    o2["give"]["chain"] = json!("dgb");
    assert!(!same_terms(&o2, &shown), "another chain");
    assert!(!same_terms(&shown, &json!({"order": "x"})), "nothing shown matches nothing");
}

#[cfg(test)]
mod own_key_tests {
    use super::*;

    fn legs(t: &crate::sitesvc::tests::T) -> (Value, Value) {
        let k = t.p.key;
        (json!({"chain": k, "kind": "coin", "asset": "", "amount": "500000000"}), json!({"chain": k, "kind": "coin", "asset": "", "amount": "100000000"}))
    }

    #[test]
    fn an_order_with_its_own_key_keeps_it_here_only() {
        let t = crate::sitesvc::tests::setup();
        let (give, get_leg) = legs(&t);
        let sk = [9u8; 32];
        let pay_to = arcade_ledger::address::base58check(t.p.p2pkh_version, &[4u8; 20]);
        let k = t.p.key;
        let opts = json!({"key": {k: hex::encode(sk)}, "payout": {k: pay_to}, "maker_fee": {"bps": 100, "to": {k: pay_to}}});
        let shown = place(&t.node, &give, &get_leg, &opts).unwrap();
        assert!(shown.get("own").is_none(), "what place answers carries no key");
        let id = shown["id"].as_str().unwrap();
        let kept = get(&t.node, "sorder", id).unwrap();
        assert_eq!(kept["own"]["keys"][k], hex::encode(sk), "kept here");
        assert!(own_orders(&t.node).iter().all(|o| o.get("own").is_none()));
        assert!(book(&t.node).iter().all(|o| o.get("own").is_none()));
        let sbook = answer(&t.node, "x", &json!({"op": "sbook"})).unwrap();
        assert!(!sbook.to_string().contains(&hex::encode(sk)), "the shared book never carries the key");
        // The order's key is the maker's key it shows, not this wallet's.
        let (_, pk) = key_of(&t.node, t.p, Some(&kept), "").unwrap();
        assert_eq!(pk, PublicKey::from_secret_key(SECP256K1, &arcade_wallet::SecretKey::from_slice(&sk).unwrap()));
        assert_eq!(payout(t.p, Some(&kept), vec![1]), arcade_ledger::address::script_for_address(t.p, &pay_to).unwrap());
        // 1% of 5 coins.
        assert_eq!(maker_fee(t.p, Some(&kept), 500_000_000).map(|f| f.0), Some(5_000_000));
        // Released: cancelled, key forgotten.
        let r = release(&t.node, id).unwrap();
        assert_eq!(r["status"], "released");
        assert!(get(&t.node, "sorder", id).unwrap().get("own").is_none());
    }

    #[test]
    fn an_order_with_its_own_key_is_checked() {
        let t = crate::sitesvc::tests::setup();
        let (give, get_leg) = legs(&t);
        let k = t.p.key;
        let addr = arcade_ledger::address::base58check(t.p.p2pkh_version, &[4u8; 20]);
        assert!(place(&t.node, &give, &get_leg, &json!({"key": {k: "00"}, "payout": {k: addr}})).is_err(), "a bad key");
        assert!(place(&t.node, &give, &get_leg, &json!({"key": {k: hex::encode([9u8; 32])}})).is_err(), "no payout");
        assert!(place(&t.node, &give, &get_leg, &json!({"maker_fee": {"bps": 900, "to": {k: addr}}})).is_err(), "over 5%");
        assert!(place(&t.node, &give, &get_leg, &Value::Null).is_ok(), "a plain order as before");
        // DigiDollars to an own-key order come out to a DigiDollar address it names.
        let dd = json!({"chain": k, "kind": "dd", "asset": "DD", "amount": "2500"});
        let opts = json!({"key": {k: hex::encode([9u8; 32])}, "payout": {k: addr}});
        assert!(place(&t.node, &dd, &get_leg, &opts).is_err(), "no DigiDollar payout address");
    }
}

#[cfg(test)]
mod canon_leg_tests {
    use super::*;

    /// A leg named by the chain's mesh name ("test") is shown by its key; unknown names stay as they are.
    #[test]
    fn legs_name_chains_by_key() {
        let mut o = json!({"give": {"chain": "dgbtest", "kind": "dd"}, "get": {"chain": "test", "kind": "coin"}, "own": {"keys": {}}});
        let p = public_order(&o);
        assert_eq!((p["give"]["chain"].as_str(), p["get"]["chain"].as_str()), (Some("dgbtest"), Some("peptest")));
        assert!(p.get("own").is_none());
        o["get"]["chain"] = json!("nosuchchain");
        assert_eq!(public_order(&o)["get"]["chain"], "nosuchchain");
    }
}

#[cfg(test)]
mod whole_fee_tests {
    use super::*;

    #[test]
    fn whole_unit_fees_round_up() {
        let t = crate::sitesvc::tests::setup();
        let to = arcade_ledger::address::base58check(t.p.p2pkh_version, &[4u8; 20]);
        let o = json!({"maker_fee": {"bps": 100, "to": {t.p.key: to}}});
        assert_eq!(whole_fee(Some(&o), 25, t.p).map(|f| f.0), Some(1), "1% of $25 is $1, rounded up");
        assert_eq!(whole_fee(Some(&o), 250, t.p).map(|f| f.0), Some(3), "1% of $250 is $3 (2.5 up)");
        assert_eq!(whole_fee(Some(&o), 200, t.p).map(|f| f.0), Some(2), "exact stays");
        assert_eq!(whole_fee(Some(&o), 0, t.p), None);
        assert_eq!(whole_fee(None, 100, t.p), None, "no fee on an order without one");
    }
}

#[cfg(test)]
mod account_tests {
    use super::*;

    const B_WORDS: &str = "legal winner thank year wave sausage worth useful legal winner thank yellow";

    /// This arcade's own wallet ("") and an added account "b" (in the account list), the own wallet active.
    fn two_accounts(t: &crate::sitesvc::tests::T) -> (String, String) {
        let app = &t.node.app;
        app.create_wallet(None).unwrap();
        app.set_active_account("b").unwrap();
        app.create_wallet(Some(B_WORDS)).unwrap();
        app.set_active_account("").unwrap();
        let _ = app.store.write(|w| w.put(arcade_app::META, b"accounts", json!([{"id": "b"}]).to_string().as_bytes()));
        let hexkey = |id: &str| hex::encode(account_key(&t.node, t.p, id).unwrap().1.serialize());
        (hexkey(""), hexkey("b"))
    }

    fn legs(t: &crate::sitesvc::tests::T) -> (Value, Value) {
        let k = t.p.key;
        (json!({"chain": k, "kind": "coin", "asset": "", "amount": "500000000"}), json!({"chain": k, "kind": "coin", "asset": "", "amount": "100000000"}))
    }

    /// An order placed under one account keeps showing (and later signing with) that account's key after a switch;
    /// a swap taken under another account belongs to that one.
    #[test]
    fn orders_and_swaps_keep_the_account_that_started_them() {
        let t = crate::sitesvc::tests::setup();
        let (a, b) = two_accounts(&t);
        assert_ne!(a, b);
        let (give, get_leg) = legs(&t);
        let shown = place(&t.node, &give, &get_leg, &Value::Null).unwrap();
        let id = shown["id"].as_str().unwrap();
        assert!(shown.get("account").is_none(), "what others see names no account");
        assert_eq!(get(&t.node, "sorder", id).unwrap()["account"], "", "placed under this arcade's own wallet");
        t.node.app.set_active_account("b").unwrap();
        let me = t.node.id_hex();
        let q = answer(&t.node, &me, &json!({"op": "squote", "order": id, "taker": me})).unwrap();
        assert_eq!(q["maker_keys"][t.p.key], a, "the order's key, not the active account's");
        // Taken while "b" is active: the swap is b's.
        let s = take(&t.node, &me, id, Some(&shown)).unwrap();
        assert_eq!(s["account"], "b");
        assert_eq!(s["taker_keys"][t.p.key], b);
        t.node.app.set_active_account("").unwrap();
        let mut rec = get(&t.node, "swap", s["id"].as_str().unwrap()).unwrap();
        assert_eq!(hex::encode(signer(&t.node, t.p, &mut rec, None).unwrap().1.serialize()), b, "b signs b's swap while the own wallet is active");
        // The maker's side of a swap of the own wallet's order, with b active.
        t.node.app.set_active_account("b").unwrap();
        let mut m = json!({"role": "maker", "account": "", "maker_keys": {t.p.key: a}});
        assert_eq!(hex::encode(signer(&t.node, t.p, &mut m, None).unwrap().1.serialize()), a);
    }

    /// A key the swap's locks don't name is never used; a swap from before swaps named their account finds its own.
    #[test]
    fn a_swap_signs_only_with_the_key_its_locks_name() {
        let t = crate::sitesvc::tests::setup();
        let (a, b) = two_accounts(&t);
        let mut wrong = json!({"role": "taker", "account": "", "taker_keys": {t.p.key: b}});
        assert!(signer(&t.node, t.p, &mut wrong, None).unwrap_err().contains("not the one"));
        let mut old = json!({"role": "taker", "taker_keys": {t.p.key: b}});
        assert_eq!(hex::encode(signer(&t.node, t.p, &mut old, None).unwrap().1.serialize()), b);
        assert_eq!(old["account"], "b", "found once, then kept");
        let mut old_own = json!({"role": "maker", "maker_keys": {t.p.key: a}});
        signer(&t.node, t.p, &mut old_own, None).unwrap();
        assert_eq!(old_own["account"], "");
    }

    /// The account signed out: its swap says so plainly and signs nothing; added back, it signs again.
    #[test]
    fn a_swap_of_a_signed_out_account_waits_for_it() {
        let t = crate::sitesvc::tests::setup();
        let (_, b) = two_accounts(&t);
        let mut s = json!({"role": "taker", "account": "b", "taker_keys": {t.p.key: b}, "step": "waiting"});
        t.node.app.drop_account("b").unwrap();
        let e = signer(&t.node, t.p, &mut s, None).unwrap_err();
        assert!(e.contains("not on this arcade now") && e.contains("refund"), "{e}");
        assert_eq!(s["step"], "waiting", "nothing given up");
        t.node.app.set_active_account("b").unwrap();
        t.node.app.create_wallet(Some(B_WORDS)).unwrap();
        t.node.app.set_active_account("").unwrap();
        assert_eq!(hex::encode(signer(&t.node, t.p, &mut s, None).unwrap().1.serialize()), b);
        // A legacy order (no account named) runs as the active account, as it did.
        assert_eq!(order_account(&t.node, &json!({})), "");
        assert_eq!(order_account(&t.node, &json!({"account": "b"})), "b");
    }

    /// A taker claims the maker's lock only with half the order's time left (at least 120 s / 3 blocks).
    #[test]
    fn a_taker_claims_only_with_time_to_spare() {
        let (secs, at) = (4 * 3600, 1_000_000);
        assert!(claim_time_left(false, at + 7200, at, secs, 150));
        assert!(!claim_time_left(false, at + 7199, at, secs, 150));
        assert!(!claim_time_left(false, at + 121, at, secs, 150), "2 minutes was enough before; not now");
        assert!(claim_time_left(false, at + 120, at, 60, 150), "a short test lock: 120 s still the floor");
        assert!(!claim_time_left(false, at + 119, at, 60, 150));
        // The ARCADE chain: 48 blocks of 150 s for 4 hours' half; 3 blocks at least.
        assert!(claim_time_left(true, 1000 + 49, 1000, secs, 150));
        assert!(!claim_time_left(true, 1000 + 48, 1000, secs, 150));
        assert!(!claim_time_left(true, 1000 + 4, 1000, secs, 150), "3 blocks was enough before; not now");
        assert!(claim_time_left(true, 1000 + 4, 1000, 300, 150));
        assert!(!claim_time_left(true, 1000 + 3, 1000, 300, 150));
    }

    /// Holds belong to the asking arcade: never for a taker name of another, never stretched by asking again, a few per
    /// arcade, and an arcade whose hold ran out waits its turn.
    #[test]
    fn order_holds_are_bound_to_the_asking_arcade() {
        let t = crate::sitesvc::tests::setup();
        two_accounts(&t);
        let (give, get_leg) = legs(&t);
        let ids: Vec<String> = (0..4).map(|_| place(&t.node, &give, &get_leg, &Value::Null).unwrap()["id"].as_str().unwrap().to_string()).collect();
        let quote = |peer: &str, taker: &str, id: &str| answer(&t.node, peer, &json!({"op": "squote", "order": id, "taker": taker}));
        assert!(quote("pa", "pb", &ids[0]).is_err(), "a hold for another arcade's name");
        assert!(quote("", "", &ids[0]).is_err());
        quote("pa", "pa", &ids[0]).unwrap();
        assert!(quote("pc", "pc", &ids[0]).unwrap_err().contains("taking that order"), "one taker at a time");
        // Asking again keeps the hold it has.
        let mut o = get(&t.node, "sorder", &ids[0]).unwrap();
        let until = now() + 100;
        o["held_until"] = json!(until);
        put(&t.node, "sorder", &o);
        quote("pa", "pa", &ids[0]).unwrap();
        assert_eq!(get(&t.node, "sorder", &ids[0]).unwrap()["held_until"], json!(until), "not stretched");
        // A website's visitors count as its own takers, up to HOLDS_PER_PEER orders at once.
        quote("pa", "pa:v1", &ids[1]).unwrap();
        quote("pa", "pa:v2", &ids[2]).unwrap();
        assert!(quote("pa", "pa:v3", &ids[3]).unwrap_err().contains("holds 3 orders"));
        quote("pc", "pc", &ids[3]).unwrap();
        // pa's hold on order 0 ran out: pa waits, others may hold it.
        let mut o = get(&t.node, "sorder", &ids[0]).unwrap();
        o["held_until"] = json!(now() - 1);
        put(&t.node, "sorder", &o);
        assert!(quote("pa", "pa", &ids[0]).unwrap_err().contains("ran out"));
        quote("pd", "pd", &ids[0]).unwrap();
        // A taker tells of its own lock only.
        let told = answer(&t.node, "pc", &json!({"op": "slocked", "order": ids[0], "hash": "00", "taker": "pd"}));
        assert!(told.unwrap_err().contains("own lock"));
    }
}
