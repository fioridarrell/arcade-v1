//! A pause point for bulk work (init installs: storing blocks, the tx index, the coin set, the
//! rules replay). The host (arcaded) installs a hook that may sleep a while, e.g. so a chain
//! following its tip on the same drive is not starved of disk reads. With no hook nothing waits.
//! Called between units of work, never inside a read or write transaction.

use std::sync::OnceLock;

type Hook = Box<dyn Fn(u16) + Send + Sync>;
static HOOK: OnceLock<Hook> = OnceLock::new();

/// Installs the hook (once; later calls are ignored).
pub fn set_hook(f: impl Fn(u16) + Send + Sync + 'static) {
    let _ = HOOK.set(Box::new(f));
}

/// Bulk work on chain `c` may go on (after the hook's wait, if any).
pub fn wait(c: u16) {
    if let Some(h) = HOOK.get() {
        h(c);
    }
}
