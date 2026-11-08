//! Minimal DNS A-record lookup over UDP to public resolvers, for coin DNS seeds when the
//! system resolver returns nothing (seen on this PC for seed.multidoge.org).

use std::net::{Ipv4Addr, UdpSocket};
use std::time::Duration;

pub const RESOLVERS: &[&str] = &["1.1.1.1:53", "8.8.8.8:53", "9.9.9.9:53"];

fn query(name: &str, id: u16) -> Vec<u8> {
    let mut q = Vec::with_capacity(32 + name.len());
    q.extend_from_slice(&id.to_be_bytes());
    q.extend_from_slice(&[0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]); // RD, 1 question
    for label in name.trim_end_matches('.').split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 1, 0, 1]); // root, type A, class IN
    q
}

fn skip_name(b: &[u8], mut i: usize) -> Option<usize> {
    loop {
        let l = *b.get(i)? as usize;
        if l == 0 {
            return Some(i + 1);
        }
        if l & 0xc0 == 0xc0 {
            return Some(i + 2);
        }
        i += 1 + l;
    }
}

fn parse(b: &[u8], id: u16) -> Option<Vec<Ipv4Addr>> {
    if b.len() < 12 || u16::from_be_bytes([b[0], b[1]]) != id {
        return None;
    }
    let qd = u16::from_be_bytes([b[4], b[5]]) as usize;
    let an = u16::from_be_bytes([b[6], b[7]]) as usize;
    let mut i = 12;
    for _ in 0..qd {
        i = skip_name(b, i)? + 4;
    }
    let mut out = Vec::new();
    for _ in 0..an {
        i = skip_name(b, i)?;
        let t = u16::from_be_bytes([*b.get(i)?, *b.get(i + 1)?]);
        let len = u16::from_be_bytes([*b.get(i + 8)?, *b.get(i + 9)?]) as usize;
        i += 10;
        if t == 1 && len == 4 {
            out.push(Ipv4Addr::new(b[i], b[i + 1], b[i + 2], b[i + 3]));
        }
        i += len;
    }
    Some(out)
}

/// A records for `name` from the first public resolver that answers.
pub fn resolve_a(name: &str) -> Vec<Ipv4Addr> {
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return vec![] };
    let _ = sock.set_read_timeout(Some(Duration::from_secs(3)));
    let id = (std::process::id() as u16) ^ 0x5a5a;
    for r in RESOLVERS {
        if sock.send_to(&query(name, id), r).is_err() {
            continue;
        }
        let mut buf = [0u8; 1500];
        if let Ok((n, _)) = sock.recv_from(&mut buf) {
            if let Some(v) = parse(&buf[..n], id) {
                if !v.is_empty() {
                    return v;
                }
            }
        }
    }
    vec![]
}
