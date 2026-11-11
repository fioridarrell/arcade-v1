//! Per-chain wire format: the few ways chains differ in how blocks and
//! transactions are laid out and identified.

use crate::hash::{sha256d, Hash256};
use crate::header::Header;
use crate::pow::{pow_hash, PowAlgo};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    /// Merge mining exists: a header with version bit 8 is followed by AuxPoW data.
    /// (On DigiByte and Kotia bit 8 means nothing of the sort.)
    pub auxpow: bool,
    /// PPCoin-style `nTime` u32 after the transaction version (Kotia).
    pub tx_time: bool,
    /// Block ends with a length-prefixed signature (PoS chains, Kotia); also an extra
    /// varint per entry in a `headers` message.
    pub block_sig: bool,
    /// Block id is scrypt(header) for header versions below this, sha256d above (Kotia: 7).
    /// 0 = always sha256d.
    pub scrypt_id_below_version: i32,
    /// SegWit serialization can appear (ask peers for witness blocks).
    pub segwit: bool,
    /// Litecoin MWEB: ask peers for blocks with the extension block.
    pub mweb: bool,
    /// The stored 80-byte header stands in for a block of another shape (the ARCADE chain,
    /// crates/arcade-arc): the block's id is carried in its merkle root field.
    pub carried_id: bool,
}

impl Format {
    /// Bitcoin layout, as Pepecoin/Dogecoin without merge mining.
    pub const BITCOIN: Format = Format {
        auxpow: false,
        tx_time: false,
        block_sig: false,
        scrypt_id_below_version: 0,
        segwit: true,
        mweb: false,
        carried_id: false,
    };
    /// Litecoin: SegWit plus the MWEB extension block.
    pub const LITECOIN: Format = Format { mweb: true, ..Format::BITCOIN };
    /// AuxPoW and SegWit together (Bellscoin 3: both from block 144,000).
    pub const AUXPOW_SEGWIT: Format = Format { auxpow: true, ..Format::BITCOIN };
    /// AuxPoW with Litecoin's SegWit and MWEB (Junkcoin 4: AuxPoW from 173,000, MWEB from 1,165,000).
    pub const AUXPOW_MWEB: Format = Format { auxpow: true, mweb: true, ..Format::BITCOIN };
    /// Dogecoin-family: AuxPoW, no SegWit.
    pub const AUXPOW: Format = Format { auxpow: true, segwit: false, ..Format::BITCOIN };
    /// Blackcoin/PPCoin-style proof of stake (Kotia).
    pub const POS_TIMED: Format = Format {
        auxpow: false,
        tx_time: true,
        block_sig: true,
        scrypt_id_below_version: 7,
        segwit: false,
        mweb: false,
        carried_id: false,
    };
    /// The ARCADE chain as the store keeps it: its transactions are laid out as Dogecoin's.
    pub const ARCADE: Format = Format { segwit: false, carried_id: true, ..Format::BITCOIN };

    /// The block id of a header on this chain.
    pub fn block_id(&self, h: &Header) -> Hash256 {
        if self.carried_id {
            return h.merkle_root;
        }
        if h.version < self.scrypt_id_below_version {
            pow_hash(PowAlgo::Scrypt, &h.encode())
        } else {
            sha256d(&h.encode())
        }
    }

    pub fn has_auxpow(&self, h: &Header) -> bool {
        self.auxpow && h.is_auxpow()
    }
}

/// getdata inventory flags (BIP144; Litecoin LIP-0006).
pub const MSG_WITNESS_FLAG: u32 = 1 << 30;
pub const MSG_MWEB_FLAG: u32 = 1 << 29;

impl Format {
    /// Inventory type to request full blocks exactly as mined.
    pub fn block_inv(&self) -> u32 {
        let mut t = 2; // MSG_BLOCK
        if self.segwit {
            t |= MSG_WITNESS_FLAG;
        }
        if self.mweb {
            t |= MSG_MWEB_FLAG;
        }
        t
    }
}

impl Default for Format {
    fn default() -> Self {
        Format::BITCOIN
    }
}
