//! Bitcoin-family P2P wire protocol, as Pepecoin Core speaks it.
//!
//! Message = magic(4) | command(12, NUL-padded) | length u32 LE | checksum(4 = sha256d[..4]) | payload.
//! Blocking std I/O: one connection per thread is plenty for one chain's sync.

use crate::auxpow::AuxPow;
use crate::encode::{write_varint, DecodeError, Reader};
use crate::format::Format;
use crate::hash::{sha256d, Hash256};
use crate::header::Header;
use crate::params::ChainParams;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const MAX_MESSAGE: usize = 32 * 1024 * 1024;
pub const MSG_TX: u32 = 1;
pub const MSG_BLOCK: u32 = 2;

#[derive(Debug, thiserror::Error)]
pub enum P2pError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("decode: {0}")]
    Decode(#[from] DecodeError),
    #[error("wrong network magic")]
    BadMagic,
    #[error("bad checksum on `{0}`")]
    BadChecksum(String),
    #[error("message too large: {0}")]
    TooLarge(usize),
    #[error("peer: {0}")]
    Protocol(String),
}

pub type Result<T> = std::result::Result<T, P2pError>;

#[derive(Debug, Clone)]
pub struct Message {
    pub command: String,
    pub payload: Vec<u8>,
}

pub fn encode_message(magic: [u8; 4], command: &str, payload: &[u8]) -> Vec<u8> {
    assert!(command.len() <= 12);
    let mut o = Vec::with_capacity(24 + payload.len());
    o.extend_from_slice(&magic);
    let mut cmd = [0u8; 12];
    cmd[..command.len()].copy_from_slice(command.as_bytes());
    o.extend_from_slice(&cmd);
    o.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    o.extend_from_slice(&sha256d(payload).0[..4]);
    o.extend_from_slice(payload);
    o
}

pub fn read_message(r: &mut impl Read, magic: [u8; 4]) -> Result<Message> {
    read_message_capped(r, magic, MAX_MESSAGE)
}

/// [`read_message`] with a tighter limit for `block` messages (the chain's largest possible
/// block, [`ChainParams::max_block_bytes`]): a bigger one is refused before anything is read
/// into memory.
pub fn read_message_capped(r: &mut impl Read, magic: [u8; 4], block_max: usize) -> Result<Message> {
    let mut head = [0u8; 24];
    r.read_exact(&mut head)?;
    if head[..4] != magic {
        return Err(P2pError::BadMagic);
    }
    let end = head[4..16].iter().position(|&b| b == 0).unwrap_or(12);
    let command = String::from_utf8_lossy(&head[4..4 + end]).into_owned();
    let len = u32::from_le_bytes(head[16..20].try_into().unwrap()) as usize;
    if len > MAX_MESSAGE || (command == "block" && len > block_max) {
        return Err(P2pError::TooLarge(len));
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload)?;
    if sha256d(&payload).0[..4] != head[20..24] {
        return Err(P2pError::BadChecksum(command));
    }
    Ok(Message { command, payload })
}

fn net_addr(out: &mut Vec<u8>, addr: Option<SocketAddr>) {
    out.extend_from_slice(&0u64.to_le_bytes()); // services
    let (ip, port) = match addr {
        Some(SocketAddr::V4(a)) => (a.ip().to_ipv6_mapped().octets(), a.port()),
        Some(SocketAddr::V6(a)) => (a.ip().octets(), a.port()),
        None => ([0u8; 16], 0),
    };
    out.extend_from_slice(&ip);
    out.extend_from_slice(&port.to_be_bytes());
}

