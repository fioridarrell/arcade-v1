//! Blocks: header, optional AuxPoW, transactions, and any trailing extension bytes.

use crate::auxpow::AuxPow;
use crate::encode::{write_varint, DecodeError, Reader};
use crate::hash::{merkle_root, Hash256};
use crate::header::Header;
use crate::tx::Tx;

#[derive(Clone, Debug)]
pub struct Block {
    pub header: Header,
    pub auxpow: Option<AuxPow>,
    pub txs: Vec<Tx>,
    /// Bytes after the last transaction (e.g. Litecoin MWEB). Empty for most chains.
    pub extra: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum BlockError {
    #[error("decode: {0}")]
    Decode(#[from] DecodeError),
    #[error("block has no transactions")]
    Empty,
    #[error("first transaction is not a coinbase")]
    NoCoinbase,
    #[error("merkle root mismatch: header {header}, computed {computed}")]
    MerkleMismatch { header: Hash256, computed: Hash256 },
}

impl Block {
    pub fn parse(bytes: &[u8]) -> Result<Block, BlockError> {
        let mut r = Reader::new(bytes);
        let header = Header::decode(&mut r)?;
        let auxpow = if header.is_auxpow() { Some(AuxPow::decode(&mut r)?) } else { None };
        let n = r.len()?;
        let mut txs = Vec::with_capacity(n.min(100_000));
        for _ in 0..n {
            txs.push(Tx::decode(&mut r)?);
        }
        let extra = r.bytes(r.remaining())?.to_vec();
        Ok(Block { header, auxpow, txs, extra })
    }

    pub fn hash(&self) -> Hash256 {
        self.header.hash()
    }

    pub fn computed_merkle_root(&self) -> Hash256 {
        let ids: Vec<Hash256> = self.txs.iter().map(|t| t.txid).collect();
        merkle_root(&ids)
    }

    /// Structural checks every stored block must pass. PoW is checked by the header chain.
    pub fn check(&self) -> Result<(), BlockError> {
        let first = self.txs.first().ok_or(BlockError::Empty)?;
        if !first.is_coinbase() {
            return Err(BlockError::NoCoinbase);
        }
        let computed = self.computed_merkle_root();
        if computed != self.header.merkle_root {
            return Err(BlockError::MerkleMismatch { header: self.header.merkle_root, computed });
        }
        Ok(())
    }

    /// Rebuild the exact wire bytes from parts (header, auxpow, txs, extra).
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Vec::new();
        o.extend_from_slice(&self.header.encode());
        if let Some(a) = &self.auxpow {
            o.extend_from_slice(&a.raw);
        }
        write_varint(&mut o, self.txs.len() as u64);
        for t in &self.txs {
            o.extend_from_slice(&t.raw);
        }
        o.extend_from_slice(&self.extra);
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BTC_GENESIS_BLOCK: &str = concat!(
        "0100000000000000000000000000000000000000000000000000000000000000000000003ba3edfd7a7b12b27ac72c3e67768f617fc81bc3888a51323a9fb8aa4b1e5e4a29ab5f49ffff001d1dac2b7c",
        "01",
        "01000000010000000000000000000000000000000000000000000000000000000000000000ffffffff4d04ffff001d0104455468652054696d65732030332f4a616e2f32303039204368616e63656c6c6f72206f6e206272696e6b206f66207365636f6e64206261696c6f757420666f722062616e6b73ffffffff0100f2052a01000000434104678afdb0fe5548271967f1a67130b7105cd6a828e03909a67962e0ea1f61deb649f6bc3f4cef38c4f35504e51ec112de5c384df7ba0b8d578a4c702b6bf11d5fac00000000"
    );

    #[test]
    fn genesis_block_parses_checks_and_roundtrips() {
        let raw = hex::decode(BTC_GENESIS_BLOCK).unwrap();
        let b = Block::parse(&raw).unwrap();
        b.check().unwrap();
        assert!(b.auxpow.is_none());
        assert!(b.extra.is_empty());
        assert_eq!(b.encode(), raw);
    }

    #[test]
    fn tampered_tx_fails_merkle() {
        let mut raw = hex::decode(BTC_GENESIS_BLOCK).unwrap();
        let last = raw.len() - 10;
        raw[last] ^= 1;
        let b = Block::parse(&raw).unwrap();
        assert!(matches!(b.check(), Err(BlockError::MerkleMismatch { .. })));
    }
}
