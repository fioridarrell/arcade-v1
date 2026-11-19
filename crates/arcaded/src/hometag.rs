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
    // own_chain: this chain's own @tag (tags per chain); a keeper from before that ignores it and answers its one
    // namespace, with its "chain" telling which (checked below).
    let q = serde_json::json!({"chain": p.key, "tag": tag, "local": true, "own_chain": true});
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
    let (name, chain) = parse(node, tag)?;
    let asked = match chain {
        Some(p) => vec![p],
        None => tag_chains(node),
    };
    let mut rows = Vec::new();
    for p in asked {
        if let Some((a, _)) = owner_on_checked(node, p, &name).map_err(|e| format!("can't tell whether @{name} is claimed on {}: {e}", p.name))? {
            rows.push((p, a));
        }
    }
    match one_of(rows) {
        Found::One(p, a) => Ok(Some((p, a))),
        Found::Many(rows) => Err(which_text(&name, &rows)),
        Found::None => Ok(None),
    }
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

// ---------------------------------------------------------------- tags per chain (Martin 2026-11-18)
//
// Every chain has its own @tags: @alice on Dogecoin and @alice on ARCADE can be two people. A tag's full name is its
// name and the chain it was claimed on, written `@alice.doge` (tags are [a-z0-9_], so the dot can't be part of a
// name); the app labels it `@alice · DOGE`. A plain `@alice` is used only when one chain has it (or every chain that
// has it has it for the same key: one person); otherwise the caller is told which ones there are.

/// The chains a tag can be claimed and looked up on: ARCADE, the ledger home and Pepecoin testnet, then the mainnet
/// chains arcades index posts and messages on (crate::chains::MESSAGE_CHAINS) when this arcade keeps their ledger or
/// borrows them (their tags then come from arcades keeping them).
pub fn tag_chains(node: &Node) -> Vec<&'static ChainParams> {
    let mut out = chains(node);
    for key in crate::chains::MESSAGE_CHAINS {
        let Some(p) = crate::chains::find(key) else { continue };
        let kept = node.chain.indexers_for(p.chain_no).contains(&"ledger.v1");
        if (kept || crate::borrow::is_borrowed(p.key)) && !out.iter().any(|o| o.chain_no == p.chain_no) {
            out.push(p);
        }
    }
    out
}

/// Whether tags can be claimed (and looked up) on `p` here.
pub fn is_tag_chain(node: &Node, p: &ChainParams) -> bool {
    tag_chains(node).iter().any(|c| c.chain_no == p.chain_no)
}

/// `@alice.doge`, `alice.doge`, `@alice` or `alice`: the name (lowercase, no @) and the chain named after the dot,
/// if any. Err for a dot naming no chain tags live on here.
pub fn parse(node: &Node, input: &str) -> Result<(String, Option<&'static ChainParams>), String> {
    let s = norm(input);
    match s.rsplit_once('.') {
        None => Ok((s, None)),
        Some((name, key)) => {
            let p = crate::chains::find(key).filter(|p| is_tag_chain(node, p)).ok_or_else(|| {
                let keys: Vec<String> = tag_chains(node).iter().map(|p| p.key.to_string()).collect();
                format!("@{name}.{key}: {key} isn't a chain @tags live on here ({})", keys.join(", "))
            })?;
            Ok((name.to_string(), Some(p)))
        }
    }
}

/// A tag's full name: `alice.doge` (no @).
pub fn qualified(p: &ChainParams, tag: &str) -> String {
    format!("{}.{}", norm(tag), p.key)
}

/// A tag as the app shows it: `@alice · DOGE`.
pub fn label(p: &ChainParams, tag: &str) -> String {
    format!("@{} · {}", norm(tag), crate::explorer::unit(p))
}

