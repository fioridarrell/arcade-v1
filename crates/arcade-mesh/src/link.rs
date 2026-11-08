//! One encrypted mesh link, compatible with the existing arcade (mesh/link.py, VERSION 2).
//!
//! HELLO (clear):  u32 BE len | "ARCM" | 2 | len(net) | net | X25519 ephemeral pk (32)
//! keys:           shared = X25519(eph_sk, their_eph)
//!                 transcript = blake2b-256(first_hello + second_hello)  (full HELLO bodies,
//!                 dialer's first; verified against a live arcade 2026-11-07)
//!                 derive(l) = blake2b-256(shared + transcript, person = l padded to 16)
//!                 dialer sends with derive("arcm-dialer"), listener with derive("arcm-listener")
//! frames:         u32 BE len | XChaCha20-Poly1305(plain, nonce = counter as 24-byte BE)
//! plain:          u16 BE len(head) | JSON head | blob
//! AUTH (first encrypted frame each way):
//!                 {"t":"auth","key":hex pk,"sig":hex,"listen":port|null}
//!                 sig = Ed25519("arcade-mesh-auth-v1" + role(D|L) + transcript)

use crate::canon::compact;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde_json::{json, Value};

use std::net::TcpStream;
use std::time::Duration;

pub const MAGIC: &[u8; 4] = b"ARCM";
pub const VERSION: u8 = 2;
pub const MAX_FRAME: usize = 64 * 1024;
pub const AUTH_CONTEXT: &[u8] = b"arcade-mesh-auth-v1";

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("handshake: {0}")]
    Handshake(String),
    #[error("frame too large: {0}")]
    TooLarge(usize),
    #[error("decryption failed (wrong keys or corrupt frame)")]
    Decrypt,
    #[error("bad frame: {0}")]
    BadFrame(String),
}

pub type Result<T> = std::result::Result<T, LinkError>;

/// A message on the link: JSON head plus opaque blob.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub head: Value,
    pub blob: Vec<u8>,
}

impl Frame {
    pub fn new(head: Value) -> Frame {
        Frame { head, blob: Vec::new() }
    }
    pub fn kind(&self) -> &str {
        self.head.get("t").and_then(Value::as_str).unwrap_or("")
    }
}

pub fn blake2b32(data: &[u8], person: &[u8]) -> [u8; 32] {
    let mut p = [0u8; 16];
    let n = person.len().min(16);
    p[..n].copy_from_slice(&person[..n]);
    let h = blake2b_simd::Params::new().hash_length(32).personal(&p).hash(data);
    h.as_bytes().try_into().unwrap()
}

pub struct Keys {
    pub transcript: [u8; 32],
    pub send: [u8; 32],
    pub recv: [u8; 32],
}

/// Derive the session keys exactly as link.py does.
pub fn derive_keys(shared: &[u8; 32], my_hello: &[u8], their_hello: &[u8], dialer: bool) -> Keys {
    let (first, second) = if dialer { (my_hello, their_hello) } else { (their_hello, my_hello) };
    let mut fs = Vec::with_capacity(64);
    fs.extend_from_slice(first);
    fs.extend_from_slice(second);
    let transcript = blake2b32(&fs, &[]);
    let mut st = Vec::with_capacity(64);
    st.extend_from_slice(shared);
    st.extend_from_slice(&transcript);
    let k_dialer = blake2b32(&st, b"arcm-dialer");
    let k_listener = blake2b32(&st, b"arcm-listener");
    let (send, recv) = if dialer { (k_dialer, k_listener) } else { (k_listener, k_dialer) };
    Keys { transcript, send, recv }
}

pub fn hello_body(net: &str, eph: &[u8; 32]) -> Vec<u8> {
    let mut b = Vec::with_capacity(6 + net.len() + 32);
    b.extend_from_slice(MAGIC);
    b.push(VERSION);
    b.push(net.len() as u8);
    b.extend_from_slice(net.as_bytes());
    b.extend_from_slice(eph);
    b
}

pub fn parse_hello(b: &[u8], net: &str) -> Result<[u8; 32]> {
    let bad = |w: &str| LinkError::Handshake(w.into());
    if b.len() < 6 || &b[..4] != MAGIC {
        return Err(bad("bad magic"));
    }
    if b[4] != VERSION {
        return Err(bad("unsupported version"));
    }
    let n = b[5] as usize;
    let theirs = b.get(6..6 + n).ok_or_else(|| bad("short hello"))?;
    if theirs != net.as_bytes() {
        return Err(LinkError::Handshake(format!(
            "network mismatch: they are {:?}, we are {net:?}",
            String::from_utf8_lossy(theirs)
        )));
    }
    let eph = b.get(6 + n..).ok_or_else(|| bad("short hello"))?;
    eph.try_into().map_err(|_| bad("ephemeral key must be 32 bytes"))
}

