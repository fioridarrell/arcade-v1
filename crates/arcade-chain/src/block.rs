//! Blocks: header, optional AuxPoW, transactions, and any trailing extension bytes.

use crate::auxpow::AuxPow;
use crate::encode::{write_varint, DecodeError, Reader};
use crate::format::Format;
use crate::hash::{merkle_root, Hash256};
use crate::header::Header;
use crate::tx::Tx;

#[derive(Clone, Debug)]
pub struct Block {
    /// Block id (sha256d of the header, or scrypt on old Kotia headers).
    pub id: Hash256,
    pub header: Header,
    pub auxpow: Option<AuxPow>,
    pub txs: Vec<Tx>,
    /// Block signature (PoS chains); None where the format has none.
    pub sig: Option<Vec<u8>>,
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
    /// Parse with Bitcoin/Dogecoin layout (AuxPoW when the header says so).
    pub fn parse(bytes: &[u8]) -> Result<Block, BlockError> {
        Self::parse_with(bytes, &Format { auxpow: true, ..Format::BITCOIN })
    }

    pub fn parse_with(bytes: &[u8], fmt: &Format) -> Result<Block, BlockError> {
        let mut r = Reader::new(bytes);
        let header = Header::decode(&mut r)?;
        let auxpow = if fmt.has_auxpow(&header) { Some(AuxPow::decode(&mut r)?) } else { None };
        let n = r.len()?;
        let mut txs = Vec::with_capacity(n.min(100_000));
        for _ in 0..n {
            txs.push(Tx::decode_with(&mut r, fmt)?);
        }
        let sig = if fmt.block_sig { Some(r.var_bytes()?.to_vec()) } else { None };
        let extra = r.bytes(r.remaining())?.to_vec();
        Ok(Block { id: fmt.block_id(&header), header, auxpow, txs, sig, extra })
    }

    pub fn hash(&self) -> Hash256 {
        self.id
    }

    /// Proof of stake: the second transaction is a coinstake (Kotia).
    pub fn is_proof_of_stake(&self) -> bool {
        self.txs.get(1).map(|t| t.is_coinstake()).unwrap_or(false)
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
        if let Some(sig) = &self.sig {
            write_varint(&mut o, sig.len() as u64);
            o.extend_from_slice(sig);
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

    /// Kotia genesis, rebuilt by the Arcade v1.0 session from upstream CreateGenesisBlock.
    #[test]
    fn kotia_genesis_block() {
        let header = "01000000000000000000000000000000000000000000000000000000000000000000000013154d160a2cca0221b412d99a34545d9ab4c457c81b8d1c1ca526be311c70f0c0af8261ffff0f1e030f0000";
        let coinbase = "01000000c0af8261010000000000000000000000000000000000000000000000000000000000000000ffffffff1500012a116f6d6e69612076696e63697420616d6f72ffffffff0100000000000000000000000000";
        let raw = hex::decode(format!("{header}01{coinbase}00")).unwrap();
        let b = Block::parse_with(&raw, &Format::POS_TIMED).unwrap();
        b.check().unwrap();
        assert_eq!(
            b.hash().to_hex(),
            "000008e4586479cd9dc5832b262414a60f152381e371cf7940ffe9f5aa4b80a0",
            "version-1 Kotia blocks are identified by scrypt"
        );
        assert_eq!(
            b.header.merkle_root.to_hex(),
            "f0701c31be26a51c1c8d1bc857c4b49a5d54349ad912b42102ca2c0a164d1513"
        );
        assert_eq!(b.sig.as_deref(), Some(&[][..]));
        assert!(!b.is_proof_of_stake());
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
