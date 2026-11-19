//! @tags across the ARCADE chain and the ledger home (docs/HOMECHAIN.md phase 2): ARCADE first, then the ledger home.
//! One key, one person: the same key hash is the same wallet on both chains (ARCADE's BIP44 coin type is the home
//! chain's), so a tag held on either belongs to that key. Until phase 3 the ledger is still written on the home chain;
//! these reads let a tag that lives only on ARCADE count everywhere a tag is asked for.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use arcade_ledger::address::{base58check, decode_base58check};
use arcade_ledger::index::{tag_of, tag_row};

/// The ARCADE chain when it can be used here (installed or borrowed) and isn't the ledger home itself.
fn arcade(node: &Node) -> Option<&'static ChainParams> {
    crate::arcsvc::defined().filter(|p| crate::arcsvc::usable(p) && p.chain_no != node.ledger_home().chain_no)
}

/// Who holds `tag` on one chain (`p`'s own namespace), as an address on `p` and the claim's height (0 when unknown):
/// this arcade's own ledger, else, with `p`'s tag tables borrowed here, lenders' rows, else a keeper asked.
pub fn owner_on(node: &Node, p: &'static ChainParams, tag: &str) -> Option<(String, u32)> {
    owner_on_checked(node, p, tag).ok().flatten()
}

/// [`owner_on`], telling "nobody holds it" (Ok(None)) from "can't tell": Err while `p`'s tags are borrowed here and no
/// arcade keeping them answered.
pub fn owner_on_checked(node: &Node, p: &'static ChainParams, tag: &str) -> Result<Option<(String, u32)>, String> {
    let tag = norm(tag);
    if let Some(r) = tag_row(&node.chain, p.chain_no, &tag).ok().flatten() {
        return Ok(Some((r.address, r.block_height)));
    }
    if !borrowed_tags(node, p) {
        return Ok(None);
    }
    if crate::lentrows::tags_lent(node, p) {
        if let Some(o) = crate::lentrows::tag_owner(node, p, &tag) {
            return Ok(Some(o));
        }
    }
    asked(node, p, &tag)
}

/// The @tag held on chain `p` by the key of `address` (any chain's p2pkh form): this arcade's own ledger (under `p`'s
/// own form, or the old home's when a snapshot carried it), else lenders' rows when `p`'s tag tables are borrowed.
pub fn of_address_on(node: &Node, p: &'static ChainParams, address: &str) -> Option<String> {
    let h = key_hash(address)?;
    [p.p2pkh_version, node_home_version(node)].into_iter().find_map(|v| tag_of(&node.chain, p.chain_no, &base58check(v, &h)).ok().flatten())
        .or_else(|| crate::lentrows::tags_lent(node, p).then(|| crate::lentrows::tag_of_key(node, p, &h)).flatten())
        // A borrowed ARCADE: a keeper's explorer.address (not limited to arcades with a handle, as lent rows are).
        .or_else(|| (crate::arcborrow::is(p) && crate::arcsvc::borrowed(p)).then(|| crate::claims::arc_tag_on_mesh(node, p, &base58check(p.p2pkh_version, &h))).flatten())
}

fn node_home_version(node: &Node) -> u8 {
    node.params.p2pkh_version
}

fn norm(tag: &str) -> String {
    tag.trim().trim_start_matches('@').to_lowercase()
}

/// Who holds `tag` on borrowed chain `p` by a keeper's explorer.tag, for a claim newer than the lent rows: asked at
/// most once a minute per (chain, tag). Err: no keeper answered.
fn asked(node: &Node, p: &'static ChainParams, tag: &str) -> Result<Option<(String, u32)>, String> {
    use std::collections::HashMap;
    use std::sync::Mutex;
    type Answers = HashMap<(u16, String), (Result<Option<(String, u32)>, String>, u64)>;
    static ASKED: Mutex<Option<Answers>> = Mutex::new(None);
    let now = arcade_app::now();
    let key = (p.chain_no, tag.to_string());
    if let Some((r, at)) = ASKED.lock().unwrap().get_or_insert_with(HashMap::new).get(&key) {
        if at + 60 > now {
            return r.clone();
        }
    }
    let q = serde_json::json!({"chain": p.key, "tag": tag, "local": true});
    let r = match crate::explorer::mesh_ask(node, p, if crate::arcborrow::is(p) { "" } else { "ledger" }, "explorer.tag", &q) {
        // The keeper's answer names the chain it read (its "chain"); one for another chain isn't this chain's tag.
        Ok((v, _)) if v["chain"].as_str().is_some_and(|c| c != p.key) => Ok(None),
        Ok((v, _)) => Ok(v["home_address"].as_str().filter(|a| !a.is_empty()).map(|a| (a.to_string(), v["claimed_at"].as_u64().unwrap_or(0) as u32))),
        Err(e) if e.contains("not claimed") => Ok(None),
        Err(e) => Err(e),
    };
    let mut g = ASKED.lock().unwrap();
    let m = g.get_or_insert_with(HashMap::new);
    if m.len() > 10_000 {
        m.clear();
    }
    m.insert(key, (r.clone(), now));
    r
}

