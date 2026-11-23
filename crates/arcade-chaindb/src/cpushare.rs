//! The share of the machine heavy chain work may use (2026-11-23: operators who picked several chains on a fresh install
//! saw every core at 100%). arcaded sets it from its CPU level (`arcaded::cpu`): at the default level heavy work gets
//! half the cores, and a governor there watches the whole process and says when it is over its share.
//!
//! Two parts, both used by every heavy pool here and in arcaded (script checks, block parsing and warming, address
//! builds, block downloads, sealing):
//! - `threads(want)`: a pool's thread count, never more than the share;
//! - `pace()`: called between pieces of work; while the process is over its share it waits a moment, so the whole
//!   arcade stays near the cap however many pools run at once (several chains, sealing beside a sync).
//!
//! No share set (tests, tools, the unthrottled level): every core, and `pace` never waits.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Cores heavy work may use (0: no cap).
static SHARE: AtomicUsize = AtomicUsize::new(0);
/// Whether the process is over its share right now (set by the governor).
static OVER: AtomicBool = AtomicBool::new(false);

/// The longest one `pace` call waits, so a thread never stalls for long on a busy machine.
const MOST: Duration = Duration::from_secs(2);
/// How often a waiting thread looks again.
const STEP: Duration = Duration::from_millis(25);

/// Every logical core of this machine.
pub fn all_cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// The cap in cores (0: none). Clearing it also clears the over mark.
pub fn set_share(cores: usize) {
    SHARE.store(cores, Ordering::Relaxed);
    if cores == 0 {
        OVER.store(false, Ordering::Relaxed);
    }
}

/// The cap in cores, when there is one.
pub fn share() -> Option<usize> {
    match SHARE.load(Ordering::Relaxed) {
        0 => None,
        n => Some(n),
    }
}

/// Threads for a pool that would use `want`: at most the share, at least one.
pub fn threads(want: usize) -> usize {
    capped(want, share())
}

/// `want` threads under a cap of `share` cores (None: no cap); at least one.
pub fn capped(want: usize, share: Option<usize>) -> usize {
    match share {
        Some(s) => want.min(s).max(1),
        None => want.max(1),
    }
}

/// Set by the governor: whether the process uses more than its share now.
pub fn set_over(over: bool) {
    OVER.store(over && share().is_some(), Ordering::Relaxed);
}

/// Whether the process uses more than its share now.
pub fn over() -> bool {
    OVER.load(Ordering::Relaxed)
}

/// Between pieces of heavy work: while the process is over its share, wait (at most 2 s). Cheap when under it.
pub fn pace() {
    if !OVER.load(Ordering::Relaxed) {
        return;
    }
    let began = Instant::now();
    while OVER.load(Ordering::Relaxed) && began.elapsed() < MOST {
        std::thread::sleep(STEP);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pools_stay_within_the_share() {
        assert_eq!(capped(8, Some(4)), 4);
        assert_eq!(capped(3, Some(4)), 3);
        assert_eq!(capped(8, None), 8);
        assert_eq!(capped(0, Some(4)), 1, "never none");
        assert_eq!(capped(8, Some(1)), 1);
    }
}
