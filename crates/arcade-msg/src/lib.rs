//! The original Dogecoin Arcade's on-chain messaging, byte for byte (docs/compat/MSG.md):
//! keys from the 12 words, "arcm" envelopes, libsodium sealed boxes, structured bodies, key
//! announcements, public posts, feed acts and the machine records.

pub mod content;
pub mod envelope;
pub mod keys;
pub mod public;

pub use envelope::{Header, Opened, MAGIC};
pub use keys::MsgKeys;