fn nonce(n: u64) -> XNonce {
    let mut b = [0u8; 24];
    b[16..].copy_from_slice(&n.to_be_bytes());
    XNonce::from(b)
}

pub fn encode_plain(f: &Frame) -> Vec<u8> {
    let head = compact(&f.head).into_bytes();
    let mut p = Vec::with_capacity(2 + head.len() + f.blob.len());
    p.extend_from_slice(&(head.len() as u16).to_be_bytes());
    p.extend_from_slice(&head);
    p.extend_from_slice(&f.blob);
    p
}

pub fn decode_plain(p: &[u8]) -> Result<Frame> {
    if p.len() >= 3 && p[0] == 0xff && p[1] == 0xff && p[2] == 0x01 {
        // Binary "pub" form (link.py:240-280). Surface it as kind "pub-bin" for now.
        return Ok(Frame { head: json!({"t": "pub-bin"}), blob: p.to_vec() });
    }
    if p.len() < 2 {
        return Err(LinkError::BadFrame("short plaintext".into()));
    }
    let n = u16::from_be_bytes([p[0], p[1]]) as usize;
    let head = p.get(2..2 + n).ok_or_else(|| LinkError::BadFrame("short head".into()))?;
    let head: Value = serde_json::from_slice(head).map_err(|e| LinkError::BadFrame(e.to_string()))?;
    Ok(Frame { head, blob: p[2 + n..].to_vec() })
}



pub struct Link {
    t: Box<dyn crate::transport::Transport>,
    send: XChaCha20Poly1305,
    recv: XChaCha20Poly1305,
    n_send: u64,
    n_recv: u64,
    /// The peer's node id (hex Ed25519 verify key), proved by its AUTH signature.
    pub peer_id: String,
    pub peer_listen: Option<u16>,
}

impl Link {
    /// Run the handshake over an established TCP stream.
    pub fn handshake(stream: TcpStream, net: &str, identity: &SigningKey, dialer: bool, listen: Option<u16>) -> Result<Link> {
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        Self::handshake_over(Box::new(crate::transport::Tcp { stream }), net, identity, dialer, listen)
    }

