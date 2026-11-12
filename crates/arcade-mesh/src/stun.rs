//! STUN Binding (RFC 5389), as the old arcade's mesh/stun.py: requests, answers, and the
//! XOR-MAPPED-ADDRESS they carry. Binding only (no TURN, no credentials).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

pub const COOKIE: u32 = 0x2112_A442;
pub const BINDING_REQUEST: u16 = 0x0001;
pub const BINDING_RESPONSE: u16 = 0x0101;
const XOR_MAPPED: u16 = 0x0020;
const MAPPED: u16 = 0x0001;

/// A Binding request with a fresh transaction id.
pub fn request() -> ([u8; 12], Vec<u8>) {
    let tx: [u8; 12] = crate::random::bytes();
    let mut b = Vec::with_capacity(20);
    b.extend_from_slice(&BINDING_REQUEST.to_be_bytes());
    b.extend_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&COOKIE.to_be_bytes());
    b.extend_from_slice(&tx);
    (tx, b)
}

fn header(p: &[u8]) -> Option<(u16, usize, [u8; 12])> {
    if p.len() < 20 || p[0] & 0xc0 != 0 || u32::from_be_bytes(p[4..8].try_into().ok()?) != COOKIE {
        return None;
    }
    let t = u16::from_be_bytes([p[0], p[1]]);
    let n = u16::from_be_bytes([p[2], p[3]]) as usize;
    if 20 + n > p.len() {
        return None;
    }
    Some((t, n, p[8..20].try_into().ok()?))
}

/// Is this datagram a STUN message (any type)?
pub fn is_stun(p: &[u8]) -> bool {
    header(p).is_some()
}

/// The transaction id of a Binding request, if it is one.
pub fn parse_request(p: &[u8]) -> Option<[u8; 12]> {
    match header(p)? {
        (BINDING_REQUEST, _, tx) => Some(tx),
        _ => None,
    }
}

/// IPv4-mapped IPv6 askers are answered (and reported) as IPv4.
pub fn normalize(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => SocketAddr::new(IpAddr::V4(v4), v6.port()),
            None => a,
        },
        _ => a,
    }
}

/// The Binding success answer telling `asker` where it came from.
pub fn response(tx: &[u8; 12], asker: SocketAddr) -> Vec<u8> {
    let asker = normalize(asker);
    let mut attrs = Vec::new();
    let xport = asker.port() ^ (COOKIE >> 16) as u16;
    match asker.ip() {
        IpAddr::V4(ip) => {
            let x = u32::from(ip) ^ COOKIE;
            attrs.extend_from_slice(&XOR_MAPPED.to_be_bytes());
            attrs.extend_from_slice(&8u16.to_be_bytes());
            attrs.extend_from_slice(&[0, 1]);
            attrs.extend_from_slice(&xport.to_be_bytes());
            attrs.extend_from_slice(&x.to_be_bytes());
            attrs.extend_from_slice(&MAPPED.to_be_bytes());
            attrs.extend_from_slice(&8u16.to_be_bytes());
            attrs.extend_from_slice(&[0, 1]);
            attrs.extend_from_slice(&asker.port().to_be_bytes());
            attrs.extend_from_slice(&ip.octets());
        }
        IpAddr::V6(ip) => {
            let mut k = COOKIE.to_be_bytes().to_vec();
            k.extend_from_slice(tx);
            let o = ip.octets();
            attrs.extend_from_slice(&XOR_MAPPED.to_be_bytes());
            attrs.extend_from_slice(&20u16.to_be_bytes());
            attrs.extend_from_slice(&[0, 2]);
            attrs.extend_from_slice(&xport.to_be_bytes());
            for i in 0..16 {
                attrs.push(o[i] ^ k[i]);
            }
        }
    }
    let mut b = Vec::with_capacity(20 + attrs.len());
    b.extend_from_slice(&BINDING_RESPONSE.to_be_bytes());
    b.extend_from_slice(&(attrs.len() as u16).to_be_bytes());
    b.extend_from_slice(&COOKIE.to_be_bytes());
    b.extend_from_slice(tx);
    b.extend_from_slice(&attrs);
    b
}

/// A Binding success answer: (transaction id, mapped address).
pub fn parse_response(p: &[u8]) -> Option<([u8; 12], SocketAddr)> {
    let (t, n, tx) = header(p)?;
    if t != BINDING_RESPONSE {
        return None;
    }
    let mut i = 20;
    let end = 20 + n;
    let mut plain = None;
    while i + 4 <= end {
        let at = u16::from_be_bytes([p[i], p[i + 1]]);
        let len = u16::from_be_bytes([p[i + 2], p[i + 3]]) as usize;
        let v = p.get(i + 4..i + 4 + len)?;
        if (at == XOR_MAPPED || at == MAPPED) && v.len() >= 8 {
            let xor = at == XOR_MAPPED;
            let port = u16::from_be_bytes([v[2], v[3]]) ^ if xor { (COOKIE >> 16) as u16 } else { 0 };
            let a = match (v[1], v.len()) {
                (1, _) => {
                    let ip = u32::from_be_bytes(v[4..8].try_into().ok()?) ^ if xor { COOKIE } else { 0 };
                    Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port))
                }
                (2, 20) => {
                    let mut k = COOKIE.to_be_bytes().to_vec();
                    k.extend_from_slice(&tx);
                    let mut o = [0u8; 16];
                    for j in 0..16 {
                        o[j] = v[4 + j] ^ if xor { k[j] } else { 0 };
                    }
                    Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(o)), port))
                }
                _ => None,
            };
            if xor {
                return a.map(|a| (tx, normalize(a)));
            }
            plain = plain.or(a);
        }
        i += 4 + ((len + 3) & !3);
    }
    plain.map(|a| (tx, normalize(a)))
}

