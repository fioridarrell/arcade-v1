//! How much of the computer the arcade takes (Martin 2026-11-20: a fresh Windows install used 90% of a friend's CPU;
//! "we need to chill out a bit"). One setting, kept in `<home>/cpu`, read at start and changed from Options:
//!
//!   low       the lowest priority (nice 19; Windows idle), a quarter of the cores for background work
//!   balanced  low priority (nice 10; Windows below normal), half the cores: the default
//!   full      normal priority, every core (as before 2026-11-20)
//!
//! Priority means the owner's own programs always come first; the core share caps the work that would otherwise use
//! every core: sealing, and the ARCADE chain's proofs of time, catalog and fraud watch (`rest_for`, `proof_threads`).
//! ARCADE_NICE, when set, still decides the priority.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Low,
    Balanced,
    Full,
}

impl Level {
    pub fn name(self) -> &'static str {
        match self {
            Level::Low => "low",
            Level::Balanced => "balanced",
            Level::Full => "full",
        }
    }
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim() {
            "low" => Some(Level::Low),
            "balanced" => Some(Level::Balanced),
            "full" => Some(Level::Full),
            _ => None,
        }
    }
    /// The nice value (0..=19) this level runs at.
    pub fn nice(self) -> i32 {
        match self {
            Level::Low => 19,
            Level::Balanced => 10,
            Level::Full => 0,
        }
    }
    /// Cores background work may use, of `all`: at least one.
    pub fn cores_of(self, all: usize) -> usize {
        match self {
            Level::Low => (all / 4).max(1),
            Level::Balanced => (all / 2).max(1),
            Level::Full => all.max(1),
        }
    }
}

static HOME: OnceLock<PathBuf> = OnceLock::new();
/// The level in force (0 low, 1 balanced, 2 full).
static LEVEL: AtomicUsize = AtomicUsize::new(1);

fn file(home: &Path) -> PathBuf {
    home.join("cpu")
}

fn level_of(n: usize) -> Level {
    match n {
        0 => Level::Low,
        2 => Level::Full,
        _ => Level::Balanced,
    }
}

fn index(l: Level) -> usize {
    match l {
        Level::Low => 0,
        Level::Balanced => 1,
        Level::Full => 2,
    }
}

/// The level in force now.
pub fn level() -> Level {
    level_of(LEVEL.load(Ordering::Relaxed))
}

/// Every logical core of this machine.
pub fn all_cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

/// Cores background work may use now (sealing's threads; estimates of how long it takes).
pub fn cores() -> usize {
    level().cores_of(all_cores())
}

/// The ARCADE chain's own background loops that may each keep a core busy: this arcade's proofs of time, the catalog
/// and the fraud watch (sealing has its own core share).
const HEAVY_LOOPS: f64 = 3.0;

/// How long a background loop rests after `worked` of work, so that the three heavy loops together use about the
/// level's share of the cores (2026-11-21, Martin: the level covers all ARCADE work; a 4-thread laptop ran them at
/// ~380%). Nothing on a machine with cores to spare (3 shared cores or more), or at full.
pub fn rest_for(worked: std::time::Duration) -> std::time::Duration {
    if level() == Level::Full {
        return std::time::Duration::ZERO;
    }
    let duty = (cores() as f64 / HEAVY_LOOPS).min(1.0);
    if duty >= 1.0 {
        return std::time::Duration::ZERO;
    }
    worked.mul_f64(1.0 / duty - 1.0).min(std::time::Duration::from_secs(600))
}

/// This arcade's own proofs of time at once: one at the low level, else two (the tip's and the parent raced on).
pub fn proof_threads() -> usize {
    if level() == Level::Low { 1 } else { 2 }
}

/// At start, before other threads exist (they inherit the priority): the level kept in `home`, else balanced.
pub fn start(home: &Path) {
    let _ = HOME.set(home.to_path_buf());
    let l = std::fs::read_to_string(file(home)).ok().and_then(|s| Level::parse(&s)).unwrap_or(Level::Balanced);
    LEVEL.store(index(l), Ordering::Relaxed);
    if std::env::var("ARCADE_NICE").is_err() {
        crate::set_nice(l.nice());
        // A thread started before this (glibc's memory trimmer) gets it too.
        crate::renice_all(0, l.nice());
    }
}

/// cpu.status: the level, what it means here, and the levels there are.
pub fn status() -> serde_json::Value {
    let l = level();
    let levels: Vec<serde_json::Value> = [Level::Low, Level::Balanced, Level::Full].iter().map(|x| serde_json::json!({"level": x.name(), "nice": x.nice(), "cores": x.cores_of(all_cores())})).collect();
    serde_json::json!({
        "level": l.name(), "cores": cores(), "all_cores": all_cores(), "nice": l.nice(),
        "nice_from_env": std::env::var("ARCADE_NICE").ok(),
        "levels": levels,
    })
}

/// cpu.set {level}: kept for the next start and put in force now. A lower priority applies to every running thread at
/// once; a higher one may need a restart (raising priority is often refused to an ordinary program).
pub fn set(want: &str) -> Result<serde_json::Value, String> {
    let l = Level::parse(want).ok_or("level: low, balanced or full")?;
    let home = HOME.get().ok_or("the arcade hasn't started yet")?;
    std::fs::write(file(home), l.name()).map_err(|e| e.to_string())?;
    let was = level();
    LEVEL.store(index(l), Ordering::Relaxed);
    let applied = std::env::var("ARCADE_NICE").is_err() && crate::renice_all(was.nice(), l.nice());
    let mut v = status();
    v["applied_now"] = serde_json::json!(applied);
    if !applied {
        v["note"] = serde_json::json!("kept; the new priority takes effect when the arcade next starts");
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_and_their_share() {
        assert_eq!(Level::parse("balanced"), Some(Level::Balanced));
        assert_eq!(Level::parse("turbo"), None);
        assert_eq!(Level::Balanced.cores_of(4), 2);
        assert_eq!(Level::Low.cores_of(4), 1);
        assert_eq!(Level::Low.cores_of(2), 1, "never none");
        assert_eq!(Level::Full.cores_of(20), 20);
        assert_eq!(Level::Balanced.cores_of(1), 1);
        assert!(Level::Low.nice() > Level::Balanced.nice() && Level::Balanced.nice() > Level::Full.nice());
    }
}

#[cfg(test)]
mod pace_tests {
    use super::*;
    #[test]
    fn rest_is_none_at_full_and_with_cores_to_spare() {
        let w = std::time::Duration::from_secs(1);
        LEVEL.store(index(Level::Full), Ordering::Relaxed);
        assert_eq!(rest_for(w), std::time::Duration::ZERO);
        LEVEL.store(index(Level::Low), Ordering::Relaxed);
        // On a machine with 12+ cores low leaves 3 to share: no rest. On a smaller one, rest in proportion.
        let r = rest_for(w);
        if all_cores() / 4 >= 3 { assert_eq!(r, std::time::Duration::ZERO); } else { assert!(r >= std::time::Duration::from_secs(2)); }
        assert_eq!(proof_threads(), 1);
        LEVEL.store(index(Level::Balanced), Ordering::Relaxed);
        assert_eq!(proof_threads(), 2);
    }
}
