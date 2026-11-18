//! Whether this arcade keeps Pepecoin testnet (`node.params`, the chain it is built on) in its own store.
//!
//! Martin 2026-11-18: every chain is borrowed until someone installs it, this one too. A new arcade takes no
//! Pepecoin testnet blocks: its wallet, sends and reads go through arcades that keep the chain (`borrow`), for as
//! long as it isn't installed. Installed (Chains & Coins, or ticked in the startup chooser), its own copy syncs,
//! borrowing meanwhile, and takes over once it has caught up. Removed, its blocks go and it is borrowed again.
//!
//! Kept in arcade.mdbx META as "home_installed" = "1" or "0". An arcade from before this (it has blocks or
//! headers of the chain) counts as installed and keeps it.

use crate::node::Node;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

const KEY: &[u8] = b"home_installed";

/// 0 = not read yet, 1 = installed, 2 = not installed.
static STATE: AtomicU8 = AtomicU8::new(0);

/// Removed a moment ago: its blocks are still being deleted, so it can't be installed again yet.
static REMOVING: AtomicBool = AtomicBool::new(false);

pub fn removing() -> bool {
    REMOVING.load(Ordering::Relaxed)
}

pub fn set_removing(on: bool) {
    REMOVING.store(on, Ordering::Relaxed);
}

/// What a new arcade does when nothing is stored yet: `had_chain` (blocks or headers here already: an arcade from
/// before this) keeps it installed; `forced` (ARCADE_HOME_INSTALLED=1, set ups that need the chain from the start)
/// too; anything else starts borrowed.
pub(crate) fn first_choice(had_chain: bool, forced: bool) -> bool {
    had_chain || forced
}

/// Read (and on the first start, decide) once, at boot, before the chain thread starts.
pub fn init(node: &Node) {
    let stored = node.app.store.read(|r| r.get(arcade_app::META, KEY)).ok().flatten();
    let on = match stored.as_deref() {
        Some(b"1") => true,
        Some(_) => false,
        None => {
            let c = node.params.chain_no;
            let tip = node.chain.tip(c).ok().flatten().map(|t| t.height).unwrap_or(0);
            let headers = node.chain.best_header(c).ok().flatten().is_some();
            let forced = std::env::var("ARCADE_HOME_INSTALLED").is_ok_and(|v| v == "1");
            let on = first_choice(tip > 0 || headers, forced);
            let _ = node.app.store.write(|w| w.put(arcade_app::META, KEY, if on { b"1" } else { b"0" }));
            if !on {
                let _ = node.app.event("chains", &format!("{} is borrowed from arcades that keep it; install it in Chains & Coins to keep its blocks here", node.params.name));
            }
            on
        }
    };
    STATE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
}

/// Whether the home chain's blocks are kept here (synced, mined, indexed).
pub fn installed() -> bool {
    STATE.load(Ordering::Relaxed) != 2
}

/// chains.add / chains.remove for the home chain. Returns whether it changed.
pub fn set(node: &Node, on: bool) -> Result<bool, String> {
    if installed() == on {
        return Ok(false);
    }
    node.app.store.write(|w| w.put(arcade_app::META, KEY, if on { b"1" } else { b"0" })).map_err(|e| e.to_string())?;
    STATE.store(if on { 1 } else { 2 }, Ordering::Relaxed);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_arcade_borrows_the_home_chain_and_an_old_one_keeps_it() {
        assert!(!first_choice(false, false), "new: borrowed until installed");
        assert!(first_choice(true, false), "blocks or headers here already: installed");
        assert!(first_choice(false, true), "ARCADE_HOME_INSTALLED=1");
    }
}