/// [`owner`], telling "nobody holds it" from "can't tell": Err while a chain's tags are borrowed and no keeper could
/// be asked, so a caller that would treat a free tag as anyone's (vault.put's reservations) doesn't.
pub fn owner_checked(node: &Node, tag: &str) -> Result<Option<(&'static ChainParams, String)>, String> {
    if let Some(o) = owner(node, tag) {
        return Ok(Some(o));
    }
    for p in chains(node) {
        owner_on_checked(node, p, tag).map_err(|e| format!("can't tell whether @{} is claimed on {}: {e}", norm(tag), p.name))?;
    }
    Ok(None)
}

/// Whether chain `p`'s tag tables are borrowed here (ARCADE borrowed; another ledger chain not kept with its ledger).
fn borrowed_tags(node: &Node, p: &ChainParams) -> bool {
    if crate::arcborrow::is(p) {
        crate::arcsvc::borrowed(p)
    } else {
        crate::borrow::is_borrowed(p.key) && !node.chain.indexers_for(p.chain_no).contains(&"ledger.v1")
    }
}

/// The chains asked, in order: ARCADE, then the ledger home; with ARCADE as the home, Pepecoin testnet too (its tags
/// count again since writes fail over there while ARCADE is stalled, Martin 2026-11-17).
fn chains(node: &Node) -> Vec<&'static ChainParams> {
    let mut out: Vec<&'static ChainParams> = arcade(node).into_iter().chain([node.ledger_home()]).collect();
    if !out.iter().any(|p| p.chain_no == node.params.chain_no) {
        out.push(node.params);
    }
    out
}

/// The time of block `h` on chain `c` (0 when unknown).
fn block_time(node: &Node, c: u16, h: u32) -> u64 {
    node.chain.hash_at(c, h).ok().flatten().and_then(|hash| node.chain.header(c, &hash).ok().flatten()).map(|r| r.header.time as u64).unwrap_or(0)
}

/// An address's key hash (any chain's p2pkh form).
fn key_hash(address: &str) -> Option<Vec<u8>> {
    decode_base58check(address).filter(|(_, h)| h.len() == 20).map(|(_, h)| h)
}

/// Whether two addresses (any chain's p2pkh form) are the same key.
pub fn same_key(a: &str, b: &str) -> bool {
    a == b || key_hash(a).is_some_and(|x| Some(x) == key_hash(b))
}

/// The same key's address on chain `p`.
pub fn address_on(p: &ChainParams, address: &str) -> Option<String> {
    key_hash(address).map(|h| base58check(p.p2pkh_version, &h))
}

/// Who holds `tag`: the chain it was found on and its owner's address there (ARCADE first).
pub fn owner(node: &Node, tag: &str) -> Option<(&'static ChainParams, String)> {
    let tag = tag.trim().trim_start_matches('@').to_lowercase();
    // Held on several chains: the same key's (the snapshot carried it) is one holder; different keys, the earliest
    // claim by block time wins (one namespace on every ledger chain).
    let rows: Vec<(&'static ChainParams, String, u64)> = chains(node)
        .into_iter()
        .filter_map(|p| owner_on(node, p, &tag).map(|(a, h)| (p, a, if h > 0 { block_time(node, p.chain_no, h) } else { 0 })))
        .collect();
    let first = rows.first()?.clone();
    if rows.iter().all(|r| key_hash(&r.1) == key_hash(&first.1)) {
        return Some((first.0, first.1));
    }
    rows.into_iter().filter(|r| r.2 > 0).min_by_key(|r| r.2).or(Some(first)).map(|r| (r.0, r.1))
}

/// The @tag held by the key of `address` (given in any chain's form): ARCADE first, then the ledger home.
pub fn of_address(node: &Node, address: &str) -> Option<String> {
    let h = key_hash(address)?;
    // On ARCADE a tag is filed under the chain's own form when claimed there, or under the old home's (Pepecoin
    // testnet's) form when the ledger snapshot carried it over: both are asked.
    chains(node).into_iter().find_map(|p| of_address_on(node, p, address)).or_else(|| own_on_borrowed(node, &h))
}

/// ARCADE borrowed here (not installed, or installed and still catching up): its tags aren't in this arcade's own
/// ledger, so this wallet's own handle is the one its claim confirmed (recorded on the key, crate::claims).
fn own_on_borrowed(node: &Node, h: &[u8]) -> Option<String> {
    let arc = crate::arcsvc::usable_chain().filter(|p| crate::arcsvc::borrowed(p))?;
    node.app.tag_keys(arc.chain_no).ok()?.into_iter().find(|k| k.tag.is_some() && key_hash(&k.address).as_deref() == Some(h)).and_then(|k| k.tag)
}

/// Does the key of `address` hold `tag` (by [`owner`])?
pub fn holds(node: &Node, tag: &str, address: &str) -> bool {
    match (owner(node, tag), key_hash(address)) {
        (Some((_, a)), Some(h)) => key_hash(&a).as_deref() == Some(&h[..]),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_on_another_chain_is_the_same_key() {
        let home = &arcade_chain::params::PEPECOIN_TEST;
        let a = base58check(home.p2pkh_version, &[7; 20]);
        let arc = crate::arcsvc::params_for("arctest").or_else(|| arcade_chain::params::ALL_CHAINS.iter().copied().find(|p| p.key == "doge")).unwrap();
        let b = address_on(arc, &a).unwrap();
        assert_ne!(a, b, "another version byte");
        assert_eq!(key_hash(&a), key_hash(&b));
        assert_eq!(address_on(home, &b).as_deref(), Some(a.as_str()));
        assert!(address_on(home, "not an address").is_none());
    }
}
