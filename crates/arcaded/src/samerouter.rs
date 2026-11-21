//! **Arcades behind one router**: two arcades at home share one public IPv4 address. Each tells the mesh that public
//! address, and a dial from one to the other's public address needs the router to send it back inside (NAT
//! hairpinning), which many home routers don't do: the two never linked directly, only through a relay or a UDP path
//! that kept dropping (Martin 2026-11-20).
//!
//! So an arcade tells a linked arcade where it listens at home (`{"t": "lan", "a": [ip, port]}`), but only when that
//! arcade's address is this arcade's own public address: it is behind the same router. The other side dials that
//! private address only when the same holds for it: the link's address of the sender is one of its own public
//! addresses. Every other private address another arcade names stays refused (meshsvc::heard_dial_ok).
//!
//! The rules for a home address: a private IPv4 (RFC 1918) or link-local one, or an IPv6 unique-local one, of the
//! same family as the public address shared; a port of 1024 or more; never this arcade's own. Its dials are heard
//! dials (MAX_HEARD_DIALS, parked after HEARD_TRIES failures) under the "lan" source.
//!
//! This arcade's public addresses are the ones STUN servers saw it as (udpsvc), remembered for PUBLIC_KEEP.
//! ARCADE_TEST_PUBLIC_IP (a comma-separated list) sets them in closed-network tests, where no address is public.

use crate::node::Node;
use arcade_mesh::link::{Frame, Link, LinkError};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A public address STUN told us is kept this long after the last answer that named it (a STUN round that found
/// nothing doesn't make the arcade forget where it is).
const PUBLIC_KEEP: Duration = Duration::from_secs(3600);
/// A hint from one arcade is acted on at most once in this long.
const HINT_EVERY: Duration = Duration::from_secs(60);
/// Most arcades whose home address is kept.
const MAX_HINTS: usize = 64;

#[derive(Default)]
struct St {
    /// Public IPs STUN saw us as, with when last.
    public: HashMap<IpAddr, Instant>,
    /// Arcade id -> its home address, when it said it, and the public IP its link had then.
    hints: HashMap<String, (SocketAddr, Instant, IpAddr)>,
}

fn st() -> std::sync::MutexGuard<'static, St> {
    static S: OnceLock<Mutex<St>> = OnceLock::new();
    S.get_or_init(Default::default).lock().unwrap()
}

/// Test mode: our public addresses are set (closed-network tests).
fn test_public() -> Option<Vec<IpAddr>> {
    let v = std::env::var("ARCADE_TEST_PUBLIC_IP").ok()?;
    Some(v.split(',').filter_map(|s| s.trim().parse().ok()).collect())
}

/// This arcade's public IPs now: what STUN saw lately (or the test setting).
pub fn our_public_ips() -> Vec<IpAddr> {
    if let Some(v) = test_public() {
        return v;
    }
    let seen = crate::udpsvc::stun_public_ips();
    let mut s = st();
    let now = Instant::now();
    for ip in seen {
        s.public.insert(ip, now);
    }
    s.public.retain(|_, t| t.elapsed() < PUBLIC_KEEP);
    let mut v: Vec<IpAddr> = s.public.keys().copied().collect();
    v.sort();
    v
}

/// The IP of a link address ("ip:port", "[v6]:port"); None for a name (wss://...).
pub fn ip_of(addr: &str) -> Option<IpAddr> {
    if let Ok(a) = addr.parse::<SocketAddr>() {
        return Some(plain(a.ip()));
    }
    addr.parse::<IpAddr>().ok().map(plain)
}

/// An IPv4-mapped IPv6 address as the IPv4 one.
fn plain(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
        v4 => v4,
    }
}

/// A home address: private IPv4 (RFC 1918) or link-local, or IPv6 unique-local (fc00::/7).
pub fn home_ip(ip: &IpAddr) -> bool {
    match plain(*ip) {
        IpAddr::V4(v) => v.is_private() || (v.is_link_local() && v.octets()[3] != 0 && v.octets()[3] != 255),
        IpAddr::V6(v) => (v.segments()[0] & 0xfe00) == 0xfc00,
    }
}

/// Which addresses count as public: meshsvc's rule, or any address in a closed-network test (ARCADE_TEST_PUBLIC_IP).
fn public(ip: &IpAddr) -> bool {
    test_public().is_some() || crate::meshsvc::public_ip(ip)
}