/// How often Binding requests are answered. An answer is larger than the request and goes to
/// whatever source address the request names, so answers are counted per asking IP and in all.
/// A node asks a server at most a few times a minute (3 tries, NAT checks, proofs), so the
/// per-IP allowance is far above what an arcade needs.
pub struct AnswerLimit {
    per_ip: std::collections::HashMap<IpAddr, (f64, std::time::Instant)>,
    all: (f64, std::time::Instant),
}

/// Answers per asking IP: 2 a second, 8 at once.
pub const ANSWER_IP_RATE: f64 = 2.0;
pub const ANSWER_IP_BURST: f64 = 8.0;
/// Answers in all: 200 a second, 400 at once.
pub const ANSWER_ALL_RATE: f64 = 200.0;
pub const ANSWER_ALL_BURST: f64 = 400.0;
/// Asking IPs remembered at once (older ones are forgotten first).
const ANSWER_IPS: usize = 4096;

impl Default for AnswerLimit {
    fn default() -> Self {
        let now = std::time::Instant::now();
        AnswerLimit { per_ip: Default::default(), all: (ANSWER_ALL_BURST, now) }
    }
}

fn refill(b: &mut (f64, std::time::Instant), rate: f64, burst: f64, now: std::time::Instant) {
    b.0 = (b.0 + now.saturating_duration_since(b.1).as_secs_f64() * rate).min(burst);
    b.1 = now;
}

impl AnswerLimit {
    /// May a request from `ip` be answered now?
    pub fn allow(&mut self, ip: IpAddr, now: std::time::Instant) -> bool {
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(ip),
            v4 => v4,
        };
        refill(&mut self.all, ANSWER_ALL_RATE, ANSWER_ALL_BURST, now);
        if self.all.0 < 1.0 {
            return false;
        }
        if !self.per_ip.contains_key(&ip) && self.per_ip.len() >= ANSWER_IPS {
            // Forget askers whose allowance is back to full; if none, refuse new ones for now.
            let full = (ANSWER_IP_BURST / ANSWER_IP_RATE) as u64 + 1;
            self.per_ip.retain(|_, (_, t)| now.saturating_duration_since(*t).as_secs() < full);
            if self.per_ip.len() >= ANSWER_IPS {
                return false;
            }
        }
        let b = self.per_ip.entry(ip).or_insert((ANSWER_IP_BURST, now));
        refill(b, ANSWER_IP_RATE, ANSWER_IP_BURST, now);
        if b.0 < 1.0 {
            return false;
        }
        b.0 -= 1.0;
        self.all.0 -= 1.0;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_counted_per_ip_and_in_all() {
        let t = std::time::Instant::now();
        let mut l = AnswerLimit::default();
        let a: IpAddr = "203.0.113.9".parse().unwrap();
        let n = (0..50).filter(|_| l.allow(a, t)).count();
        assert_eq!(n, ANSWER_IP_BURST as usize);
        // The same asker as IPv4-mapped IPv6 shares the allowance.
        assert!(!l.allow("::ffff:203.0.113.9".parse().unwrap(), t));
        // Another asker has its own; after a second the first has two more.
        assert!(l.allow("203.0.113.10".parse().unwrap(), t));
        let t1 = t + std::time::Duration::from_secs(1);
        assert_eq!((0..10).filter(|_| l.allow(a, t1)).count(), ANSWER_IP_RATE as usize);
        // In all: many askers at once get at most the overall burst.
        let mut l = AnswerLimit::default();
        let n = (0..2000u32).filter(|i| l.allow(IpAddr::V4(std::net::Ipv4Addr::from(0x0a00_0000 + i)), t)).count();
        assert_eq!(n, ANSWER_ALL_BURST as usize);
    }

    #[test]
    fn binding_round_trip() {
        let (tx, req) = request();
        assert_eq!(parse_request(&req), Some(tx));
        let a: SocketAddr = "203.0.113.9:40001".parse().unwrap();
        let r = response(&tx, a);
        assert_eq!(parse_response(&r), Some((tx, a)));
        // An IPv4-mapped asker is answered as IPv4.
        let m: SocketAddr = "[::ffff:203.0.113.9]:40001".parse().unwrap();
        assert_eq!(parse_response(&response(&tx, m)), Some((tx, a)));
        let v6: SocketAddr = "[2001:db8::7]:5000".parse().unwrap();
        assert_eq!(parse_response(&response(&tx, v6)), Some((tx, v6)));
        assert!(parse_request(&r).is_none());
    }

    #[test]
    fn rfc5769_ipv4_response() {
        // RFC 5769 2.2 (fingerprint/integrity attributes are skipped).
        let hex = "0101003c2112a442b7e7a701bc34d686fa87dfae8022000b7465737420766563746f7220002000080001a147e112a643000800144e7b2dda01e5b32e6f76ab4c0d0dbf1fb11b4d7c80280004c07d4c96";
        let p = hex::decode(hex).unwrap();
        let (_, a) = parse_response(&p).unwrap();
        assert_eq!(a, "192.0.2.1:32853".parse::<SocketAddr>().unwrap());
    }
}
