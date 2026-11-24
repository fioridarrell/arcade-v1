//! `arcade-signer`: a small process that alone holds the wallet's decrypted seed.
//!
//! arcaded keeps public keys and addresses and asks this process to sign. The request travels
//! over a filesystem socket in the arcade's data folder (`signer/signer.sock`). On Linux the
//! socket is mode 0600; on Windows its ACL grants the current user only. Nothing here listens
//! on the mesh port or on HTTP, so a page or another arcade cannot open it.
//!
//! The process starts locked. It decrypts the seed only after `approve` (the desktop window,
//! once the user presses Approve on a card), signs a transaction only when every input, output,
//! amount, address and fee matches that card, and locks again as soon as it has signed or when
//! the approval times out (5 minutes, unless `ARCADE_SIGNER_UNLOCK_SECS` says otherwise).

mod client;
mod protocol;

pub use client::{socket_path, signer_dir, Client};
pub use protocol::{Card, JobIn, JobOut, ShownOutput, SignJob, MAX_FRAME};

#[cfg(feature = "server")]
mod engine;
#[cfg(feature = "server")]
mod seed;
#[cfg(feature = "server")]
mod serve;

#[cfg(feature = "server")]
pub use seed::{import_account, import_phrase};
#[cfg(feature = "server")]
pub use serve::serve;

/// How long an approval stays usable when `ARCADE_SIGNER_UNLOCK_SECS` is unset.
pub const DEFAULT_UNLOCK_SECS: u64 = 300;

/// The approval window. `ARCADE_SIGNER_UNLOCK_SECS` overrides it (1 second through one day).
pub fn unlock_secs() -> u64 {
    match std::env::var("ARCADE_SIGNER_UNLOCK_SECS").ok().and_then(|v| v.parse::<u64>().ok()) {
        Some(n) => n.clamp(1, 86_400),
        None => DEFAULT_UNLOCK_SECS,
    }
}