    /// Run the handshake over any transport (TCP, WebSocket).
    pub fn handshake_over(
        mut t: Box<dyn crate::transport::Transport>,
        net: &str,
        identity: &SigningKey,
        dialer: bool,
        listen: Option<u16>,
    ) -> Result<Link> {
        t.set_read_timeout(Duration::from_secs(10))?;
        let eph_sk = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
        let eph_pk = x25519_dalek::PublicKey::from(&eph_sk).to_bytes();
        let my_hello = hello_body(net, &eph_pk);
        t.send(&my_hello)?;
        let their_hello = t.recv()?;
        let their_eph = parse_hello(&their_hello, net)?;
        let shared = eph_sk.diffie_hellman(&x25519_dalek::PublicKey::from(their_eph)).to_bytes();
        let keys = derive_keys(&shared, &my_hello, &their_hello, dialer);

        let mut link = Link {
            t,
            send: XChaCha20Poly1305::new_from_slice(&keys.send).unwrap(),
            recv: XChaCha20Poly1305::new_from_slice(&keys.recv).unwrap(),
            n_send: 0,
            n_recv: 0,
            peer_id: String::new(),
            peer_listen: None,
        };

        let role = |d: bool| if d { b'D' } else { b'L' };
        let mut msg = AUTH_CONTEXT.to_vec();
        msg.push(role(dialer));
        msg.extend_from_slice(&keys.transcript);
        let sig = identity.sign(&msg);
        let my_id = hex::encode(identity.verifying_key().to_bytes());
        link.send(&Frame::new(json!({
            "t": "auth",
            "key": my_id,
            "sig": hex::encode(sig.to_bytes()),
            "listen": listen,
        })))?;

        let auth = link.recv()?;
        if auth.kind() != "auth" {
            return Err(LinkError::Handshake(format!("expected auth, got {:?}", auth.kind())));
        }
        let key_hex = auth.head["key"].as_str().unwrap_or("");
        let sig_hex = auth.head["sig"].as_str().unwrap_or("");
        let key: [u8; 32] = hex::decode(key_hex)
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| LinkError::Handshake("bad peer key".into()))?;
        if key_hex == my_id {
            return Err(LinkError::Handshake("peer is ourselves".into()));
        }
        let sig: [u8; 64] = hex::decode(sig_hex)
            .ok()
            .and_then(|v| v.try_into().ok())
            .ok_or_else(|| LinkError::Handshake("bad peer signature".into()))?;
        let vk = VerifyingKey::from_bytes(&key).map_err(|e| LinkError::Handshake(e.to_string()))?;
        let mut theirs = AUTH_CONTEXT.to_vec();
        theirs.push(role(!dialer));
        theirs.extend_from_slice(&keys.transcript);
        vk.verify(&theirs, &ed25519_dalek::Signature::from_bytes(&sig))
            .map_err(|_| LinkError::Handshake("peer auth signature does not verify".into()))?;
        link.peer_id = key_hex.to_string();
        link.peer_listen = auth.head["listen"].as_u64().filter(|p| *p > 0 && *p < 65536).map(|p| p as u16);
        link.t.set_read_timeout(Duration::from_secs(45))?; // DEAD_AFTER
        Ok(link)
    }

    /// Dial `host:port` (TCP) or `wss://host/mesh` (WebSocket over TLS).
    pub fn dial(addr: &str, net: &str, identity: &SigningKey, listen: Option<u16>) -> Result<Link> {
        if addr.starts_with("wss://") {
            let t = crate::transport::Wss::connect(addr, Duration::from_secs(15))?;
            return Self::handshake_over(Box::new(t), net, identity, true, listen);
        }
        use std::net::ToSocketAddrs;
        let sa = addr
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| LinkError::Handshake(format!("cannot resolve {addr}")))?;
        let s = TcpStream::connect_timeout(&sa, Duration::from_secs(10))?;
        s.set_nodelay(true)?;
        Self::handshake(s, net, identity, true, listen)
    }

    pub fn send(&mut self, f: &Frame) -> Result<()> {
        let ct = self.send.encrypt(&nonce(self.n_send), encode_plain(f).as_slice()).map_err(|_| LinkError::Decrypt)?;
        self.n_send += 1;
        self.t.send(&ct)?;
        Ok(())
    }

    pub fn recv(&mut self) -> Result<Frame> {
        let ct = self.t.recv()?;
        let pt = self.recv.decrypt(&nonce(self.n_recv), ct.as_slice()).map_err(|_| LinkError::Decrypt)?;
        self.n_recv += 1;
        decode_plain(&pt)
    }

    /// How long `recv` waits before returning a timeout error (WouldBlock / TimedOut).
    pub fn set_read_timeout(&mut self, d: Duration) -> std::io::Result<()> {
        self.t.set_read_timeout(d)
    }

    /// Where this link goes (address or URL).
    pub fn describe(&self) -> String {
        self.t.describe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    #[test]
    fn two_links_talk() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap().to_string();
        let a = SigningKey::from_bytes(&[1; 32]);
        let b = SigningKey::from_bytes(&[2; 32]);
        let b_id = hex::encode(b.verifying_key().to_bytes());
        let t = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut link = Link::handshake(s, "arcade-test", &b, false, Some(8421)).unwrap();
            let f = link.recv().unwrap();
            link.send(&Frame { head: json!({"t":"pong","n":f.head["n"]}), blob: vec![9, 9] }).unwrap();
            link.peer_id
        });
        let mut link = Link::dial(&addr, "arcade-test", &a, None).unwrap();
        assert_eq!(link.peer_id, b_id);
        assert_eq!(link.peer_listen, Some(8421));
        link.send(&Frame::new(json!({"t":"ping","n":"deadbeef"}))).unwrap();
        let f = link.recv().unwrap();
        assert_eq!(f.kind(), "pong");
        assert_eq!(f.blob, vec![9, 9]);
        assert_eq!(t.join().unwrap(), hex::encode(a.verifying_key().to_bytes()));
    }

    #[test]
    fn network_mismatch_is_refused() {
        let body = hello_body("arcade-main", &[0; 32]);
        assert!(parse_hello(&body, "arcade-test").is_err());
        assert_eq!(parse_hello(&body, "arcade-main").unwrap(), [0; 32]);
    }
}