/// An addr message: (address, last seen) for each public IPv4/IPv6 entry.
pub fn parse_addr(payload: &[u8]) -> Vec<(SocketAddr, u32)> {
    let mut r = Reader::new(payload);
    let Ok(n) = r.varint() else { return Vec::new() };
    let mut out = Vec::new();
    for _ in 0..n.min(1000) {
        let (Ok(time), Ok(_services), Ok(ip), Ok(port)) = (r.u32(), r.u64(), r.bytes(16), r.bytes(2)) else { break };
        let ip: [u8; 16] = ip.try_into().unwrap();
        let port = u16::from_be_bytes([port[0], port[1]]);
        let v6 = std::net::Ipv6Addr::from(ip);
        let ip: std::net::IpAddr = match v6.to_ipv4_mapped() {
            Some(v4) => v4.into(),
            None => v6.into(),
        };
        if port == 0 || !public_ip(&ip) {
            continue;
        }
        out.push((SocketAddr::new(ip, port), time));
    }
    out
}

/// A routable address (not private, loopback, link-local, multicast or documentation).
pub fn public_ip(ip: &std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(a) => !(a.is_private() || a.is_loopback() || a.is_link_local() || a.is_unspecified() || a.is_broadcast() || a.is_multicast() || a.is_documentation() || a.octets()[0] == 100 && (a.octets()[1] & 0xc0) == 64 || a.octets()[0] == 0),
        std::net::IpAddr::V6(a) => {
            let s = a.segments();
            !(a.is_loopback() || a.is_unspecified() || a.is_multicast() || (s[0] & 0xfe00) == 0xfc00 || (s[0] & 0xffc0) == 0xfe80 || s[0] == 0x2001 && s[1] == 0x0db8 || s[0] == 0xfd87)
        }
    }
}

pub fn version_payload(params: &ChainParams, peer: Option<SocketAddr>, start_height: i32, relay: bool) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&params.protocol_version.to_le_bytes());
    p.extend_from_slice(&0u64.to_le_bytes()); // our services: none (we don't serve blocks yet)
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    p.extend_from_slice(&now.to_le_bytes());
    net_addr(&mut p, peer);
    net_addr(&mut p, None);
    let nonce: u64 = now as u64 ^ 0x5eed_a5ca_de00_0001 ^ (std::process::id() as u64) << 32;
    p.extend_from_slice(&nonce.to_le_bytes());
    let ua = params.user_agent().as_bytes();
    write_varint(&mut p, ua.len() as u64);
    p.extend_from_slice(ua);
    p.extend_from_slice(&start_height.to_le_bytes());
    p.push(relay as u8); // relay: whether peers should announce unconfirmed transactions
    p
}

#[derive(Debug, Clone)]
pub struct PeerVersion {
    pub version: i32,
    pub services: u64,
    pub user_agent: String,
    pub start_height: i32,
}

pub fn parse_version(payload: &[u8]) -> Result<PeerVersion> {
    let mut r = Reader::new(payload);
    let version = r.i32()?;
    let services = r.u64()?;
    r.i64()?;
    r.bytes(26)?;
    r.bytes(26)?;
    r.u64()?;
    let user_agent = String::from_utf8_lossy(r.var_bytes()?).into_owned();
    let start_height = r.i32()?;
    Ok(PeerVersion { version, services, user_agent, start_height })
}

pub fn getheaders_payload(version: i32, locator: &[Hash256], stop: Hash256) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend_from_slice(&version.to_le_bytes());
    write_varint(&mut p, locator.len() as u64);
    for h in locator {
        p.extend_from_slice(&h.0);
    }
    p.extend_from_slice(&stop.0);
    p
}

/// One entry of a `headers` message: the header, its AuxPoW if any, and its raw bytes.
#[derive(Debug, Clone)]
pub struct HeaderEntry {
    pub header: Header,
    pub auxpow: Option<AuxPow>,
}

/// Entries are header [+ AuxPoW] + tx count (0) [+ block-sig length (0) on PoS chains].
pub fn parse_headers(payload: &[u8], fmt: &Format) -> Result<Vec<HeaderEntry>> {
    let mut r = Reader::new(payload);
    let n = r.len()?;
    let mut v = Vec::with_capacity(n.min(2000));
    for _ in 0..n {
        let header = Header::decode(&mut r)?;
        let auxpow = if fmt.has_auxpow(&header) { Some(AuxPow::decode(&mut r)?) } else { None };
        let _tx_count = r.varint()?;
        if fmt.block_sig {
            let _sig = r.var_bytes()?;
        }
        v.push(HeaderEntry { header, auxpow });
    }
    Ok(v)
}

