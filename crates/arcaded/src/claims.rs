//! The @tag claim flow (whiteboard steps 6-9): check -> fund (faucet) -> send -> confirm.

use crate::node::Node;
use arcade_app::{Claim, ClaimStatus};
use arcade_chain::OutPoint;
use arcade_ledger::tags;
use arcade_wallet::tx::{build_claim, Utxo};

/// 0.01 coin per kB, the Dogecoin-family default.
pub const FEE_PER_KB: u64 = 1_000_000;
pub const DUST: u64 = 1_000_000;

#[derive(Debug, serde::Serialize)]
pub struct Check {
    pub tag: String,
    pub available: bool,
    pub reason: Option<String>,
}

pub fn check(node: &Node, tag: &str) -> Check {
    let tag = tag.trim().trim_start_matches('@').to_lowercase();
    if let Some(p) = tags::tag_problem(&tag) {
        return Check { tag, available: false, reason: Some(p.into()) };
    }
    let idx = node.tags.lock().unwrap();
    if let Some(row) = idx.tags.by_tag.get(&tag) {
        return Check { tag, available: false, reason: Some(format!("taken by {} at block {}", row.address, row.block_height)) };
    }
    drop(idx);
    let pending = node.app.claims().unwrap_or_default().into_iter().any(|c| {
        c.tag == tag && matches!(c.status, ClaimStatus::Sent | ClaimStatus::Funding | ClaimStatus::Checking)
    });
    if pending {
        return Check { tag, available: false, reason: Some("you already have a claim for it in progress".into()) };
    }
    Check { tag, available: true, reason: None }
}

/// Spendable coins of a tag key's address.
fn coins(node: &Node, script: &[u8]) -> Vec<Utxo> {
    let p = node.params;
    let c = p.chain_no;
    let tip = node.chain.tip(c).ok().flatten().map(|t| t.height).unwrap_or(0);
    let mut out = Vec::new();
    for (t12, vout, value, _h) in node.chain.coins_for_script(c, script).unwrap_or_default() {
        for txid in node.chain.txids_by_t12(p, &t12).unwrap_or_default() {
            let op = OutPoint { txid, vout };
            if let Ok(Some(coin)) = node.chain.coin(c, &op) {
                if coin.coinbase && coin.height + p.coinbase_maturity > tip {
                    continue; // immature
                }
                out.push(Utxo { outpoint: op, value, script: script.to_vec() });
            }
        }
    }
    out
}

pub fn start(node: &Node, tag: &str) -> Result<Claim, String> {
    let ck = check(node, tag);
    if !ck.available {
        return Err(format!("@{} is not available: {}", ck.tag, ck.reason.unwrap_or_default()));
    }
    if !node.app.has_wallet().map_err(|e| e.to_string())? {
        return Err("create a wallet first (arcade wallet create)".into());
    }
    let index = node.app.free_tag_index(node.params.chain_no).map_err(|e| e.to_string())?;
    let now = arcade_app::now();
    let claim = Claim {
        id: format!("{}:{}:{now}", node.params.key, ck.tag),
        chain: node.params.chain_no,
        tag: ck.tag.clone(),
        key_index: index,
        status: ClaimStatus::Checking,
        txid: None,
        note: String::new(),
        created: now,
        updated: now,
    };
    node.app.put_claim(&claim).map_err(|e| e.to_string())?;
    node.app.event("tag", &format!("claim for @{} started (key {index})", ck.tag)).ok();
    advance_one(node, claim)
}

/// Move one claim forward as far as it can go right now.
fn advance_one(node: &Node, mut c: Claim) -> Result<Claim, String> {
    let p = node.params;
    let (key, sk) = node.app.tag_key(p, c.key_index).map_err(|e| e.to_string())?;
    let script = arcade_wallet::keys::p2pkh_script_for(
        &arcade_wallet::secp256k1::PublicKey::from_slice(&hex::decode(&key.pubkey).unwrap()).unwrap(),
    );
    let set = |c: &mut Claim, s: ClaimStatus, note: &str| {
        c.status = s;
        c.note = note.into();
        c.updated = arcade_app::now();
    };
    match c.status {
        ClaimStatus::Checking | ClaimStatus::Funding => {
            // Someone else may have taken it meanwhile.
            if let Some(row) = node.tags.lock().unwrap().tags.by_tag.get(&c.tag) {
                if row.address != key.address {
                    set(&mut c, ClaimStatus::Failed, "taken by someone else first");
                    node.app.put_claim(&c).ok();
                    return Ok(c);
                }
            }
            let have = coins(node, &script);
            match build_claim(p, &tags::claim_payload(&c.tag), &sk, &have, FEE_PER_KB, DUST, arcade_app::now() as u32) {
                Ok(tx) => {
                    node.tx_outbox.lock().unwrap().push(tx.raw.clone());
                    c.txid = Some(tx.txid.to_hex());
                    set(&mut c, ClaimStatus::Sent, "claim sent; waiting for a block");
                    node.app.event("tag", &format!("claim for @{} sent in {}", c.tag, tx.txid)).ok();
                }
                Err(e) => {
                    if c.status == ClaimStatus::Checking {
                        let already = node.app.faucet_requests().unwrap_or_default().iter().any(|f| f.address == key.address);
                        if !already {
                            crate::faucet::ask(node, &key.address, &c.tag)?;
                        }
                    }
                    set(&mut c, ClaimStatus::Funding, &format!("waiting for test coins to {} ({e})", key.address));
                }
            }
        }
        ClaimStatus::Sent => {
            let confirmed = node.tags.lock().unwrap().tags.by_tag.get(&c.tag).map(|r| r.address == key.address);
            match confirmed {
                Some(true) => {
                    set(&mut c, ClaimStatus::Confirmed, "confirmed on chain");
                    let mut k = key.clone();
                    k.tag = Some(c.tag.clone());
                    node.app
                        .store
                        .write(|w| {
                            w.put(
                                arcade_app::TAG_KEYS,
                                &arcade_store::KeyBuf::new().u16(k.chain).u32(k.index).done(),
                                &serde_json::to_vec(&k).unwrap(),
                            )
                        })
                        .ok();
                    node.app.event("tag", &format!("@{} is yours", c.tag)).ok();
                }
                Some(false) => set(&mut c, ClaimStatus::Failed, "someone else's claim confirmed first"),
                None => {}
            }
        }
        ClaimStatus::Confirmed | ClaimStatus::Failed => {}
    }
    node.app.put_claim(&c).map_err(|e| e.to_string())?;
    Ok(c)
}

/// Called after each sync round.
pub fn advance_all(node: &Node) {
    for c in node.app.claims().unwrap_or_default() {
        if !matches!(c.status, ClaimStatus::Confirmed | ClaimStatus::Failed) {
            let _ = advance_one(node, c);
        }
    }
}
