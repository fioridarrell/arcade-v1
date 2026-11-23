//! Each linked arcade's round trip, for the Home tab (`mesh.overview` `ping_ms`, `ping_via`).
//!
//! Timed from the link's own pings (every 15 s both ways, the first one at once): the time from sending a ping to
//! reading its pong. The frames go over whatever carries the link (TCP, its UDP path when it has one, or a relay), so
//! the time is that path's.
//!
//! The original arcade showed a smoothed average of the same thing, and it read far above the network (p90 219 to
//! 364 ms, p99 about 1 s, measured on it in 2026-10): the pong waited for a process busy with other work, and
//! smoothing kept every spike. Busy time only ever adds to a round trip, so this keeps the *least* of the last few
//! pongs, which is the closest to the network's own time. Nothing is measured when asked: a read costs nothing.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Pongs kept per arcade (the least of them is shown).
const SAMPLES: usize = 8;
/// A ping not answered in this long is forgotten.
const PING_WAIT: Duration = Duration::from_secs(60);
/// Pings waiting for their pong, at most (all links together).
const PENDING_MAX: usize = 4096;

struct Rtt {
    samples: VecDeque<u32>, // microseconds
    relayed: bool,
}

#[derive(Default)]
struct St {
    /// Ping "n" -> (the link's peer, sent at).
    pending: HashMap<String, (String, Instant)>,
    by_peer: BTreeMap<String, Rtt>,
}

#[track_caller]
fn st() -> crate::heldlock::Held<'static, St> {
    static S: OnceLock<crate::heldlock::HeldLock<St>> = OnceLock::new();
    S.get_or_init(|| crate::heldlock::HeldLock::new("link rtt", St::default())).lock().unwrap_or_else(|e| e.into_inner())
}

/// A new ping's "n" for a link to `peer`: 8 hex digits, unique among the pings waiting (pongs echo it).
pub fn ping_n(peer: &str) -> String {
    static N: AtomicU32 = AtomicU32::new(0);
    let n = format!("{:08x}", N.fetch_add(1, Ordering::Relaxed) ^ 0x5a5a_0000);
    let now = Instant::now();
    let mut s = st();
    if s.pending.len() >= PENDING_MAX {
        s.pending.retain(|_, (_, at)| now.duration_since(*at) < PING_WAIT);
    }
    if s.pending.len() < PENDING_MAX {
        s.pending.insert(n.clone(), (peer.to_string(), now));
    }
    n
}

/// A pong read on a link to `peer` (`relayed`: the link goes through a relay).
pub fn on_pong(peer: &str, n: &str, relayed: bool) {
    let mut s = st();
    let Some((who, at)) = s.pending.remove(n) else { return };
    if who != peer {
        return;
    }
    let us = at.elapsed().as_micros().min(u32::MAX as u128) as u32;
    note(&mut s, peer, us, relayed);
}

fn note(s: &mut St, peer: &str, us: u32, relayed: bool) {
    let e = s.by_peer.entry(peer.to_string()).or_insert_with(|| Rtt { samples: VecDeque::new(), relayed });
    if e.relayed != relayed {
        // The link changed (a direct link after a relayed one): only the new path's times count.
        e.samples.clear();
        e.relayed = relayed;
    }
    e.samples.push_back(us);
    while e.samples.len() > SAMPLES {
        e.samples.pop_front();
    }
}

/// A link to `peer` ended: its times go with it (another link to it times its own).
pub fn link_down(peer: &str) {
    st().by_peer.remove(peer);
}

/// `peer`'s round trip in ms (one decimal) and whether it is relayed, once a pong came back.
pub fn get(peer: &str) -> Option<(f64, bool)> {
    let s = st();
    let r = s.by_peer.get(peer)?;
    let least = *r.samples.iter().min()?;
    Some(((least as f64 / 100.0).round() / 10.0, r.relayed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_least_of_the_last_pongs_is_shown() {
        let mut s = St::default();
        for us in [40_000, 12_300, 250_000, 900_000] {
            note(&mut s, "p", us, false);
        }
        let least = *s.by_peer["p"].samples.iter().min().unwrap();
        assert_eq!(least, 12_300, "a busy moment (250 ms, 900 ms) never shows as the ping");
        for _ in 0..SAMPLES {
            note(&mut s, "p", 30_000, false);
        }
        assert_eq!(*s.by_peer["p"].samples.iter().min().unwrap(), 30_000, "old pongs leave the window");
        note(&mut s, "p", 80_000, true);
        assert_eq!(s.by_peer["p"].samples.len(), 1, "a relayed path starts its own times");
        assert!(s.by_peer["p"].relayed);
    }

    #[test]
    fn pongs_count_only_for_their_own_ping_and_link() {
        let n = ping_n("peer-a");
        on_pong("peer-b", &n, false);
        assert!(get("peer-b").is_none(), "another link's pong");
        let n = ping_n("peer-a");
        on_pong("peer-a", "ffffffff", false);
        assert!(get("peer-a").is_none(), "a pong for no ping of ours");
        on_pong("peer-a", &n, false);
        let (ms, relayed) = get("peer-a").unwrap();
        assert!(ms >= 0.0 && ms < 1000.0 && !relayed);
        on_pong("peer-a", &n, false); // the same pong twice counts once
        assert_eq!(st().by_peer["peer-a"].samples.len(), 1);
        link_down("peer-a");
        assert!(get("peer-a").is_none());
    }
}
