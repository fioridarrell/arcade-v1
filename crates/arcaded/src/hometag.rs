//! @tags across the ARCADE chain and the ledger home (docs/HOMECHAIN.md phase 2): ARCADE first, then the ledger home.
//! One key, one person: the same key hash is the same wallet on both chains (ARCADE's BIP44 coin type is the home
//! chain's), so a tag held on either belongs to that key. Until phase 3 the ledger is still written on the home chain;
//! these reads let a tag that lives only on ARCADE count everywhere a tag is asked for.

use crate::node::Node;
use arcade_chain::params::ChainParams;
use arcade_ledger::address::{base58check, decode_base58check};
use arcade_ledger::index::{tag_of, tag_row};

/// The ARCADE chain when it runs here and isn't the ledger home itself.
fn arcade(node: &Node) -> Option<&'static ChainParams> {
    crate::arcsvc::defined().filter(|p| crate::arcsvc::runs(p) && p.chain_no != node.ledger_home().chain_no)
}

/// The chains asked, in order: ARCADE, then the ledger home.
fn chains(node: &Node) -> Vec<&'static ChainParams> {
    arcade(node).into_iter().chain([node.ledger_home()]).collect()
}

/// An address's key hash (any chain's p2pkh form).
fn key_hash(address: &str) -> Option<Vec<u8>> {
    decode_base58check(address).filter(|(_, h)| h.len() == 20).map(|(_, h)| h)
}

/// The same key's address on chain `p`.
pub fn address_on(p: &ChainParams, address: &str) -> Option<String> {
    key_hash(address).map(|h| base58check(p.p2pkh_version, &h))
}

/// Who holds `tag`: the chain it was found on and its owner's address there (ARCADE first).
pub fn owner(node: &Node, tag: &str) -> Option<(&'static ChainParams, String)> {
    let tag = tag.trim().trim_start_matches('@').to_lowercase();
    chains(node).into_iter().find_map(|p| tag_row(&node.chain, p.chain_no, &tag).ok().flatten().map(|row| (p, row.address)))
}

/// The @tag held by the key of `address` (given in any chain's form): ARCADE first, then the ledger home.
pub fn of_address(node: &Node, address: &str) -> Option<String> {
    let h = key_hash(address)?;
    // On ARCADE a tag is filed under the chain's own form when claimed there, or under the old home's (Pepecoin
    // testnet's) form when the ledger snapshot carried it over: both are asked.
    chains(node).into_iter().find_map(|p| {
        [p.p2pkh_version, node.params.p2pkh_version].into_iter().find_map(|v| tag_of(&node.chain, p.chain_no, &base58check(v, &h)).ok().flatten())
    })
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
