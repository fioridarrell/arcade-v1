//! Base58Check addresses and hash160.

use crate::script::{classify, ScriptKind};
use arcade_chain::params::ChainParams;
use ripemd::Ripemd160;
use sha2::{Digest, Sha256};

pub fn hash160(data: &[u8]) -> [u8; 20] {
    Ripemd160::digest(Sha256::digest(data)).into()
}

pub fn base58check(version: u8, payload: &[u8]) -> String {
    let mut v = vec![version];
    v.extend_from_slice(payload);
    bs58::encode(v).with_check().into_string()
}

/// (version, payload) of a Base58Check string.
pub fn decode_base58check(s: &str) -> Option<(u8, Vec<u8>)> {
    let v = bs58::decode(s).with_check(None).into_vec().ok()?;
    let (ver, rest) = v.split_first()?;
    Some((*ver, rest.to_vec()))
}

/// The address an output pays, as the arcade names it: P2PKH and P2PK with the chain's
/// pubkeyhash version, P2SH with its scripthash version. None for everything else.
pub fn address_of(script: &[u8], p: &ChainParams) -> Option<String> {
    match classify(script) {
        ScriptKind::P2pkh(h) => Some(base58check(p.p2pkh_version, &h)),
        ScriptKind::P2pk(k) => Some(base58check(p.p2pkh_version, &hash160(&k))),
        ScriptKind::P2sh(h) => Some(base58check(p.p2sh_version, &h)),
        _ => None,
    }
}

/// Only P2PKH / P2SH count as a sender (tx.py determine_sender).
pub fn sender_address_of(script: &[u8], p: &ChainParams) -> Option<String> {
    match classify(script) {
        ScriptKind::P2pkh(h) => Some(base58check(p.p2pkh_version, &h)),
        ScriptKind::P2sh(h) => Some(base58check(p.p2sh_version, &h)),
        _ => None,
    }
}

/// scriptPubKey for a P2PKH address string.
pub fn p2pkh_script(h: &[u8; 20]) -> Vec<u8> {
    let mut s = vec![0x76, 0xa9, 0x14];
    s.extend_from_slice(h);
    s.extend_from_slice(&[0x88, 0xac]);
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcade_chain::params::{DIGIBYTE_MAIN, LITECOIN_MAIN};

    /// Conformance vectors from the existing arcade (tests/conformance/vectors.json):
    /// H160 = sha256(b"conformance hash160")[:20].
    #[test]
    fn conformance_addresses() {
        let h: [u8; 20] = Sha256::digest(b"conformance hash160")[..20].try_into().unwrap();
        assert_eq!(address_of(&p2pkh_script(&h), &DIGIBYTE_MAIN).unwrap(), "DRTVyZzxyvPNZdh8HhCaCipnXVegVJPaY1");
        assert_eq!(address_of(&p2pkh_script(&h), &LITECOIN_MAIN).unwrap(), "LfYMhXN9mAj9HSCgjFCJvyiwraHfLShFTN");
        let mut p2sh = vec![0xa9, 0x14];
        p2sh.extend_from_slice(&h);
        p2sh.push(0x87);
        assert_eq!(address_of(&p2sh, &DIGIBYTE_MAIN).unwrap(), "ShcQU9qUQsgHYwHz6YC6CrokJ99oyQCgQw");
        assert_eq!(address_of(&p2sh, &LITECOIN_MAIN).unwrap(), "MUDZfjxjBXetvJUrn5rwuEGX7aoYmW1q62");
        assert_eq!(decode_base58check("DRTVyZzxyvPNZdh8HhCaCipnXVegVJPaY1").unwrap(), (30, h.to_vec()));
    }
}