/// Is `peer_ip` (the address a link to another arcade has here) one of `ours`: the other arcade is behind our router?
/// Only a public address counts (`public`).
pub fn same_router(peer_ip: IpAddr, ours: &[IpAddr], public: impl Fn(&IpAddr) -> bool) -> bool {
    let p = plain(peer_ip);
    public(&p) && ours.iter().any(|o| plain(*o) == p)
}

/// May `lan` be dialed as the home address of an arcade whose link here comes from `peer_ip`? Only when that is one
/// of our public addresses (`same_router`), `lan` is a home address of the same family with a port of 1024 or more,
/// and it isn't ours (`me`: our own home address and mesh port).
pub fn hint_ok(peer_ip: IpAddr, ours: &[IpAddr], lan: SocketAddr, me: Option<SocketAddr>, public: impl Fn(&IpAddr) -> bool) -> bool {
    let ip = plain(lan.ip());
    same_router(peer_ip, ours, public)
        && home_ip(&ip)
        && ip.is_ipv4() == plain(peer_ip).is_ipv4()
        && lan.port() >= 1024
        && me.is_none_or(|m| !(plain(m.ip()) == ip && m.port() == lan.port()))
}

/// Our home address and mesh port, as we tell an arcade behind our router.
fn my_home(node: &Node) -> Option<SocketAddr> {
    let port = node.mesh_port?;
    let ip = IpAddr::V4(crate::udpsvc::lan_ip()?);
    home_ip(&ip).then_some(SocketAddr::new(ip, port))
}

/// The "lan" frame for a link whose address is `addr`, when that arcade is behind our router; None otherwise.
pub fn frame(node: &Node, addr: &str) -> Option<Frame> {
    let peer_ip = ip_of(addr)?;
    if !same_router(peer_ip, &our_public_ips(), public) {
        return None;
    }
    let me = my_home(node)?;
    Some(Frame::new(json!({"t": "lan", "a": [me.ip().to_string(), me.port()]})))
}

/// Send our home address on this link if it goes to an arcade behind our router and we haven't yet (`sent`).
pub fn tell(node: &Node, link: &mut Link, addr: &str, sent: &mut bool) -> Result<(), LinkError> {
    if *sent {
        return Ok(());
    }
    if let Some(f) = frame(node, addr) {
        link.send(&f)?;
        *sent = true;
    }
    Ok(())
}

/// The home address in a "lan" frame.
fn parse(h: &Value) -> Option<SocketAddr> {
    let ip: IpAddr = h["a"].get(0)?.as_str()?.parse().ok()?;
    let port = u16::try_from(h["a"].get(1)?.as_u64()?).ok()?;
    Some(SocketAddr::new(plain(ip), port))
}

/// A "lan" frame from arcade `peer`, on a link whose address here is `addr`: its home address is kept and dialed when
/// it is behind our router (`hint_ok`); anything else is dropped.
pub fn on_frame(node: &Node, peer: &str, addr: &str, h: &Value) {
    let (Some(lan), Some(peer_ip)) = (parse(h), ip_of(addr)) else { return };
    if !hint_ok(peer_ip, &our_public_ips(), lan, my_home(node), public) {
        return;
    }
    {
        let mut s = st();
        if s.hints.get(peer).is_some_and(|(a, t, _)| *a == lan && t.elapsed() < HINT_EVERY) {
            return;
        }
        if s.hints.len() >= MAX_HINTS && !s.hints.contains_key(peer) {
            let oldest = s.hints.iter().min_by_key(|(_, x)| x.1).map(|(k, _)| k.clone());
            if let Some(k) = oldest {
                s.hints.remove(&k);
            }
        }
        s.hints.insert(peer.to_string(), (lan, Instant::now(), peer_ip));
    }
    remember(node, peer, lan, peer_ip);
    if direct(node, peer) {
        return;
    }
    let _ = node.app.event("mesh", &format!("{} is behind our router: dialing it at home ({lan})", &peer[..peer.len().min(16)]));
    crate::meshsvc::dial_lan(&node.self_arc(), lan.to_string());
}

/// Home addresses arcades behind our router told us, kept across restarts (2026-11-21: the GX10 restarted for an update
/// and never linked to the arcade beside it again: the hint comes only over a link at our public address, a UDP path that
/// didn't come up, and its release files were fetched through a relay that kept timing out). At most MAX_HINTS.
const KEPT: &[u8] = b"samerouter_hints";

