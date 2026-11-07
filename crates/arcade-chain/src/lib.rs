//! Chain primitives for Arcade V1.0: parsing and checking blocks, headers,
//! transactions and AuxPoW, exactly as they appear on the wire.
//!
//! Nothing here touches storage or the network; it is pure and testable.

pub mod auxpow;
pub mod block;
pub mod encode;
pub mod hash;
pub mod p2p;
pub mod params;
pub mod header;
pub mod pow;
pub mod tx;

pub use block::Block;
pub use encode::{DecodeError, Reader};
pub use hash::Hash256;
pub use header::Header;
pub use tx::{OutPoint, Tx, TxIn, TxOut};