/// What a tag names.
#[derive(Clone)]
pub enum Found {
    /// One holder: the chain (the one named, or the only one with it) and the owner's address there.
    One(&'static ChainParams, String),
    /// Different holders on several chains: each chain and owner. The caller asks which one.
    Many(Vec<(&'static ChainParams, String)>),
    None,
}

/// Several chains' holders of one name, as one answer: the same key on all (one person, as the ledger snapshot made
/// on ARCADE and Pepecoin testnet) is one holder, the first chain's; different keys are several.
fn one_of(rows: Vec<(&'static ChainParams, String)>) -> Found {
    match rows.first() {
        None => Found::None,
        Some(first) if rows.iter().all(|r| key_hash(&r.1) == key_hash(&first.1)) => Found::One(first.0, first.1.clone()),
        Some(_) => Found::Many(rows),
    }
}

/// What `input` (`@alice.doge` or `@alice`) names. Err when the chain named can't be asked (its tags borrowed and no
/// keeper answered) or the dot names no tag chain.
pub fn find(node: &Node, input: &str) -> Result<Found, String> {
    let (name, chain) = parse(node, input)?;
    match chain {
        Some(p) => Ok(owner_on_checked(node, p, &name)?.map_or(Found::None, |(a, _)| Found::One(p, a))),
        None => Ok(one_of(tag_chains(node).into_iter().filter_map(|p| owner_on(node, p, &name).map(|(a, _)| (p, a))).collect())),
    }
}

/// The "which @alice?" answer for a name held on several chains: each as {tag, chain, label, address, name}, the
/// last the holder's profile name where one is known.
pub fn choices(node: &Node, name: &str, rows: &[(&'static ChainParams, String)]) -> Vec<serde_json::Value> {
    rows.iter()
        .map(|(p, a)| {
            let shown = crate::msgsvc::profile_on(node, p, a).and_then(|v| v["n"].as_str().or(v["name"].as_str()).map(String::from));
            serde_json::json!({"tag": qualified(p, name), "chain": p.key, "label": label(p, name), "address": a, "name": shown})
        })
        .collect()
}

/// The words for a name held on several chains: "@alice is claimed on 2 chains: @alice.arctest, @alice.doge; say which".
pub fn which_text(name: &str, rows: &[(&'static ChainParams, String)]) -> String {
    let all: Vec<String> = rows.iter().map(|(p, _)| format!("@{}", qualified(p, name))).collect();
    format!("@{name} is claimed on {} chains by different people: {}; say which", rows.len(), all.join(", "))
}

/// Who `input` names, or the words why not (not claimed; claimed on several chains, say which; can't tell).
pub fn owner_or_err(node: &Node, input: &str) -> Result<(&'static ChainParams, String), String> {
    let (name, _) = parse(node, input)?;
    match find(node, input)? {
        Found::One(p, a) => Ok((p, a)),
        Found::Many(rows) => Err(which_text(&name, &rows)),
        Found::None => Err(format!("@{name} is not claimed")),
    }
}

/// Where a payment on `coin` to the holder of a tag on chain `on` (owner `holder`) goes, never a guess:
/// - `coin` is the tag's own chain: the holder's address;
/// - `coin` derives keys as the tag's chain does (the same BIP44 coin type: ARCADE testnet beside Pepecoin testnet),
///   so the same wallet holds the same key there: that key's address on `coin`;
/// - otherwise the address the holder has proven on `coin` (A:// op 7 wallet proofs on ARCADE, found through the
///   person: the tag's ARCADE key, or the ARCADE owner that proved the tag's address).
///
/// Err with the words why when there's none.
pub fn pay_address(node: &Node, coin: &'static ChainParams, on: &'static ChainParams, tag: &str, holder: &str) -> Result<String, String> {
    if coin.chain_no == on.chain_no {
        return Ok(holder.to_string());
    }
    if coin.bip44_coin_type == on.bip44_coin_type {
        return address_on(coin, holder).ok_or_else(|| format!("@{}'s address doesn't read", qualified(on, tag)));
    }
    let who = format!("@{}", qualified(on, tag));
    let arc = crate::arcsvc::defined().filter(|p| crate::arcsvc::usable(p)).ok_or_else(|| format!("{who}'s {} address isn't known here: paste their address", coin.name))?;
    if !crate::arcsvc::own(arc) {
        return Err(format!("{who}'s proven {} address is on {}, which this arcade borrows and doesn't keep: paste their address", coin.name, arc.name));
    }
    // The person on ARCADE: the tag's key there, or whoever proved the tag's address.
    let owner = if on.bip44_coin_type == arc.bip44_coin_type {
        address_on(arc, holder)
    } else {
        crate::walletproofs::owner_of_proven(node, arc.chain_no, on.key, holder)
    }
    .ok_or_else(|| format!("{who} hasn't proven any other coin's address (wallet proofs): paste their {} address", coin.name))?;
    if coin.chain_no == arc.chain_no {
        return Ok(owner);
    }
    crate::walletproofs::proven_on(node, arc.chain_no, &owner, coin.key)
        .ok_or_else(|| format!("{who} hasn't proven a {} address (wallet proofs): paste their address, or ask them to publish one", coin.name))
}

/// Every tag the key of `address` holds, each with its chain (a profile lists them all).
pub fn all_of_address(node: &Node, address: &str) -> Vec<(&'static ChainParams, String)> {
    tag_chains(node).into_iter().filter_map(|p| of_address_on(node, p, address).map(|t| (p, t))).collect()
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

/// Does the key of `address` hold `tag`: on the chain named (`@alice.doge`), or on any tag chain (a plain `@alice`:
/// each chain's @alice is its own holder's)?
pub fn holds(node: &Node, tag: &str, address: &str) -> bool {
    let (Ok((name, chain)), Some(h)) = (parse(node, tag), key_hash(address)) else { return false };
    let asked = chain.map_or_else(|| tag_chains(node), |p| vec![p]);
    asked.into_iter().any(|p| owner_on(node, p, &name).is_some_and(|(a, _)| key_hash(&a).as_deref() == Some(&h[..])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcade_store::KeyBuf;
    use serde_json::json;

    fn put_tag(node: &Node, c: u16, tag: &str, address: &str) {
        let v = json!({"a": address, "t": "00", "h": 2, "p": 0});
        node.chain.store(c).write(|w| {
            w.put(arcade_ledger::index::TAG, &KeyBuf::new().u16(c).bytes(tag.as_bytes()).done(), v.to_string().as_bytes())?;
            w.put(arcade_ledger::index::TAG_BY_ADDRESS, &KeyBuf::new().u16(c).bytes(address.as_bytes()).done(), tag.as_bytes())
        }).unwrap();
    }

    fn put_proof(node: &Node, c: u16, owner: &str, coin: &str, address: &str, h: u64) {
        let key = KeyBuf::new().u16(c).bytes(owner.as_bytes()).u8(0).bytes(coin.as_bytes()).u8(0).bytes(address.as_bytes()).done();
        node.chain.store(c).write(|w| w.put(arcade_ledger::engine::A_WALLET, &key, json!({"h": h, "txid": "00"}).to_string().as_bytes())).unwrap();
    }

    /// Tags per chain: full names and labels, one holder or several, and the words for "which one".
    #[test]
    fn full_names_labels_and_which() {
        let doge = arcade_chain::params::ALL_CHAINS.iter().copied().find(|p| p.key == "doge").unwrap();
        let peptest = &arcade_chain::params::PEPECOIN_TEST;
        assert_eq!(qualified(doge, "@Alice"), "alice.doge");
        assert_eq!(label(doge, "alice"), "@alice · DOGE");
        let a = base58check(peptest.p2pkh_version, &[1; 20]);
        let same = base58check(doge.p2pkh_version, &[1; 20]);
        let other = base58check(doge.p2pkh_version, &[2; 20]);
        // The same key on two chains is one holder (the ledger snapshot's case).
        assert!(matches!(one_of(vec![(peptest, a.clone()), (doge, same)]), Found::One(p, ref x) if p.key == "peptest" && *x == a));
        // Different keys: several, to pick from.
        let rows = vec![(peptest, a), (doge, other)];
        assert!(matches!(one_of(rows.clone()), Found::Many(ref r) if r.len() == 2));
        assert!(matches!(one_of(vec![]), Found::None));
        assert_eq!(which_text("alice", &rows), "@alice is claimed on 2 chains by different people: @alice.peptest, @alice.doge; say which");
    }

    /// On this arcade's home chain: `@alice.<home>` and a plain `@alice` both name its holder; a dot naming no tag chain
    /// is refused; an unclaimed tag is "not claimed".
    #[test]
    fn parse_and_find_on_the_home_chain() {
        let t = crate::sitesvc::tests::setup();
        let (node, p) = (&t.node, t.p);
        let _ = node.chain.add_indexer(p.chain_no, std::sync::Arc::new(arcade_ledger::engine::LedgerIndexer));
        let alice = base58check(p.p2pkh_version, &[9; 20]);
        put_tag(node, p.chain_no, "alice", &alice);
        let (n, c) = parse(node, "@Alice").unwrap();
        assert!(n == "alice" && c.is_none());
        let (n, c) = parse(node, &format!("@alice.{}", p.key)).unwrap();
        assert_eq!((n.as_str(), c.map(|c| c.key)), ("alice", Some(p.key)));
        assert!(parse(node, "@alice.nochain").unwrap_err().contains("isn't a chain @tags live on"));
        assert_eq!(owner_or_err(node, "@alice").unwrap().1, alice);
        assert_eq!(owner_or_err(node, &format!("alice.{}", p.key)).unwrap().1, alice);
        assert_eq!(owner_or_err(node, "@bob").unwrap_err(), "@bob is not claimed");
        assert!(holds(node, "@alice", &alice) && !holds(node, "@alice", &base58check(p.p2pkh_version, &[8; 20])));
        assert_eq!(all_of_address(node, &alice).iter().map(|(c, t)| (c.key, t.as_str())).collect::<Vec<_>>(), [(p.key, "alice")]);
    }

    /// Two people, one name: @alice held by one key on the home chain and by another on Dogecoin (its ledger kept
    /// here). A plain @alice is ambiguous (each listed, to pick); `@alice.<chain>` names each one's own holder; each
    /// holds the tag only on their own chain; a payment in another coin with no proof is refused.
    #[test]
    fn one_name_two_people_on_two_chains() {
        let t = crate::sitesvc::tests::setup();
        let (node, p) = (&t.node, t.p);
        let doge = arcade_chain::params::ALL_CHAINS.iter().copied().find(|c| c.key == "doge").unwrap();
        for c in [p.chain_no, doge.chain_no] {
            let _ = node.chain.add_indexer(c, std::sync::Arc::new(arcade_ledger::engine::LedgerIndexer));
        }
        let home_alice = base58check(p.p2pkh_version, &[1; 20]);
        let doge_alice = base58check(doge.p2pkh_version, &[2; 20]);
        put_tag(node, p.chain_no, "alice", &home_alice);
        put_tag(node, doge.chain_no, "alice", &doge_alice);
        assert!(tag_chains(node).iter().any(|c| c.key == "doge"), "Dogecoin's ledger kept: tags live there");
        let Found::Many(rows) = find(node, "@alice").unwrap() else { panic!("a plain @alice should be ambiguous") };
        assert_eq!(rows.iter().map(|(c, _)| c.key).collect::<Vec<_>>(), [p.key, "doge"]);
        assert!(owner_or_err(node, "@alice").unwrap_err().contains("say which"));
        assert!(owner_checked(node, "alice").unwrap_err().contains("say which"));
        assert_eq!(owner_or_err(node, "@alice.doge").unwrap().1, doge_alice);
        assert_eq!(owner_or_err(node, &format!("@alice.{}", p.key)).unwrap().1, home_alice);
        assert!(holds(node, "@alice.doge", &doge_alice) && !holds(node, "@alice.doge", &home_alice));
        assert!(holds(node, "@alice", &home_alice) && holds(node, "@alice", &doge_alice));
        // Claiming on each chain only looks at that chain: a free name on one is free there, whoever holds it elsewhere.
        assert!(!crate::claims::check_on(node, doge, "alice").available);
        assert!(crate::claims::check_on(node, doge, "bob").available || crate::claims::check_on(node, doge, "bob").reason.is_some_and(|r| !r.contains("taken on")));
        // Paying the Dogecoin @alice on the home chain's coin: different keys, no proof here, so refused.
        assert!(pay_address(node, p, doge, "alice", &doge_alice).is_err());
        assert_eq!(pay_address(node, doge, doge, "alice", &doge_alice).unwrap(), doge_alice);
    }

    /// Where a payment goes: the tag's own chain pays its holder; a chain deriving keys alike pays the same key; any
    /// other coin only an address the person proved (never a guess).
    #[test]
    fn payment_goes_to_the_holder_or_a_proven_address() {
        let t = crate::sitesvc::tests::setup();
        let (node, p) = (&t.node, t.p);
        let _ = node.chain.add_indexer(p.chain_no, std::sync::Arc::new(arcade_ledger::engine::LedgerIndexer));
        let alice = base58check(p.p2pkh_version, &[9; 20]);
        assert_eq!(pay_address(node, p, p, "alice", &alice).unwrap(), alice);
        let doge = arcade_chain::params::ALL_CHAINS.iter().copied().find(|c| c.key == "doge").unwrap();
        // No ARCADE chain here: another coin's address can't be known, so it's refused.
        assert!(pay_address(node, doge, p, "alice", &alice).unwrap_err().contains("paste their address"));
        // The proofs themselves: the newest proven address on a coin, and who proved an address.
        put_proof(node, p.chain_no, "aOwner", "doge", "DOld", 5);
        put_proof(node, p.chain_no, "aOwner", "doge", "DNew", 9);
        put_proof(node, p.chain_no, "aOwner", "ltc", "LOne", 7);
        assert_eq!(crate::walletproofs::proven_on(node, p.chain_no, "aOwner", "doge").as_deref(), Some("DNew"));
        assert_eq!(crate::walletproofs::proven_on(node, p.chain_no, "aOwner", "dgb"), None);
        assert_eq!(crate::walletproofs::owner_of_proven(node, p.chain_no, "ltc", "LOne").as_deref(), Some("aOwner"));
        assert_eq!(crate::walletproofs::owner_of_proven(node, p.chain_no, "ltc", "LTwo"), None);
    }

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