fn kept(node: &Node) -> Vec<(String, SocketAddr, IpAddr)> {
    let raw = node.app.store.read(|r| r.get(arcade_app::META, KEPT)).ok().flatten();
    let v: Value = raw.and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
    v.as_array().into_iter().flatten().filter_map(|e| {
        Some((e["peer"].as_str()?.to_string(), e["lan"].as_str()?.parse().ok()?, e["via"].as_str()?.parse().ok()?))
    }).collect()
}

fn remember(node: &Node, peer: &str, lan: SocketAddr, via: IpAddr) {
    let mut all: Vec<(String, SocketAddr, IpAddr)> = kept(node).into_iter().filter(|(p, _, _)| p != peer).collect();
    all.insert(0, (peer.to_string(), lan, via));
    all.truncate(MAX_HINTS);
    let v: Vec<Value> = all.iter().map(|(p, l, i)| json!({"peer": p, "lan": l.to_string(), "via": i.to_string()})).collect();
    let _ = node.app.store.write(|w| w.put(arcade_app::META, KEPT, json!(v).to_string().as_bytes()));
}

/// After a start: once STUN has told us our public address (up to KEPT_WAIT), the home address of each arcade kept
/// above is dialed again when it is still behind our router (its public address then is still ours).
const KEPT_WAIT: Duration = Duration::from_secs(600);
pub fn start(node: std::sync::Arc<Node>) {
    let saved = kept(&node);
    if saved.is_empty() {
        return;
    }
    let _ = std::thread::Builder::new().name("same router".into()).spawn(move || {
        let t = Instant::now();
        while our_public_ips().is_empty() && t.elapsed() < KEPT_WAIT {
            std::thread::sleep(Duration::from_secs(5));
        }
        let ours = our_public_ips();
        for (peer, lan, via) in saved {
            if !same_router(via, &ours, public) || direct(&node, &peer) {
                continue;
            }
            st().hints.insert(peer.clone(), (lan, Instant::now(), via));
            let _ = node.app.event("mesh", &format!("{} was behind our router before this start: dialing it at home ({lan})", &peer[..peer.len().min(16)]));
            crate::meshsvc::dial_lan(&node, lan.to_string());
        }
    });
}

/// The home address arcade `id` told us, while it is still behind our router (its public address then is still ours).
pub fn hint_for(id: &str) -> Option<String> {
    let (lan, _, peer_ip) = *st().hints.get(id)?;
    same_router(peer_ip, &our_public_ips(), public).then(|| lan.to_string())
}

