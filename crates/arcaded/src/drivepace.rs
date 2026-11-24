//! Bulk install work yields the drive to chains following their tip (docs/INITSYNC.md, "Sharing
//! a drive"). An init install stores blocks and builds its indexes as fast as the drive allows;
//! on a spinning disk that starves a chain on the same drive that was at its tip: each new
//! block's reads wait behind the install, and the chain falls behind for hours.
//!
//! A monitor thread looks at every running chain every few seconds: a chain that was at its tip
//! in the last hours and is now more than LAG blocks behind marks its drive as starved. Bulk
//! work on a starved drive (arcade_chaindb::pace::wait, between units of work) waits, up to
//! MAX_WAIT at a time, so an install never stops for good (a chain may lag for other reasons:
//! no peers, a stuck source). ARCADE_DRIVE_PACE=0 turns it off.

use crate::node::Node;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Blocks behind before a tip chain counts as starved.
const LAG: u32 = 2;
/// A chain counts only if it was at its tip this recently (a chain still catching up from far
/// behind is an install of its own, not one to yield to).
const AT_TIP_WITHIN: Duration = Duration::from_secs(6 * 3600);
/// The longest one wait lasts; then one unit of work goes ahead.
const MAX_WAIT: Duration = Duration::from_secs(30);
const EVERY: Duration = Duration::from_secs(5);

#[derive(Default)]
struct State {
    /// Chain number -> the device its file is on.
    dev: HashMap<u16, u64>,
    /// Device -> why it is starved ("btc 7 blocks behind").
    starved: HashMap<u64, String>,
    /// Chain number -> when it was last at its tip.
    at_tip: HashMap<u16, Instant>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

pub fn on() -> bool {
    on_of(std::env::var("ARCADE_DRIVE_PACE").ok().as_deref())
}

fn on_of(v: Option<&str>) -> bool {
    !matches!(v.map(|s| s.trim().to_ascii_lowercase()).as_deref(), Some("0" | "off" | "no" | "false"))
}

/// Starts the monitor and installs the wait as arcade_chaindb's pace hook.
pub fn start(node: Arc<Node>) {
    if !on() {
        return;
    }
    arcade_chaindb::pace::set_hook(wait);
    std::thread::Builder::new()
        .name("drive pace".into())
        .spawn(move || loop {
            look(&node);
            std::thread::sleep(EVERY);
        })
        .unwrap();
}

#[cfg(unix)]
fn dev_of(p: &std::path::Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(p).ok().map(|m| m.dev())
}

#[cfg(not(unix))]
fn dev_of(_: &std::path::Path) -> Option<u64> {
    None
}

/// How far a chain following its tip is behind: its best header less its tip. A peer's claimed
/// height is not used: a peer can claim any height.
pub fn lag_of(tip: u32, best_header: u32) -> u32 {
    best_header.saturating_sub(tip)
}

/// One running chain as the monitor sees it: number, key, device, in init, paused, blocks behind.
type Seen<'a> = (u16, &'a str, Option<u64>, bool, bool, u32);