pub fn getdata_payload(kind: u32, hashes: &[Hash256]) -> Vec<u8> {
    let mut p = Vec::new();
    write_varint(&mut p, hashes.len() as u64);
    for h in hashes {
        p.extend_from_slice(&kind.to_le_bytes());
        p.extend_from_slice(&h.0);
    }
    p
}

/// A connected, handshaken peer.
pub struct Peer {
    stream: TcpStream,
    magic: [u8; 4],
    pub addr: SocketAddr,
    pub version: PeerVersion,
    protocol_version: i32,
    pub format: Format,
    /// The largest `block` message taken from this peer (ChainParams::max_block_bytes).
    block_max: usize,
    /// The peer broke the protocol (a block not asked for, too big, over a request's byte
    /// limit): the connection is shut and the peer is of no further use.
    dropped: bool,
}

/// Part of the error a block request gives when the peer sent more bytes than the request
/// allowed ([`Peer::get_blocks_max`]): the blocks may be honest but bigger than expected (ask
/// for fewer at once), and the peer was dropped since the rest is still on the wire.
pub const OVER_BYTE_LIMIT: &str = "over the request's byte limit";

impl Peer {
    pub fn connect(params: &ChainParams, addr: &str, timeout: Duration) -> Result<Peer> {
        Self::connect_with(params, addr, timeout, false)
    }

    /// Connect; with `relay` the peer announces unconfirmed transactions (mempool).
    pub fn connect_with(params: &ChainParams, addr: &str, timeout: Duration, relay: bool) -> Result<Peer> {
        let sa = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| P2pError::Protocol(format!("cannot resolve {addr}")))?;
        let mut stream = TcpStream::connect_timeout(&sa, timeout)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        stream.set_nodelay(true)?;
        let magic = params.magic;
        stream.write_all(&encode_message(magic, "version", &version_payload(params, Some(sa), 0, relay)))?;