/// Whether a link to `id` at a home address is up (no need to dial it there again).
fn direct(node: &Node, id: &str) -> bool {
    node.links.lock().unwrap().values().any(|l| l.node_id == id && ip_of(&l.addr).is_some_and(|ip| home_ip(&ip)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }
    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }
    /// The mesh's rule, with the documentation ranges these tests use (203.0.113.0/24, 198.51.100.0/24,
    /// 2001:db8::/32) taken as public.
    fn pub_(ip: &IpAddr) -> bool {
        let doc = match ip {
            IpAddr::V4(v) => matches!(v.octets(), [203, 0, 113, _] | [198, 51, 100, _]),
            IpAddr::V6(v) => v.segments()[..2] == [0x2001, 0xdb8],
        };
        doc || crate::meshsvc::public_ip(ip)
    }
    /// Closed-network tests: any address.
    fn any(_: &IpAddr) -> bool {
        true
    }

    /// Behind one router: the other arcade's link comes from our own public address.
    #[test]
    fn same_router_only_when_the_public_address_is_ours() {
        let ours = [ip("203.0.113.9")];
        assert!(same_router(ip("203.0.113.9"), &ours, pub_));
        assert!(same_router(ip("::ffff:203.0.113.9"), &ours, pub_), "an IPv4-mapped form is the same address");
        assert!(!same_router(ip("203.0.113.10"), &ours, pub_), "another public address");
        assert!(!same_router(ip("203.0.113.9"), &[], pub_), "we don't know our public address");
        // A private or loopback address is never "ours" in the mesh (only in a closed-network test).
        assert!(!same_router(ip("192.168.1.5"), &[ip("192.168.1.5")], pub_));
        assert!(!same_router(ip("127.0.0.1"), &[ip("127.0.0.1")], pub_));
        assert!(same_router(ip("127.0.0.1"), &[ip("127.0.0.1")], any));
    }

    /// The home address is dialed only for an arcade behind our router, and only a home address of the same family.
    #[test]
    fn a_home_address_is_dialed_only_behind_our_router() {
        let ours = [ip("203.0.113.9")];
        let me = Some(sa("192.168.1.10:8431"));
        let peer = ip("203.0.113.9");
        assert!(hint_ok(peer, &ours, sa("192.168.1.20:8421"), me, pub_));
        assert!(hint_ok(peer, &ours, sa("10.0.0.7:8421"), me, pub_));
        assert!(hint_ok(peer, &ours, sa("172.16.3.4:8421"), me, pub_));
        assert!(hint_ok(peer, &ours, sa("169.254.10.20:8421"), me, pub_), "link-local");
        // Refused: another public address said it (any other arcade's word for a private address).
        assert!(!hint_ok(ip("198.51.100.7"), &ours, sa("192.168.1.20:8421"), me, pub_));
        // Refused: not a home address.
        assert!(!hint_ok(peer, &ours, sa("198.51.100.7:8421"), me, pub_), "public");
        assert!(!hint_ok(peer, &ours, sa("127.0.0.1:8421"), me, pub_), "loopback");
        assert!(!hint_ok(peer, &ours, sa("100.64.0.1:8421"), me, pub_), "shared (CGNAT)");
        assert!(!hint_ok(peer, &ours, sa("0.0.0.0:8421"), me, pub_), "unspecified");
        assert!(!hint_ok(peer, &ours, sa("224.0.0.1:8421"), me, pub_), "multicast");
        assert!(!hint_ok(peer, &ours, sa("255.255.255.255:8421"), me, pub_), "broadcast");
        assert!(!hint_ok(peer, &ours, sa("169.254.255.255:8421"), me, pub_), "link-local broadcast");
        // Refused: a low port (a router's admin page, say), port 0.
        assert!(!hint_ok(peer, &ours, sa("192.168.1.1:80"), me, pub_));
        assert!(!hint_ok(peer, &ours, sa("192.168.1.20:0"), me, pub_));
        // Refused: ours.
        assert!(!hint_ok(peer, &ours, sa("192.168.1.10:8431"), me, pub_));
        assert!(hint_ok(peer, &ours, sa("192.168.1.10:8421"), me, pub_), "another arcade on this machine");
        // Refused: another family than the shared public address.
        assert!(!hint_ok(peer, &ours, sa("[fd12:3456::7]:8421"), me, pub_));
        let ours6 = [ip("2001:db8::1")];
        assert!(hint_ok(ip("2001:db8::1"), &ours6, sa("[fd12:3456::7]:8421"), None, pub_), "IPv6 unique-local");
        assert!(!hint_ok(ip("2001:db8::1"), &ours6, sa("192.168.1.20:8421"), None, pub_), "IPv4 home for an IPv6 public");
        assert!(!hint_ok(ip("2001:db8::1"), &ours6, sa("[2001:db8::2]:8421"), None, pub_), "IPv6 not unique-local");
        assert!(!hint_ok(ip("2001:db8::1"), &ours6, sa("[fe80::1]:8421"), None, pub_), "IPv6 link-local (no scope to dial it by)");
        // The mesh's own rule: a documentation address isn't public, so never "ours" outside a test.
        assert!(!hint_ok(peer, &ours, sa("192.168.1.20:8421"), me, crate::meshsvc::public_ip));
    }

    #[test]
    fn link_addresses_give_their_ip() {
        assert_eq!(ip_of("203.0.113.9:8421"), Some(ip("203.0.113.9")));
        assert_eq!(ip_of("[::ffff:203.0.113.9]:8421"), Some(ip("203.0.113.9")));
        assert_eq!(ip_of("[2001:db8::1]:8421"), Some(ip("2001:db8::1")));
        assert_eq!(ip_of("wss://arcade.example.com/mesh"), None);
        assert_eq!(parse(&json!({"a": ["192.168.1.20", 8421]})), Some(sa("192.168.1.20:8421")));
        assert_eq!(parse(&json!({"a": ["192.168.1.20", 70000]})), None);
        assert_eq!(parse(&json!({"a": "192.168.1.20:8421"})), None);
    }
}
