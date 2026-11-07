//! The 80-byte block header.

use crate::encode::{DecodeError, Reader};
use crate::hash::{sha256d, Hash256};

/// AuxPoW flag in the version field (Dogecoin/Namecoin convention).
pub const VERSION_AUXPOW: i32 = 1 << 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub version: i32,
    pub prev: Hash256,
    pub merkle_root: Hash256,
    pub time: u32,
    pub bits: u32,
    pub nonce: u32,
}

impl Header {
    pub const SIZE: usize = 80;

    pub fn decode(r: &mut Reader) -> Result<Header, DecodeError> {
        Ok(Header {
            version: r.i32()?,
            prev: r.hash()?,
            merkle_root: r.hash()?,
            time: r.u32()?,
            bits: r.u32()?,
            nonce: r.u32()?,
        })
    }

    pub fn parse(bytes: &[u8; 80]) -> Header {
        Self::decode(&mut Reader::new(bytes)).expect("80 bytes is always a header")
    }

    pub fn encode(&self) -> [u8; 80] {
        let mut b = [0u8; 80];
        b[0..4].copy_from_slice(&self.version.to_le_bytes());
        b[4..36].copy_from_slice(&self.prev.0);
        b[36..68].copy_from_slice(&self.merkle_root.0);
        b[68..72].copy_from_slice(&self.time.to_le_bytes());
        b[72..76].copy_from_slice(&self.bits.to_le_bytes());
        b[76..80].copy_from_slice(&self.nonce.to_le_bytes());
        b
    }

    /// Block id: sha256d of the 80 bytes (for scrypt chains too).
    pub fn hash(&self) -> Hash256 {
        sha256d(&self.encode())
    }

    pub fn is_auxpow(&self) -> bool {
        self.version & VERSION_AUXPOW != 0
    }

    /// Merge-mining chain id carried in the version's upper 16 bits.
    pub fn chain_id(&self) -> u32 {
        (self.version as u32) >> 16
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const BTC_GENESIS_HEADER: &str = "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c";

    #[test]
    fn genesis_header_hash() {
        let raw: [u8; 80] = hex::decode(BTC_GENESIS_HEADER).unwrap().try_into().unwrap();
        let h = Header::parse(&raw);
        assert_eq!(h.encode(), raw);
        assert_eq!(
            h.hash().to_hex(),
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
        );
        assert!(!h.is_auxpow());
    }
}