        let mut version = None;
        let mut got_verack = false;
        while version.is_none() || !got_verack {
            let m = read_message(&mut stream, magic)?;
            match m.command.as_str() {
                "version" => {
                    version = Some(parse_version(&m.payload)?);
                    stream.write_all(&encode_message(magic, "verack", &[]))?;
                }
                "verack" => got_verack = true,
                _ => {} // sendheaders, sendcmpct, feefilter, ping... before verack: ignore
            }
        }
        let version = version.unwrap();
        let protocol_version = version.version.min(params.protocol_version);
        Ok(Peer { stream, magic, addr: sa, version, protocol_version, format: params.format, block_max: params.max_block_bytes(), dropped: false })
    }

    pub fn send(&mut self, command: &str, payload: &[u8]) -> Result<()> {
        if std::env::var_os("ARCADE_P2P_DEBUG").is_some() {
            eprintln!("-> {} ({} bytes)", command, payload.len());
        }
        self.stream.write_all(&encode_message(self.magic, command, payload))?;
        Ok(())
    }

    /// Next message, answering pings transparently.
    pub fn recv(&mut self) -> Result<Message> {
        loop {
            let m = match read_message_capped(&mut self.stream, self.magic, self.block_max) {
                Ok(m) => m,
                Err(e) => {
                    // Whatever went wrong, the stream is out of step now (or gone).
                    if !matches!(&e, P2pError::Io(x) if matches!(x.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut)) {
                        self.drop_peer();
                    }
                    return Err(e);
                }
            };
            if std::env::var_os("ARCADE_P2P_DEBUG").is_some() {
                eprintln!("<- {} ({} bytes) {}", m.command, m.payload.len(),
                    if m.command == "reject" { String::from_utf8_lossy(&m.payload).into_owned() } else { String::new() });
            }
            if m.command == "ping" {
                self.send("pong", &m.payload)?;
                continue;
            }
            return Ok(m);
        }
    }

    /// Wait for a message with this command, skipping others (inv, addr, ...).
    pub fn recv_cmd(&mut self, command: &str) -> Result<Message> {
        // Bounded as a whole (other messages keep arriving, so the read timeout alone isn't).
        let deadline = std::time::Instant::now() + Duration::from_secs(90);
        loop {
            if std::time::Instant::now() > deadline {
                return Err(P2pError::Protocol(format!("no {command} from the peer in time")));
            }
            let m = self.recv()?;
            if m.command == command {
                return Ok(m);
            }
        }
    }

    pub fn get_headers(&mut self, locator: &[Hash256]) -> Result<Vec<HeaderEntry>> {
        self.send("getheaders", &getheaders_payload(self.protocol_version, locator, Hash256::ZERO))?;
        let m = self.recv_cmd("headers")?;
        parse_headers(&m.payload, &self.format)
    }

    /// Request blocks; returns raw block bytes in the order received. Every block must be one
    /// asked for (its header's id), each once: anything else drops the peer.
    pub fn get_blocks(&mut self, hashes: &[Hash256]) -> Result<Vec<Vec<u8>>> {
        self.get_blocks_max(hashes, usize::MAX)
    }

    /// [`Peer::get_blocks`], refusing (and dropping the peer) once the blocks received pass
    /// `max_bytes` in all: what one request can make us hold is bounded however a peer pads its
    /// blocks. The error then contains [`OVER_BYTE_LIMIT`].
    pub fn get_blocks_max(&mut self, hashes: &[Hash256], max_bytes: usize) -> Result<Vec<Vec<u8>>> {
        if self.dropped {
            return Err(P2pError::Protocol("the peer was dropped".into()));
        }
        self.send("getdata", &getdata_payload(self.format.block_inv(), hashes))?;
        let mut want: std::collections::HashSet<Hash256> = hashes.iter().copied().collect();
        let mut out = Vec::with_capacity(hashes.len());
        let mut bytes = 0usize;
        // A whole-request limit: a peer that keeps talking (pings, inv, addr) but never sends
        // the blocks would otherwise keep this waiting forever (the read timeout never fires).
        let deadline = std::time::Instant::now() + Duration::from_secs(60 + 2 * hashes.len() as u64);
        while out.len() < hashes.len() {
            if std::time::Instant::now() > deadline {
                self.drop_peer();
                return Err(P2pError::Protocol(format!("peer sent {} of {} blocks in time", out.len(), hashes.len())));
            }
            let m = self.recv()?;
            match m.command.as_str() {
                "block" => {
                    let id = m.payload.get(..80).and_then(|h| <&[u8; 80]>::try_from(h).ok()).map(|h| self.format.block_id(&Header::parse(h)));
                    if !id.is_some_and(|id| want.remove(&id)) {
                        self.drop_peer();
                        return Err(P2pError::Protocol(match id {
                            Some(id) => format!("peer sent block {id}, not one asked for (or twice)"),
                            None => "peer sent a block shorter than a header".into(),
                        }));
                    }
                    bytes += m.payload.len();
                    if bytes > max_bytes {
                        self.drop_peer();
                        return Err(P2pError::Protocol(format!("{OVER_BYTE_LIMIT}: {} of {} blocks came to {bytes} bytes, over {max_bytes}", out.len() + 1, hashes.len())));
                    }
                    out.push(m.payload);
                }
                "notfound" => {
                    // The blocks it does have may still come: they'd answer the next request.
                    self.drop_peer();
                    return Err(P2pError::Protocol("peer does not have a requested block".into()));
                }
                _ => {}
            }
        }
        Ok(out)
    }

    /// Shut the connection: the peer broke the protocol, or what it still sends would be read
    /// as the answer to the next request.
    fn drop_peer(&mut self) {
        self.dropped = true;
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }

    /// The connection was shut ([`Peer::get_blocks_max`]): use another peer.
    pub fn dropped(&self) -> bool {
        self.dropped
    }

    /// Ask the peer which other nodes it knows (getaddr) and collect the addresses it sends
    /// within `wait` (Core answers with up to 1,000, in one or more addr messages).
    pub fn get_addrs(&mut self, wait: Duration) -> Result<Vec<(SocketAddr, u32)>> {
        self.send("getaddr", &[])?;
        let deadline = std::time::Instant::now() + wait;
        let mut out = Vec::new();
        let old = self.stream.read_timeout().ok().flatten();
        while std::time::Instant::now() < deadline {
            let left = deadline.saturating_duration_since(std::time::Instant::now()).max(Duration::from_millis(50));
            self.stream.set_read_timeout(Some(left))?;
            match self.recv() {
                Ok(m) if m.command == "addr" => {
                    out.extend(parse_addr(&m.payload));
                    // A big answer is the reply to getaddr; small ones are gossip.
                    if out.len() >= 100 {
                        break;
                    }
                }
                Ok(_) => {}
                Err(P2pError::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => break,
                Err(e) => {
                    let _ = self.stream.set_read_timeout(old);
                    return if out.is_empty() { Err(e) } else { Ok(out) };
                }
            }
        }
        let _ = self.stream.set_read_timeout(old);
        Ok(out)
    }

    pub fn set_read_timeout(&self, d: Duration) -> Result<()> {
        self.stream.set_read_timeout(Some(d))?;
        Ok(())
    }

    /// Inventory type for asking a transaction exactly as relayed (witness included).
    pub fn tx_inv(&self) -> u32 {
        if self.format.segwit { MSG_TX | crate::format::MSG_WITNESS_FLAG } else { MSG_TX }
    }

    pub fn send_tx(&mut self, raw: &[u8]) -> Result<()> {
        self.send("tx", raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::PEPECOIN_TEST;

    #[test]
    fn addr_parses_public_only() {
        let mut p = vec![3u8];
        for (ip, port) in [([0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 8, 8, 4, 4], 22556u16), ([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0xff, 0xff, 192, 168, 1, 2], 22556), ([0x26, 0x05, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1], 32552)] {
            p.extend_from_slice(&1_700_000_000u32.to_le_bytes());
            p.extend_from_slice(&1u64.to_le_bytes());
            p.extend_from_slice(&ip);
            p.extend_from_slice(&port.to_be_bytes());
        }
        let v: Vec<String> = parse_addr(&p).into_iter().map(|(a, _)| a.to_string()).collect();
        assert_eq!(v, vec!["8.8.4.4:22556".to_string(), "[2605:1::1]:32552".to_string()]);
    }

    #[test]
    fn message_roundtrip() {
        let bytes = encode_message(PEPECOIN_TEST.magic, "ping", &[1, 2, 3, 4, 5, 6, 7, 8]);
        let m = read_message(&mut &bytes[..], PEPECOIN_TEST.magic).unwrap();
        assert_eq!(m.command, "ping");
        assert_eq!(m.payload, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(matches!(read_message(&mut &bytes[..], [0; 4]), Err(P2pError::BadMagic)));
    }

    #[test]
    fn a_block_message_over_the_chains_largest_block_is_refused_unread() {
        use crate::params::BITCOIN_MAIN;
        let max = BITCOIN_MAIN.max_block_bytes();
        assert!((4_000_000..=5_100_000).contains(&max), "{max}");
        // Only the 24-byte head: refused before the payload is read (or allocated).
        let mut head = encode_message(BITCOIN_MAIN.magic, "block", &[]);
        head[16..20].copy_from_slice(&((max + 1) as u32).to_le_bytes());
        assert!(matches!(read_message_capped(&mut &head[..], BITCOIN_MAIN.magic, max), Err(P2pError::TooLarge(n)) if n == max + 1));
        // Other messages keep the protocol's own limit.
        head[4..9].copy_from_slice(b"other");
        assert!(matches!(read_message_capped(&mut &head[..], BITCOIN_MAIN.magic, max), Err(P2pError::Io(_))));
        // Chains whose limits aren't exact keep the message limit.
        assert_eq!(crate::params::LITECOIN_MAIN.max_block_bytes(), MAX_MESSAGE);
    }

    /// A peer on loopback that answers the handshake, then sends `blocks` after the first
    /// getdata.
    fn fake_peer(blocks: Vec<Vec<u8>>) -> String {
        use crate::params::BITCOIN_MAIN;
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let magic = BITCOIN_MAIN.magic;
            let _ = read_message(&mut s, magic).unwrap();
            s.write_all(&encode_message(magic, "version", &version_payload(&BITCOIN_MAIN, None, 1, false))).unwrap();
            s.write_all(&encode_message(magic, "verack", &[])).unwrap();
            loop {
                let Ok(m) = read_message(&mut s, magic) else { return };
                if m.command == "getdata" {
                    for b in &blocks {
                        if s.write_all(&encode_message(magic, "block", b)).is_err() {
                            return;
                        }
                    }
                }
            }
        });
        addr
    }

    #[test]
    fn a_block_not_asked_for_or_over_the_byte_limit_drops_the_peer() {
        use crate::params::BITCOIN_MAIN;
        let genesis = hex::decode(BITCOIN_MAIN.genesis_header).unwrap();
        let mut other = genesis.clone();
        other[76] ^= 1; // another nonce: another block id
        let gid = BITCOIN_MAIN.genesis_hash();
        // Asked for genesis, sent another block: refused, and the peer is shut.
        let mut p = Peer::connect(&BITCOIN_MAIN, &fake_peer(vec![other.clone()]), Duration::from_secs(5)).unwrap();
        let e = p.get_blocks(&[gid]).unwrap_err().to_string();
        assert!(e.contains("not one asked for"), "{e}");
        assert!(p.dropped());
        assert!(p.get_blocks(&[gid]).is_err());
        // The same block twice for one asked: refused too.
        let mut p = Peer::connect(&BITCOIN_MAIN, &fake_peer(vec![genesis.clone(), genesis.clone()]), Duration::from_secs(5)).unwrap();
        let other_id = BITCOIN_MAIN.block_id(&Header::parse(other[..80].try_into().unwrap()));
        assert!(p.get_blocks(&[gid, other_id]).unwrap_err().to_string().contains("not one asked for"));
        // Over the request's byte limit: refused with OVER_BYTE_LIMIT, the peer shut.
        let mut p = Peer::connect(&BITCOIN_MAIN, &fake_peer(vec![genesis.clone(), other.clone()]), Duration::from_secs(5)).unwrap();
        let e = p.get_blocks_max(&[gid, other_id], genesis.len() + 10).unwrap_err().to_string();
        assert!(e.contains(OVER_BYTE_LIMIT), "{e}");
        assert!(p.dropped());
        // Both asked for, within the limit: taken.
        let mut p = Peer::connect(&BITCOIN_MAIN, &fake_peer(vec![other.clone(), genesis.clone()]), Duration::from_secs(5)).unwrap();
        assert_eq!(p.get_blocks_max(&[gid, other_id], 2 * genesis.len()).unwrap(), vec![other, genesis]);
        assert!(!p.dropped());
    }

    #[test]
    fn version_parses_back() {
        let p = version_payload(&PEPECOIN_TEST, None, 7, false);
        let v = parse_version(&p).unwrap();
        assert_eq!(v.version, 70016);
        assert_eq!(v.user_agent, "/arcade:1.0.0/");
        assert_eq!(v.start_height, 7);
    }
}