/// The starved drives, from each chain's device, init state, pause and lag. Pure, for tests.
pub fn starved_devs(chains: &[Seen<'_>], at_tip: &mut HashMap<u16, Instant>, now: Instant) -> HashMap<u64, String> {
    let mut out: HashMap<u64, String> = HashMap::new();
    for &(c, key, dev, init, paused, lag) in chains {
        if init || paused {
            continue;
        }
        if lag <= 1 {
            at_tip.insert(c, now);
            continue;
        }
        let recent = at_tip.get(&c).is_some_and(|t| now.duration_since(*t) < AT_TIP_WITHIN);
        if let (true, true, Some(d)) = (lag > LAG, recent, dev) {
            out.entry(d).and_modify(|w| w.push_str(&format!(", {key} {lag} behind"))).or_insert(format!("{key} {lag} blocks behind"));
        }
    }
    out
}

fn look(node: &Node) {
    let runs: Vec<_> = node.extra.lock().unwrap().values().cloned().collect();
    let mut rows = Vec::new();
    let mut devs = HashMap::new();
    for run in &runs {
        let p = run.params;
        let c = p.chain_no;
        let dev = dev_of(&node.chain.own_dir(c));
        if let Some(d) = dev {
            devs.insert(c, d);
        }
        let init = node.chain.init_active(c);
        let paused = run.paused.load(Ordering::Relaxed);
        let lag = if init || paused {
            0
        } else {
            let tip = node.chain.tip(c).ok().flatten().map_or(0, |t| t.height);
            let best = node.chain.best_header(c).ok().flatten().and_then(|h| node.chain.header(c, &h).ok().flatten()).map_or(0, |r| r.height);
            lag_of(tip, best)
        };
        rows.push((c, p.key, dev, init, paused, lag));
    }
    let now = Instant::now();
    let mut g = STATE.lock().unwrap();
    let st = g.get_or_insert_with(State::default);
    let starved = starved_devs(&rows, &mut st.at_tip, now);
    for (d, why) in &starved {
        if !st.starved.contains_key(d) {
            let _ = node.app.event("chains", &format!("installs on the drive of {why} now yield to it (ARCADE_DRIVE_PACE)"));
        }
    }
    for d in st.starved.keys() {
        if !starved.contains_key(d) {
            let _ = node.app.event("chains", "installs on a drive go on at full speed: the chains following their tip there caught up");
        }
    }
    st.starved = starved;
    st.dev = devs;
}

/// The pace hook: while chain `c`'s drive is starved, wait (at most MAX_WAIT).
pub fn wait(c: u16) {
    let t = Instant::now();
    loop {
        let starved = STATE.lock().unwrap().as_ref().is_some_and(|st| st.dev.get(&c).is_some_and(|d| st.starved.contains_key(d)));
        if !starved || t.elapsed() >= MAX_WAIT {
            return;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// For chain status: why installs on this chain's drive are waiting, if they are.
pub fn note(c: u16) -> Option<String> {
    let g = STATE.lock().unwrap();
    let st = g.as_ref()?;
    st.dev.get(&c).and_then(|d| st.starved.get(d)).map(|w| format!("yielding the drive to {w}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tip_chain_that_falls_behind_starves_its_drive_only() {
        let mut at = HashMap::new();
        let t0 = Instant::now();
        // btc at its tip, doge installing on the same drive (1), dgb on another drive (2).
        let rows = [(1u16, "btc", Some(1u64), false, false, 0u32), (2, "doge", Some(1), true, false, 0), (3, "dgb", Some(2), false, false, 0)];
        assert!(starved_devs(&rows, &mut at, t0).is_empty());
        // btc falls 7 behind: drive 1 starved; dgb's drive is not.
        let rows = [(1u16, "btc", Some(1u64), false, false, 7u32), (2, "doge", Some(1), true, false, 0), (3, "dgb", Some(2), false, false, 0)];
        let s = starved_devs(&rows, &mut at, t0 + Duration::from_secs(60));
        assert_eq!(s.len(), 1);
        assert!(s[&1].contains("btc 7 blocks behind"));
        // Two behind is within the slack.
        let rows = [(1u16, "btc", Some(1u64), false, false, 2u32)];
        assert!(starved_devs(&rows, &mut at, t0 + Duration::from_secs(90)).is_empty());
        // A chain never at its tip (still catching up) or paused or in init never starves a drive.
        let mut fresh = HashMap::new();
        let rows = [(4u16, "ltc", Some(1u64), false, false, 500_000u32), (5, "pep", Some(1), false, true, 50), (6, "lky", Some(1), true, false, 50)];
        assert!(starved_devs(&rows, &mut fresh, t0).is_empty());
        // At its tip long ago: no longer counted.
        let rows = [(1u16, "btc", Some(1u64), false, false, 7u32)];
        assert!(starved_devs(&rows, &mut at, t0 + AT_TIP_WITHIN + Duration::from_secs(61)).is_empty());
    }

    #[test]
    fn lag_is_headers_over_tip() {
        assert_eq!(lag_of(977_313, 977_321), 8);
        assert_eq!(lag_of(977_313, 977_313), 0);
        assert_eq!(lag_of(10, 5), 0);
    }

    #[test]
    fn the_switch() {
        assert!(on_of(None) && on_of(Some("1")));
        assert!(!on_of(Some("0")) && !on_of(Some("off")) && !on_of(Some(" NO ")));
    }
}
