//! WebRTC links between app windows and website viewers, made by the arcade (Martin
//! 2026-11-19: "Both should use WebRTC and the UDP punching so that they can connect").
//!
//! A website's viewer reaches other players over WebRTC (its doors, _page_doors.js.html): one
//! compact description each way over the room (ICE user and password, DTLS fingerprint and role,
//! UDP candidates), a link of three negotiated data channels ('u' unordered, 'r' reliable, 'm'
//! for media talk), then pictures and sound negotiated as whole descriptions on 'm'. An app
//! window on Linux has no RTCPeerConnection (Ubuntu's WebKitGTK is built without WebRTC), so
//! its arcade is that peer for it, here: one str0m connection per (window seat, viewer),
//! speaking the viewer's own wire, so the viewer's page needs no changes.
//!
//! - ICE runs on one UDP socket of this arcade: a host candidate; server-reflexive ones from
//!   the arcades' STUN servers, asked from that same socket (so the mapping a STUN server saw is
//!   the one the viewer's checks come in through: the hole is punched from both sides); and
//!   relayed ones through an arcade's media relay (TURN, turnsvc.rs, Ashburn's among them),
//!   allocated from that socket too.
//! - Pictures and sound are never transcoded. The window encodes VP8 and Opus (WebCodecs) and
//!   hands its pieces to realtime.media as before (0xA1, media.js); for a player linked here
//!   they are put back into whole frames and sent as RTP. The viewer's RTP is put back into
//!   whole frames and handed to the window as the same pieces. A viewer that offers no VP8
//!   (H.264 only) gets its video refused; its sound still comes.
//! - Key frames: the viewer's PLI or FIR becomes the window's own key-frame note, and the
//!   window's note becomes a PLI. Packets the viewer NACKs are resent from the send buffer.
//! - Fast data rides 'u' and 'r', as between two viewers.
//!
//! Two V1.0 windows keep the arcades' fast lane (fastlane.rs): it already crosses NAT (UDP
//! paths, then relaying arcades), its frames are signed by the arcades, and a WebRTC link
//! between two arcades would only wrap the same frames again. The lane also stays for a viewer
//! whose WebRTC link can't be made: the window then tells it 'p' and the pair uses pieces.
//!
//! Lock order: this module's lock, then realtime's. Realtime never calls in here holding its own.

use crate::node::Node;
use crate::turnsvc as tw;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use str0m::change::{SdpAnswer, SdpOffer, SdpPendingOffer};
use str0m::channel::{ChannelConfig, ChannelId, Reliability};
use str0m::format::Codec;
use str0m::media::{Direction, Frequency, KeyframeRequestKind, MediaKind, MediaTime, Mid};
use str0m::net::{Protocol, Receive};
use str0m::{Candidate, CandidateKind, Event, IceConnectionState, Input, Output, Rtc};

/// Links in all, and for one window seat (the viewer's own limit for a fast room).
const MAX_PAIRS: usize = 128;
const PAIRS_PER_SEAT: usize = 60;
/// A link not up this long after it was made has failed; one lost this long has too.
const CONNECT_WAIT: Duration = Duration::from_secs(15);
const LOST_WAIT: Duration = Duration::from_secs(8);
/// A failed link is still reported this long (the window then falls back to pieces).
const FAILED_KEEP: Duration = Duration::from_secs(30);
/// Waiting for this arcade's own candidates before a description goes out: as the viewer's
/// fastGathered, 2.5 s, or up to 6 s while a relay that was asked has not answered.
const GATHER_WAIT: Duration = Duration::from_millis(2500);
const RELAY_WAIT: Duration = Duration::from_secs(6);
/// ICE servers are asked again after this long; STUN mappings rechecked this often.
const ICE_KEEP: Duration = Duration::from_secs(1800);
const STUN_EVERY: Duration = Duration::from_secs(60);
/// Relays are let go when no link has used them for this long.
const RELAY_IDLE: Duration = Duration::from_secs(600);
/// As the viewer's mediaPc: the one giving way waits before offering; one told "yours" waits;
/// half way through a renegotiation this long, that step is undone.
const MEDIA_YIELD: Duration = Duration::from_millis(1500);
const MEDIA_AFTER: Duration = Duration::from_millis(4000);
const MEDIA_STUCK: Duration = Duration::from_secs(10);
/// A media piece (media.js): header bytes and the most bytes of frame in one piece.
const HEAD: usize = 17;
const PIECE: usize = 48000;
const K_CAM: u8 = 1;
const K_MIC: u8 = 2;
const K_SCR: u8 = 3;
const K_SND: u8 = 4;
const K_NOTE: u8 = 9;
/// The window is asked for a key frame at most this often per link.
const KEY_ASK_EVERY: Duration = Duration::from_millis(400);
/// What a viewer's description may carry (sdpPack: 10 candidates).
const MOST_CANDIDATES: usize = 10;

#[derive(Clone, Copy, PartialEq, Debug)]
enum State {
    Connecting,
    Up,
    Failed,
}

/// One kind of the window's media going to the viewer.
#[derive(Default)]
struct Tx {
    mid: Option<Mid>,
    /// Negotiated as sending (else inactive).
    active: bool,
    /// The window sends this kind to this viewer now.
    want: bool,
    /// A picture: nothing but a key frame may start it.
    need_key: bool,
}

/// One of the viewer's media coming in.
struct Rx {
    seq: u32,
    w: u16,
    h: u16,
}

struct Part {
    parts: u8,
    got: Vec<Option<Vec<u8>>>,
    at: Instant,
}

struct Pair {
    tok: String,
    peer: String,
    room: String,
    who: Value,
    rtc: Rtc,
    born: Instant,
    state: State,
    failed_at: Option<Instant>,
    lost_at: Option<Instant>,
    next: Instant,
    /// We sent the compact offer and wait for the viewer's answer.
    offered: bool,
    polite: bool,
    chans: [Option<ChannelId>; 3],
    m_open: bool,
    pending: Option<(SdpPendingOffer, Vec<(u8, Mid, bool)>, Instant)>,
    offer_at: Option<Instant>,
    wait_until: Option<Instant>,
    tx: BTreeMap<u8, Tx>,
    rx: HashMap<Mid, Rx>,
    /// The viewer's second picture (a screen beside its camera): its stream id ({x} on 'm').
    xid: String,
    stream: String,
    xstream: String,
    said_x: bool,
    parts: HashMap<(u8, u32), Part>,
    key_asked: Option<Instant>,
    pli_at: Option<Instant>,
    /// The viewer's relayed candidates, and how the link goes.
    their_relays: HashSet<SocketAddr>,
    relayed: bool,
    near: &'static str,
    far: &'static str,
    refused: Vec<String>,
    /// Frames written to the viewer and heard from it, and writes held for a key frame.
    sent: u64,
    heard: u64,
    held: u64,
    /// Pieces from the window by kind, and those that came while their kind was not negotiated.
    pieces_by_kind: [u64; 10],
    waiting: u64,
}

/// A login on a media relay (TURN client, RFC 5766, UDP).
struct Turn {
    server: SocketAddr,
    user: String,
    pass: String,
    realm: Vec<u8>,
    nonce: Vec<u8>,
    key: Option<Vec<u8>>,
    relayed: Option<SocketAddr>,
    /// The request waiting for its answer: (transaction, what, peer for a permission).
    asking: HashMap<[u8; 12], (u16, Option<SocketAddr>)>,
    tries: u32,
    expires: Instant,
    perms: HashMap<IpAddr, Instant>,
    failed: bool,
    asked_at: Instant,
}

struct Svc {
    sock: Arc<UdpSocket>,
    port: u16,
    host: Option<SocketAddr>,
    srflx: Vec<SocketAddr>,
    stun: Vec<SocketAddr>,
    asked: HashMap<[u8; 12], Instant>,
    stun_at: Option<Instant>,
    turns: Vec<Turn>,
    ice_at: Option<Instant>,
    used_at: Instant,
    pairs: HashMap<String, Pair>,
    /// Events for window seats, handed over once this lock is let go.
    events: Vec<(String, Value)>,
    relay_only: bool,
    log: Vec<String>,
}

fn svc() -> &'static Mutex<Option<Svc>> {
    static S: OnceLock<Mutex<Option<Svc>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

fn key(tok: &str, peer: &str) -> String {
    format!("{tok}|{peer}")
}

fn b64(b: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(b)
}

fn tid() -> [u8; 12] {
    let mut t = [0u8; 12];
    rand_core::RngCore::fill_bytes(&mut rand_core::OsRng, &mut t);
    t
}

fn env_on(k: &str) -> bool {
    std::env::var(k).is_ok_and(|v| v == "1")
}

/// This computer's address toward the internet (the default route's), the host candidate.
fn host_ip() -> Option<IpAddr> {
    if let Some(ip) = std::env::var("ARCADE_RTC_HOST").ok().and_then(|v| v.parse().ok()) {
        return Some(ip);
    }
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    match s.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_unspecified() => Some(IpAddr::V4(ip)),
        _ => None,
    }
}

// ---------------------------------------------------------------- the service

/// The socket and its thread, on first use.
fn ensure() -> Result<(), String> {
    let mut g = svc().lock().unwrap();
    if g.is_some() {
        return Ok(());
    }
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("no UDP socket for WebRTC: {e}"))?;
    let port = sock.local_addr().map(|a| a.port()).unwrap_or(0);
    let sock = Arc::new(sock);
    let host = host_ip().map(|ip| SocketAddr::new(ip, port));
    *g = Some(Svc {
        sock: sock.clone(),
        port,
        host,
        srflx: Vec::new(),
        stun: Vec::new(),
        asked: HashMap::new(),
        stun_at: None,
        turns: Vec::new(),
        ice_at: None,
        used_at: Instant::now(),
        pairs: HashMap::new(),
        events: Vec::new(),
        relay_only: env_on("ARCADE_RTC_RELAY_ONLY"),
        log: Vec::new(),
    });
    std::thread::Builder::new().name("webrtc".into()).spawn(move || run(sock)).map_err(|e| e.to_string())?;
    Ok(())
}

fn run(sock: Arc<UdpSocket>) {
    let mut buf = vec![0u8; 4096];
    let mut wait = Duration::from_millis(50);
    loop {
        let _ = sock.set_read_timeout(Some(wait.max(Duration::from_millis(1))));
        let got = sock.recv_from(&mut buf);
        let events = {
            let mut g = svc().lock().unwrap();
            let Some(s) = g.as_mut() else { return };
            let now = Instant::now();
            if let Ok((n, from)) = got {
                s.input(now, from, &buf[..n]);
                // What else is waiting, without sleeping.
                let _ = sock.set_nonblocking(true);
                for _ in 0..64 {
                    match sock.recv_from(&mut buf) {
                        Ok((n, from)) => s.input(Instant::now(), from, &buf[..n]),
                        Err(_) => break,
                    }
                }
                let _ = sock.set_nonblocking(false);
            }
            s.tick(Instant::now());
            let now = Instant::now();
            wait = s.pairs.values().map(|p| p.next.saturating_duration_since(now)).min().unwrap_or(Duration::from_millis(250)).min(Duration::from_millis(50));
            if s.pairs.is_empty() {
                wait = Duration::from_millis(250);
            }
            std::mem::take(&mut s.events)
        };
        hand_over(events);
    }
}

fn hand_over(events: Vec<(String, Value)>) {
    for (tok, ev) in events {
        crate::realtime::push_event(&tok, ev);
    }
}

impl Svc {
    fn note(&mut self, line: String) {
        if self.log.len() >= 200 {
            self.log.remove(0);
        }
        self.log.push(line);
    }

    // ------------------------------------------------ the wire in

    fn input(&mut self, now: Instant, from: SocketAddr, data: &[u8]) {
        if let Some(i) = self.turns.iter().position(|t| t.server == from) {
            self.turn_in(i, now, data);
            return;
        }
        // An answer to our own STUN question: this socket's address outside.
        if data.len() >= 20 && data[0] == 0x01 && data[1] == 0x01 {
            if let Some((t, addr)) = arcade_mesh::stun::parse_response(data) {
                if self.asked.remove(&t).is_some() {
                    if !self.srflx.contains(&addr) && addr.is_ipv4() {
                        self.srflx.push(addr);
                        self.note(format!("stun: this socket is {addr} outside"));
                    }
                    return;
                }
            }
        }
        let Some(dest) = self.host.or_else(|| Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), self.port))) else { return };
        self.to_pairs(now, from, dest, data, false);
    }

    fn to_pairs(&mut self, now: Instant, source: SocketAddr, dest: SocketAddr, data: &[u8], relayed: bool) {
        let Ok(r) = Receive::new(Protocol::Udp, source, dest, data) else { return };
        let input = Input::Receive(now, r);
        let Some(k) = self.pairs.iter().find(|(_, p)| p.state != State::Failed && p.rtc.accepts(&input)).map(|(k, _)| k.clone()) else { return };
        let p = self.pairs.get_mut(&k).unwrap();
        if relayed || p.their_relays.contains(&source) {
            p.relayed = true;
        }
        if p.rtc.handle_input(input).is_err() {
            p.fail(now);
        }
        self.drive(&k);
    }

    // ------------------------------------------------ driving a link

    fn drive(&mut self, k: &str) {
        let now = Instant::now();
        loop {
            let Some(p) = self.pairs.get_mut(k) else { return };
            match p.rtc.poll_output() {
                Ok(Output::Timeout(t)) => {
                    p.next = t;
                    break;
                }
                Ok(Output::Transmit(t)) => {
                    let relay = self.turns.iter().position(|r| r.relayed == Some(t.source));
                    let p = self.pairs.get_mut(k).unwrap();
                    p.near = if relay.is_some() { "relay" } else if self.srflx.iter().any(|a| a.ip() == t.source.ip()) { "srflx" } else { "host" };
                    p.far = if p.their_relays.contains(&t.destination) { "relay" } else { p.far };
                    if relay.is_some() || p.their_relays.contains(&t.destination) {
                        p.relayed = true;
                    }
                    match relay {
                        Some(i) => self.turn_send(i, now, t.destination, &t.contents),
                        None => {
                            let _ = self.sock.send_to(&t.contents, t.destination);
                        }
                    }
                }
                Ok(Output::Event(e)) => {
                    let mut out = Vec::new();
                    p.on_event(now, e, &mut out);
                    self.events.extend(out);
                }
                Err(e) => {
                    let line = format!("rtc {}: {e}", short(&p.peer));
                    p.fail(now);
                    self.note(line);
                    break;
                }
            }
        }
    }

    fn tick(&mut self, now: Instant) {
        let keys: Vec<String> = self.pairs.keys().cloned().collect();
        for k in &keys {
            let p = self.pairs.get_mut(k).unwrap();
            if p.state != State::Failed && now >= p.next {
                if p.rtc.handle_input(Input::Timeout(now)).is_err() {
                    p.fail(now);
                }
            }
            p.check(now);
            p.negotiate(now);
            if p.state != State::Failed {
                self.drive(k);
            }
        }
        // Links that failed a while ago, and those whose seat is gone.
        self.pairs.retain(|_, p| {
            let keep = p.failed_at.is_none_or(|t| now.duration_since(t) < FAILED_KEEP) && crate::realtime::seat_alive(&p.tok);
            if !keep {
                p.rtc.disconnect();
            }
            keep
        });
        if !self.pairs.is_empty() {
            self.used_at = now;
        }
        self.keep_turns(now);
        if self.stun_at.is_none_or(|t| now.duration_since(t) >= STUN_EVERY) && (!self.pairs.is_empty() || self.stun_at.is_none()) && !self.stun.is_empty() {
            self.ask_stun(now);
        }
        self.asked.retain(|_, at| now.duration_since(*at) < Duration::from_secs(10));
    }

    fn ask_stun(&mut self, now: Instant) {
        self.stun_at = Some(now);
        for a in self.stun.clone() {
            let (t, req) = arcade_mesh::stun::request();
            self.asked.insert(t, now);
            let _ = self.sock.send_to(&req, a);
        }
    }

    // ------------------------------------------------ the relay (TURN client)

    fn turn_ask(&mut self, i: usize, kind: u16, mut attrs: Vec<(u16, Vec<u8>)>, peer: Option<SocketAddr>) {
        let t = &mut self.turns[i];
        let id = tid();
        if let Some(key) = t.key.clone() {
            attrs.push((tw::A_USERNAME, t.user.as_bytes().to_vec()));
            attrs.push((tw::A_REALM, t.realm.clone()));
            attrs.push((tw::A_NONCE, t.nonce.clone()));
            let msg = tw::build(kind, &id, &attrs, Some(&key));
            t.asking.insert(id, (kind, peer));
            let _ = self.sock.send_to(&msg, t.server);
        } else {
            let msg = tw::build(kind, &id, &attrs, None);
            t.asking.insert(id, (kind, peer));
            let _ = self.sock.send_to(&msg, t.server);
        }
    }

    fn turn_allocate(&mut self, i: usize, now: Instant) {
        let t = &mut self.turns[i];
        t.tries += 1;
        t.asked_at = now;
        if t.tries > 4 {
            t.failed = true;
            return;
        }
        self.turn_ask(i, tw::ALLOCATE, vec![(tw::A_TRANSPORT, vec![tw::UDP, 0, 0, 0])], None);
    }

    fn turn_in(&mut self, i: usize, now: Instant, data: &[u8]) {
        let Some(m) = tw::parse(data) else { return };
        // Data from a peer, through the relay.
        if m.kind == 0x010 | tw::DATA {
            let (Some(peer), Some(d), Some(relayed)) = (m.attrs.get(&tw::A_PEER).and_then(|v| tw::peer_addr(v)), m.attrs.get(&tw::A_DATA), self.turns[i].relayed) else { return };
            let d = d.clone();
            self.to_pairs(now, SocketAddr::V4(peer), relayed, &d, true);
            return;
        }
        let Some((kind, peer)) = self.turns[i].asking.remove(&m.tid) else { return };
        let ok = m.kind == 0x100 | kind;
        let code = m.attrs.get(&tw::A_ERROR).filter(|v| v.len() >= 4).map(|v| v[2] as u16 * 100 + v[3] as u16).unwrap_or(0);
        if !ok {
            if code == 401 || code == 438 {
                let t = &mut self.turns[i];
                if let Some(r) = m.attrs.get(&tw::A_REALM) {
                    t.realm = r.clone();
                }
                if let Some(n) = m.attrs.get(&tw::A_NONCE) {
                    t.nonce = n.clone();
                }
                let words = format!("{}:{}:{}", t.user, String::from_utf8_lossy(&t.realm), t.pass);
                t.key = Some(tw::md5(words.as_bytes()));
                match kind {
                    tw::ALLOCATE => self.turn_allocate(i, now),
                    tw::REFRESH => self.turn_ask(i, tw::REFRESH, vec![(tw::A_LIFETIME, 600u32.to_be_bytes().to_vec())], None),
                    _ => {
                        if let Some(p) = peer {
                            self.turns[i].perms.remove(&p.ip());
                        }
                    }
                }
            } else {
                let line = format!("relay {}: {:#06x} refused ({code})", self.turns[i].server, kind);
                if kind == tw::ALLOCATE {
                    self.turns[i].failed = true;
                }
                self.note(line);
            }
            return;
        }
        let life = m.attrs.get(&tw::A_LIFETIME).filter(|v| v.len() == 4).map(|v| u32::from_be_bytes([v[0], v[1], v[2], v[3]]) as u64).unwrap_or(600);
        match kind {
            tw::ALLOCATE => {
                let relayed = m.attrs.get(&tw::A_RELAYED).and_then(|v| tw::peer_addr(v)).map(SocketAddr::V4);
                let mapped = m.attrs.get(&tw::A_MAPPED).and_then(|v| tw::peer_addr(v)).map(SocketAddr::V4);
                let t = &mut self.turns[i];
                t.relayed = relayed;
                t.expires = now + Duration::from_secs(life);
                let server = t.server;
                if let Some(a) = mapped {
                    if !self.srflx.contains(&a) {
                        self.srflx.push(a);
                    }
                }
                self.note(format!("relay {server}: allocated {relayed:?}"));
            }
            tw::REFRESH => self.turns[i].expires = now + Duration::from_secs(life),
            _ => {}
        }
    }

    fn turn_send(&mut self, i: usize, now: Instant, peer: SocketAddr, data: &[u8]) {
        let SocketAddr::V4(_) = peer else { return };
        let stale = self.turns[i].perms.get(&peer.ip()).is_none_or(|at| now.duration_since(*at) > Duration::from_secs(240));
        if stale {
            self.turns[i].perms.insert(peer.ip(), now);
            let id = tid();
            self.turn_ask(i, tw::PERMISSION, vec![(tw::A_PEER, tw::xor_addr(peer, &id))], Some(peer));
        }
        let id = tid();
        let msg = tw::build(0x010 | tw::SEND, &id, &[(tw::A_PEER, tw::xor_addr(peer, &id)), (tw::A_DATA, data.to_vec())], None);
        let _ = self.sock.send_to(&msg, self.turns[i].server);
    }

    fn keep_turns(&mut self, now: Instant) {
        let idle = now.duration_since(self.used_at) > RELAY_IDLE;
        if idle && !self.turns.is_empty() {
            for i in 0..self.turns.len() {
                if self.turns[i].relayed.is_some() {
                    self.turn_ask(i, tw::REFRESH, vec![(tw::A_LIFETIME, 0u32.to_be_bytes().to_vec())], None);
                }
            }
            self.turns.clear();
            self.ice_at = None;
            return;
        }
        for i in 0..self.turns.len() {
            let t = &self.turns[i];
            if t.failed {
                continue;
            }
            if t.relayed.is_none() && now.duration_since(t.asked_at) > Duration::from_millis(1500) {
                self.turn_allocate(i, now);
            } else if t.relayed.is_some() && t.expires.saturating_duration_since(now) < Duration::from_secs(120) && t.asking.is_empty() {
                self.turn_ask(i, tw::REFRESH, vec![(tw::A_LIFETIME, 600u32.to_be_bytes().to_vec())], None);
                self.turns[i].expires = now + Duration::from_secs(150);
            }
        }
    }

    // ------------------------------------------------ candidates

    fn candidates(&self) -> Vec<Candidate> {
        let mut out = Vec::new();
        let base = self.host.unwrap_or_else(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), self.port));
        for t in &self.turns {
            if let Some(r) = t.relayed {
                if let Ok(c) = Candidate::relayed(r, base, "udp") {
                    out.push(c);
                }
            }
        }
        if self.relay_only {
            return out;
        }
        for a in &self.srflx {
            if Some(*a) != self.host {
                if let Ok(c) = Candidate::server_reflexive(*a, base, "udp") {
                    out.push(c);
                }
            }
        }
        if let Some(h) = self.host {
            if let Ok(c) = Candidate::host(h, "udp") {
                out.push(c);
            }
        }
        out
    }

    fn gathered(&self) -> bool {
        let stun_done = self.stun.is_empty() || !self.srflx.is_empty();
        let relay_done = self.turns.iter().all(|t| t.failed || t.relayed.is_some());
        stun_done && relay_done
    }

    fn relay_waiting(&self) -> bool {
        self.turns.iter().any(|t| !t.failed && t.relayed.is_none())
    }
}

// ---------------------------------------------------------------- one link

impl Pair {
    fn fail(&mut self, now: Instant) {
        if self.state != State::Failed {
            self.state = State::Failed;
            self.failed_at = Some(now);
        }
    }

    fn check(&mut self, now: Instant) {
        match self.state {
            State::Connecting if now.duration_since(self.born) > CONNECT_WAIT => self.fail(now),
            State::Up if self.lost_at.is_some_and(|t| now.duration_since(t) > LOST_WAIT) => self.fail(now),
            _ => {}
        }
        if !self.rtc.is_alive() {
            self.fail(now);
        }
        self.parts.retain(|_, p| now.duration_since(p.at) < Duration::from_secs(3));
    }

    fn chan(&self, i: usize) -> Option<ChannelId> {
        self.chans[i]
    }

    fn say_m(&mut self, v: &Value) {
        let Some(id) = self.chan(2) else { return };
        if let Some(mut c) = self.rtc.channel(id) {
            let _ = c.write(false, v.to_string().as_bytes());
        }
    }

    fn open_channels(&mut self) {
        let cfg = |label: &str, id: u16, ordered: bool, reliability: Reliability| ChannelConfig {
            label: label.into(),
            ordered,
            reliability,
            negotiated: Some(id),
            protocol: String::new(),
        };
        let mut d = self.rtc.direct_api();
        let u = d.create_data_channel(cfg("u", 0, false, Reliability::MaxRetransmits { retransmits: 0 }));
        let r = d.create_data_channel(cfg("r", 1, true, Reliability::Reliable));
        let m = d.create_data_channel(cfg("m", 2, true, Reliability::Reliable));
        self.chans = [Some(u), Some(r), Some(m)];
    }

    fn event(&self, ev: Value, out: &mut Vec<(String, Value)>) {
        let mut ev = ev;
        ev["room"] = json!(self.room);
        ev["member"] = self.who.clone();
        out.push((self.tok.clone(), ev));
    }

    /// The viewer's piece of media (or a note) for the window, cut as media.js cuts.
    fn piece_out(&self, kind: u8, key: bool, seq: u32, t: u32, w: u16, h: u16, body: &[u8], out: &mut Vec<(String, Value)>) {
        for msg in pieces(kind, key, seq, t, w, h, body) {
            self.event(json!({"type": "mchunk", "b64": b64(&msg)}), out);
        }
    }

    fn ask_key(&mut self, now: Instant, out: &mut Vec<(String, Value)>) {
        if self.key_asked.is_some_and(|t| now.duration_since(t) < KEY_ASK_EVERY) {
            return;
        }
        self.key_asked = Some(now);
        self.piece_out(K_NOTE, false, 0, 0, 0, 0, br#"{"kf":1}"#, out);
    }

    fn pli(&mut self, now: Instant) {
        if self.pli_at.is_some_and(|t| now.duration_since(t) < Duration::from_millis(500)) {
            return;
        }
        self.pli_at = Some(now);
        let mids: Vec<Mid> = self.rx.keys().copied().collect();
        for mid in mids {
            if self.rtc.media(mid).is_some_and(|m| m.kind() == MediaKind::Video) {
                if let Some(w) = self.rtc.writer(mid) {
                    let mut w = w;
                    let _ = w.request_keyframe(None, KeyframeRequestKind::Pli);
                }
            }
        }
    }

    fn on_event(&mut self, now: Instant, e: Event, out: &mut Vec<(String, Value)>) {
        match e {
            Event::Connected => {
                self.state = State::Up;
                self.lost_at = None;
            }
            Event::IceConnectionStateChange(s) => match s {
                IceConnectionState::Disconnected => {
                    if self.lost_at.is_none() {
                        self.lost_at = Some(now);
                    }
                }
                IceConnectionState::Connected | IceConnectionState::Completed => self.lost_at = None,
                _ => {}
            },
            Event::ChannelOpen(id, _) => {
                if Some(id) == self.chan(2) {
                    self.m_open = true;
                    if self.tx.values().any(|t| t.want) {
                        self.soon(now);
                    }
                }
            }
            Event::ChannelClose(id) => {
                if Some(id) == self.chan(2) {
                    self.m_open = false;
                }
            }
            Event::ChannelData(d) => {
                if Some(d.id) == self.chan(2) {
                    if let Ok(text) = std::str::from_utf8(&d.data) {
                        let text = text.to_string();
                        self.on_m(now, &text, out);
                    }
                } else if Some(d.id) == self.chan(0) || Some(d.id) == self.chan(1) {
                    let ev = match (d.binary, std::str::from_utf8(&d.data)) {
                        (false, Ok(t)) => json!({"type": "fast", "data": t}),
                        _ => json!({"type": "fast", "b64": b64(&d.data)}),
                    };
                    self.event(ev, out);
                }
            }
            Event::MediaAdded(m) => {
                if m.direction.is_receiving() {
                    self.rx.entry(m.mid).or_insert(Rx { seq: 0, w: 0, h: 0 });
                }
            }
            Event::MediaData(d) => self.media_in(now, d, out),
            Event::KeyframeRequest(r) => {
                if self.tx.values().any(|t| t.mid == Some(r.mid)) {
                    self.ask_key(now, out);
                }
            }
            _ => {}
        }
    }

    /// RTP from the viewer, put back together into a frame: to the window as pieces.
    fn media_in(&mut self, now: Instant, d: str0m::media::MediaData, out: &mut Vec<(String, Value)>) {
        let Some(m) = self.rtc.media(d.mid) else { return };
        let video = m.kind() == MediaKind::Video;
        let second = !self.xid.is_empty() && m.stream_id() == self.xid;
        let kind = match (video, second) {
            (true, false) => K_CAM,
            (true, true) => K_SCR,
            (false, false) => K_MIC,
            (false, true) => K_SND,
        };
        let codec = d.params.spec().codec;
        if (video && codec != Codec::Vp8) || (!video && codec != Codec::Opus) {
            return;
        }
        let key = !video || d.is_keyframe();
        let rx = self.rx.entry(d.mid).or_insert(Rx { seq: 0, w: 0, h: 0 });
        if video && key {
            if let Some((w, h)) = vp8_size(&d.data) {
                rx.w = w;
                rx.h = h;
            }
        }
        let (seq, w, h) = (rx.seq, rx.w, rx.h);
        rx.seq = rx.seq.wrapping_add(1);
        if video && (w == 0 || (!d.contiguous && !key)) {
            // A gap, or nothing to start a picture from: a key frame, please.
            self.pli(now);
            if w == 0 {
                return;
            }
        }
        let t = d.time.rebase(Frequency::MILLIS).numer() as u32;
        self.heard += 1;
        self.piece_out(kind, key, seq, t, w, h, &d.data, out);
    }

    /// A message on the media channel 'm': the viewer's perfect negotiation (mediaPc).
    fn on_m(&mut self, now: Instant, text: &str, out: &mut Vec<(String, Value)>) {
        let Ok(msg) = serde_json::from_str::<Value>(text) else { return };
        if let Some(x) = msg["x"].as_str() {
            self.xid = x.chars().take(128).collect();
            return;
        }
        if !msg["yours"].is_null() && msg["yours"] != false && msg["yours"] != 0 {
            // They could not take our offer and make one of their own: ours is undone.
            self.pending = None;
            self.wait_until = Some(now + MEDIA_AFTER);
            return;
        }
        let d = &msg["d"];
        let (Some(kind), Some(sdp)) = (d["type"].as_str(), d["sdp"].as_str()) else { return };
        match kind {
            "offer" => {
                if self.pending.is_some() && !self.polite {
                    return; // theirs is ignored, ours goes on (they give way)
                }
                self.pending = None; // ours undone (a rollback is dropping it)
                let ours: HashSet<String> = self.tx.values().filter_map(|t| t.mid.map(|m| m.to_string())).collect();
                let sdp = &theirs_recv_only(sdp, &ours);
                let answer = SdpOffer::from_sdp_string(sdp).map_err(|e| e.to_string()).and_then(|o| self.rtc.sdp_api().accept_offer(o).map_err(|e| e.to_string()));
                match answer {
                    Ok(a) => {
                        let sdp = a.to_sdp_string();
                        self.refused_video(&sdp);
                        self.say_m(&json!({"d": {"type": "answer", "sdp": sdp}}));
                        if self.wants_change() {
                            self.soon(now);
                        }
                    }
                    Err(e) => {
                        self.refused.push(format!("offer: {e}"));
                        self.say_m(&json!({"yours": 1}));
                        self.offer_at = Some(now);
                    }
                }
            }
            "answer" => {
                let Some((pending, planned, _)) = self.pending.take() else { return };
                let ok = SdpAnswer::from_sdp_string(sdp).map_err(|e| e.to_string()).and_then(|a| self.rtc.sdp_api().accept_answer(pending, a).map_err(|e| e.to_string()));
                match ok {
                    Ok(()) => {
                        for (kind, mid, active) in planned {
                            let t = self.tx.entry(kind).or_default();
                            t.mid = Some(mid);
                            if active && !t.active {
                                t.need_key = kind == K_CAM || kind == K_SCR;
                            }
                            t.active = active;
                        }
                        if self.tx.values().any(|t| t.active && t.need_key) {
                            self.ask_key(now, out);
                        }
                        if self.wants_change() {
                            self.soon(now);
                        }
                    }
                    Err(e) => self.refused.push(format!("answer: {e}")),
                }
            }
            _ => {}
        }
    }

    /// A video section the viewer offered that we answered with port 0 (no VP8 there).
    fn refused_video(&mut self, answer: &str) {
        if answer.lines().any(|l| l.starts_with("m=video 0 ")) {
            let line = "video refused: the viewer offered no VP8".to_string();
            if !self.refused.contains(&line) {
                self.refused.push(line);
            }
        }
    }

    fn wants_change(&self) -> bool {
        self.tx.values().any(|t| t.want != t.active)
    }

    fn soon(&mut self, now: Instant) {
        if self.offer_at.is_none() {
            self.offer_at = Some(now + if self.polite { MEDIA_YIELD } else { Duration::ZERO });
        }
    }

    /// Our own offer, when what the window sends changed and the link is free for it.
    fn negotiate(&mut self, now: Instant) {
        if let Some((_, _, at)) = &self.pending {
            if now.duration_since(*at) > MEDIA_STUCK {
                self.pending = None;
                self.offer_at = Some(now);
            }
            return;
        }
        let Some(at) = self.offer_at else { return };
        if now < at || !self.m_open || self.state != State::Up {
            return;
        }
        if self.wait_until.is_some_and(|t| now < t) {
            return;
        }
        self.offer_at = None;
        if !self.wants_change() {
            return;
        }
        let stream = self.stream.clone();
        let xstream = self.xstream.clone();
        let mut screen = false;
        let mut planned = Vec::new();
        let mut api = self.rtc.sdp_api();
        for (kind, t) in self.tx.iter() {
            if t.want == t.active {
                continue;
            }
            match t.mid {
                None if t.want => {
                    let second = *kind == K_SCR || *kind == K_SND;
                    screen |= second;
                    let mk = if *kind == K_CAM || *kind == K_SCR { MediaKind::Video } else { MediaKind::Audio };
                    let sid = if second { xstream.clone() } else { stream.clone() };
                    let mid = api.add_media(mk, Direction::SendOnly, Some(sid.clone()), Some(format!("{sid}-{kind}")), None);
                    planned.push((*kind, mid, true));
                }
                None => {}
                Some(mid) => {
                    api.set_direction(mid, if t.want { Direction::SendOnly } else { Direction::Inactive });
                    planned.push((*kind, mid, t.want));
                }
            }
        }
        if !api.has_changes() {
            return;
        }
        let Some((offer, pending)) = api.apply() else { return };
        if screen && !self.said_x {
            // The second picture's stream id goes first, on the channel that also carries the offer.
            self.said_x = true;
            self.say_m(&json!({"x": xstream}));
        }
        self.say_m(&json!({"d": {"type": "offer", "sdp": offer.to_sdp_string()}}));
        self.pending = Some((pending, planned, now));
    }

    /// A piece of the window's media for this viewer.
    fn piece_in(&mut self, now: Instant, u: &[u8], out: &mut Vec<(String, Value)>) {
        let Some(h) = Head::parse(u) else { return };
        if (h.kind as usize) < 10 {
            self.pieces_by_kind[h.kind as usize] += 1;
        }
        let body = &u[HEAD..];
        let whole: Vec<u8> = if h.parts > 1 {
            let p = self.parts.entry((h.kind, h.seq)).or_insert_with(|| Part { parts: h.parts, got: vec![None; h.parts as usize], at: now });
            if p.parts != h.parts || h.part >= h.parts {
                return;
            }
            p.got[h.part as usize] = Some(body.to_vec());
            if p.got.iter().any(|x| x.is_none()) {
                return;
            }
            let p = self.parts.remove(&(h.kind, h.seq)).unwrap();
            p.got.into_iter().flatten().flatten().collect()
        } else {
            body.to_vec()
        };
        if h.kind == K_NOTE {
            let Ok(o) = serde_json::from_slice::<Value>(&whole) else { return };
            if o["kf"].is_number() || o["kf"] == true {
                self.pli(now);
            }
            if o["off"].is_number() || o["off"] == true {
                for t in self.tx.values_mut() {
                    t.want = false;
                }
                self.soon(now);
            }
            if o["screenOff"].is_number() || o["screenOff"] == true {
                for k in [K_SCR, K_SND] {
                    if let Some(t) = self.tx.get_mut(&k) {
                        t.want = false;
                    }
                }
                self.soon(now);
            }
            return;
        }
        if ![K_CAM, K_MIC, K_SCR, K_SND].contains(&h.kind) {
            return;
        }
        let t = self.tx.entry(h.kind).or_default();
        if !t.want {
            t.want = true;
            t.need_key = h.kind == K_CAM || h.kind == K_SCR;
        }
        let (Some(mid), true) = (t.mid, t.active) else {
            self.waiting += 1;
            self.soon(now);
            return;
        };
        let video = h.kind == K_CAM || h.kind == K_SCR;
        if video && t.need_key {
            if !h.key {
                self.held += 1;
                self.ask_key(now, out);
                return;
            }
            self.tx.get_mut(&h.kind).unwrap().need_key = false;
        }
        let Some(mut w) = self.rtc.writer(mid) else { return };
        if !video && h.w > 0 {
            // The voice's loudness, for the viewer's "who is speaking" (RFC 6464).
            let db = (h.w - 1).min(127) as i8;
            w = w.audio_level(-db, db < 50);
        }
        let want = if video { Codec::Vp8 } else { Codec::Opus };
        let Some(pt) = w.payload_params().find(|p| p.spec().codec == want).map(|p| p.pt()) else { return };
        let rtp = if video { MediaTime::new(h.t as u64 * 90, Frequency::NINETY_KHZ) } else { MediaTime::new(h.t as u64 * 48, Frequency::FORTY_EIGHT_KHZ) };
        match w.write(pt, now, rtp, whole) {
            Ok(()) => self.sent += 1,
            Err(e) => {
                self.refused.push(format!("write: {e}"));
                self.refused.truncate(20);
            }
        }
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

// ---------------------------------------------------------------- pieces (media.js's wire)

/// A piece's header: 0xA1, kind, flags (1: key frame), seq (u32), time (u32, ms), part, parts,
/// width (u16), height (u16).
struct Head {
    kind: u8,
    key: bool,
    seq: u32,
    t: u32,
    part: u8,
    parts: u8,
    /// A picture's width; for sound, 1 + its level in -dBov (media.js levelWord), 0 when not said.
    w: u16,
}

impl Head {
    fn parse(u: &[u8]) -> Option<Head> {
        if u.len() < HEAD || u[0] != 0xA1 {
            return None;
        }
        let be = |a: usize| u32::from_be_bytes([u[a], u[a + 1], u[a + 2], u[a + 3]]);
        Some(Head { kind: u[1], key: u[2] & 1 == 1, seq: be(3), t: be(7), part: u[11], parts: u[12].max(1), w: u16::from_be_bytes([u[13], u[14]]) })
    }
}

/// A whole frame cut into pieces as media.js cuts them (PIECE bytes of frame each).
fn pieces(kind: u8, key: bool, seq: u32, t: u32, w: u16, h: u16, body: &[u8]) -> Vec<Vec<u8>> {
    let parts = body.len().div_ceil(PIECE).max(1);
    if parts > 255 {
        return Vec::new();
    }
    (0..parts)
        .map(|i| {
            let part = &body[(i * PIECE).min(body.len())..((i + 1) * PIECE).min(body.len())];
            let mut m = Vec::with_capacity(HEAD + part.len());
            m.extend_from_slice(&[0xA1, kind, key as u8]);
            m.extend_from_slice(&seq.to_be_bytes());
            m.extend_from_slice(&t.to_be_bytes());
            m.extend_from_slice(&[i as u8, parts as u8]);
            m.extend_from_slice(&w.to_be_bytes());
            m.extend_from_slice(&h.to_be_bytes());
            m.extend_from_slice(part);
            m
        })
        .collect()
}

/// A VP8 key frame's picture size (RFC 6386 9.1: start code 9d 01 2a, then 14-bit sizes).
fn vp8_size(f: &[u8]) -> Option<(u16, u16)> {
    if f.len() < 10 || f[0] & 1 != 0 || f[3..6] != [0x9d, 0x01, 0x2a] {
        return None;
    }
    let w = u16::from_le_bytes([f[6], f[7]]) & 0x3fff;
    let h = u16::from_le_bytes([f[8], f[9]]) & 0x3fff;
    (w > 0 && h > 0).then_some((w, h))
}

// ---------------------------------------------------------------- compact descriptions (sdpPack)

fn token_ok(s: &str, most: usize) -> bool {
    !s.is_empty() && s.len() <= most && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
}

/// The candidates of a viewer's description that this arcade can use: "ip port kind".
fn their_candidates(o: &Value) -> Vec<(SocketAddr, char)> {
    let mut out = Vec::new();
    for c in o["c"].as_array().into_iter().flatten().take(MOST_CANDIDATES) {
        let f: Vec<&str> = c.as_str().unwrap_or("").split(' ').collect();
        if f.len() != 3 {
            continue;
        }
        // A host hidden behind an mDNS name can't be used here: its checks still come in (prflx).
        let (Ok(ip), Ok(port)) = (f[0].parse::<IpAddr>(), f[1].parse::<u16>()) else { continue };
        let k = f[2].chars().next().unwrap_or('h');
        if "hspr".contains(k) && port != 0 && f[2].len() == 1 {
            out.push((SocketAddr::new(ip, port), k));
        }
    }
    out
}

/// A viewer's compact description as a whole offer (as sdpUnpack builds it), with `setup`.
fn unpack(o: &Value, setup: &str) -> Result<String, String> {
    let (u, p, f) = (o["u"].as_str().unwrap_or(""), o["p"].as_str().unwrap_or(""), o["f"].as_str().unwrap_or(""));
    if !token_ok(u, 256) || !token_ok(p, 256) || f.len() != 64 || !f.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("a description needs u, p and a sha-256 f".into());
    }
    if !["actpass", "active", "passive"].contains(&setup) {
        return Err("setup is actpass, active or passive".into());
    }
    let fp: Vec<String> = f.to_uppercase().as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).to_string()).collect();
    let mut lines = vec![
        "v=0".to_string(),
        "o=- 4611731400430051336 2 IN IP4 127.0.0.1".into(),
        "s=-".into(),
        "t=0 0".into(),
        "a=group:BUNDLE 0".into(),
        "a=msid-semantic: WMS".into(),
        "m=application 9 UDP/DTLS/SCTP webrtc-datachannel".into(),
        "c=IN IP4 0.0.0.0".into(),
        format!("a=ice-ufrag:{u}"),
        format!("a=ice-pwd:{p}"),
        format!("a=fingerprint:sha-256 {}", fp.join(":")),
        format!("a=setup:{setup}"),
        "a=mid:0".into(),
        "a=sctp-port:5000".into(),
        "a=max-message-size:262144".into(),
    ];
    for (i, (a, k)) in their_candidates(o).into_iter().enumerate() {
        let (typ, prio) = match k {
            's' => ("srflx", 1694498815u32),
            'p' => ("prflx", 1862270975),
            'r' => ("relay", 16777215),
            _ => ("host", 2130706431),
        };
        let raddr = if typ == "host" { String::new() } else { " raddr 0.0.0.0 rport 0".into() };
        lines.push(format!("a=candidate:{} 1 udp {} {} {} typ {typ}{raddr}", i + 1, prio - i as u32, a.ip(), a.port()));
    }
    lines.push("a=end-of-candidates".into());
    Ok(lines.join("\r\n") + "\r\n")
}

/// This arcade's side as a compact description: relays first, then public, then host.
fn pack(rtc: &mut Rtc, t: &str, setup: &str, cands: &[Candidate]) -> Value {
    let creds = rtc.direct_api().local_ice_credentials();
    let f: String = rtc.direct_api().local_dtls_fingerprint().bytes.iter().map(|b| format!("{b:02x}")).collect();
    let mut c = Vec::new();
    for want in [CandidateKind::Relayed, CandidateKind::ServerReflexive, CandidateKind::Host] {
        for x in cands.iter().filter(|x| x.kind() == want) {
            let k = match want {
                CandidateKind::Relayed => "r",
                CandidateKind::ServerReflexive => "s",
                _ => "h",
            };
            if c.len() < MOST_CANDIDATES {
                c.push(format!("{} {} {k}", x.addr().ip(), x.addr().port()));
            }
        }
    }
    json!({"t": t, "u": creds.ufrag, "p": creds.pass, "f": f, "s": setup, "c": c})
}

/// The viewer's offer with its own send-and-receive sections made send-only. This arcade sends on
/// sections of its own (one per kind of the window's media); a send-and-receive answer on the
/// viewer's would hand its page an empty track beside the real one (in the same stream, so its
/// picture box could show the empty one). A section of ours that the viewer also sends on keeps both.
fn theirs_recv_only(sdp: &str, ours: &HashSet<String>) -> String {
    let mut out = String::with_capacity(sdp.len());
    let mut sections: Vec<Vec<&str>> = vec![Vec::new()];
    for line in sdp.split("\r\n") {
        if line.starts_with("m=") {
            sections.push(Vec::new());
        }
        sections.last_mut().unwrap().push(line);
    }
    for (i, sec) in sections.iter().enumerate() {
        let mid = sec.iter().find_map(|l| l.strip_prefix("a=mid:")).unwrap_or("");
        let theirs = i > 0 && sec.first().is_some_and(|l| l.starts_with("m=audio") || l.starts_with("m=video")) && !ours.contains(mid);
        for line in sec {
            if !out.is_empty() {
                out.push_str("\r\n");
            }
            out.push_str(if theirs && *line == "a=sendrecv" { "a=sendonly" } else { line });
        }
    }
    out
}

fn setup_of(sdp: &str) -> String {
    sdp.lines().find_map(|l| l.strip_prefix("a=setup:")).map(|s| s.trim().to_string()).unwrap_or_else(|| "active".into())
}

fn new_rtc() -> Rtc {
    Rtc::builder().clear_codecs().enable_vp8(true).enable_opus(true, false).build(Instant::now())
}

// ---------------------------------------------------------------- what the window asks

/// ICE servers for this arcade's own side: realtime.ice's (its relay where proved, else linked
/// arcades'), linked arcades' proved STUN, and test overrides.
fn ice_servers(node: &Node) -> (Vec<SocketAddr>, Vec<(SocketAddr, String, String)>) {
    let parse = |v: &str| -> Vec<SocketAddr> { v.split(',').filter_map(|a| a.trim().parse().ok()).collect() };
    let stun_env = std::env::var("ARCADE_RTC_STUN").ok();
    let turn_env = std::env::var("ARCADE_RTC_TURN").ok();
    if stun_env.is_some() || turn_env.is_some() {
        let stun = stun_env.map(|v| parse(&v)).unwrap_or_default();
        let turn = turn_env
            .map(|v| parse(&v))
            .unwrap_or_default()
            .into_iter()
            .filter_map(|a| tw::login("webrtc").map(|(u, p)| (a, u, p)))
            .collect();
        return (stun, turn);
    }
    let ice = tw::ice(node, &format!("rtc-{}", &node.id_hex()[..12.min(node.id_hex().len())]), false);
    let mut stun: Vec<SocketAddr> = Vec::new();
    for s in ice["stun"].as_array().into_iter().flatten() {
        if let Some(a) = s.as_str().and_then(|s| s.strip_prefix("stun:")).and_then(|a| a.parse().ok()) {
            stun.push(a);
        }
    }
    for (_, hosts) in crate::udpsvc::peer_stun() {
        stun.extend(hosts.into_iter().filter(|a| a.is_ipv4()));
    }
    stun.sort();
    stun.dedup();
    stun.truncate(4);
    let mut turn = Vec::new();
    for s in ice["iceServers"].as_array().into_iter().flatten() {
        let (Some(u), Some(p)) = (s["username"].as_str(), s["credential"].as_str()) else { continue };
        let first = s["urls"].as_array().into_iter().flatten().filter_map(|x| x.as_str()).find_map(|x| {
            let a = x.strip_prefix("turn:")?.split('?').next()?;
            a.parse::<SocketAddr>().ok().filter(|a| a.is_ipv4())
        });
        if let Some(a) = first {
            turn.push((a, u.to_string(), p.to_string()));
        }
        if turn.len() >= 2 {
            break;
        }
    }
    (stun, turn)
}

/// Ask for this arcade's candidates (once in a while) and wait until they are in, as a viewer waits.
fn gather(node: &Node) -> Result<(), String> {
    ensure()?;
    let stale = svc().lock().unwrap().as_ref().is_none_or(|s| s.ice_at.is_none_or(|t| t.elapsed() > ICE_KEEP));
    if stale {
        let (stun, turn) = ice_servers(node);
        let mut g = svc().lock().unwrap();
        let s = g.as_mut().unwrap();
        let now = Instant::now();
        s.ice_at = Some(now);
        s.used_at = now;
        s.stun = stun;
        for (server, user, pass) in turn {
            if s.turns.iter().any(|t| t.server == server) {
                continue;
            }
            s.turns.push(Turn {
                server,
                user,
                pass,
                realm: Vec::new(),
                nonce: Vec::new(),
                key: None,
                relayed: None,
                asking: HashMap::new(),
                tries: 0,
                expires: now,
                perms: HashMap::new(),
                failed: false,
                asked_at: now,
            });
            let i = s.turns.len() - 1;
            s.turn_allocate(i, now);
        }
        s.ask_stun(now);
    }
    let start = Instant::now();
    loop {
        {
            let g = svc().lock().unwrap();
            let s = g.as_ref().unwrap();
            let waited = start.elapsed();
            if s.gathered() || waited >= RELAY_WAIT || (waited >= GATHER_WAIT && !s.relay_waiting()) {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn new_pair(s: &mut Svc, tok: &str, peer: &str, room: String, me: &str, who: Value) -> Result<Pair, String> {
    if s.pairs.len() >= MAX_PAIRS {
        return Err("too many WebRTC links on this arcade".into());
    }
    if s.pairs.values().filter(|p| p.tok == tok).count() >= PAIRS_PER_SEAT {
        return Err("too many WebRTC links for this seat".into());
    }
    let mut rtc = new_rtc();
    let cands = s.candidates();
    if cands.is_empty() {
        return Err("no candidates here (no network?)".into());
    }
    for c in cands {
        rtc.add_local_candidate(c);
    }
    Ok(Pair::new(tok, peer, room, who, me > peer, rtc))
}

impl Pair {
    fn new(tok: &str, peer: &str, room: String, who: Value, polite: bool, rtc: Rtc) -> Pair {
    let r = arcade_mesh::random::hex(4);
    let now = Instant::now();
    Pair {
        tok: tok.into(),
        peer: peer.into(),
        room,
        who,
        rtc,
        born: now,
        state: State::Connecting,
        failed_at: None,
        lost_at: None,
        next: now,
        offered: false,
        polite,
        chans: [None, None, None],
        m_open: false,
        pending: None,
        offer_at: None,
        wait_until: None,
        tx: BTreeMap::new(),
        rx: HashMap::new(),
        xid: String::new(),
        stream: format!("arcade-{r}"),
        xstream: format!("arcade-x-{r}"),
        said_x: false,
        parts: HashMap::new(),
        key_asked: None,
        pli_at: None,
        their_relays: HashSet::new(),
        relayed: false,
        near: "",
        far: "",
        refused: Vec::new(),
        sent: 0,
        heard: 0,
        held: 0,
        pieces_by_kind: [0; 10],
        waiting: 0,
    }
    }
}

/// The viewer's description taken: their candidates added, the link's channels made.
fn take_theirs(p: &mut Pair, o: &Value, setup: &str) -> Result<String, String> {
    let sdp = unpack(o, setup)?;
    let offer = SdpOffer::from_sdp_string(&sdp).map_err(|e| format!("their description: {e}"))?;
    let answer = p.rtc.sdp_api().accept_offer(offer).map_err(|e| format!("their description: {e}"))?;
    for (a, k) in their_candidates(o) {
        if k == 'r' {
            p.their_relays.insert(a);
        }
    }
    p.open_channels();
    Ok(setup_of(&answer.to_sdp_string()))
}

/// realtime.rtc: a WebRTC link between this window seat (token) and the viewer `peer`.
/// op "offer": our compact offer (this seat's id is the lower); "signal": theirs ('o' or 'a'),
/// answered with ours for an offer; "close".
pub fn api(node: &Node, tok: &str, peer: &str, op: &str, signal: &Value) -> Result<Value, String> {
    let k = key(tok, peer);
    if op == "close" {
        // Also for a player who has left the room already.
        if !crate::realtime::seat_alive(tok) {
            return Err("join the room first".into());
        }
        if let Some(s) = svc().lock().unwrap().as_mut() {
            if let Some(mut p) = s.pairs.remove(&k) {
                p.rtc.disconnect();
            }
        }
        return Ok(json!({}));
    }
    let (room, me, who) = crate::realtime::rtc_seat(node, tok, peer)?;
    match op {
        "offer" => {
            gather(node)?;
            let mut g = svc().lock().unwrap();
            let s = g.as_mut().ok_or("no WebRTC here")?;
            let mut p = new_pair(s, tok, peer, room, &me, who)?;
            let cands = s.candidates();
            let o = pack(&mut p.rtc, "o", "actpass", &cands);
            p.offered = true;
            // Our first offer: a viewer's answer is read back as an offer (take_theirs), and the
            // side that offered first controls ICE.
            if let Some(mut old) = s.pairs.insert(k, p) {
                old.rtc.disconnect();
            }
            let mut o = o;
            o["v1"] = json!(1);
            Ok(json!({"signal": o}))
        }
        "signal" => match signal["t"].as_str() {
            Some("o") => {
                gather(node)?;
                let mut g = svc().lock().unwrap();
                let s = g.as_mut().ok_or("no WebRTC here")?;
                let mut p = new_pair(s, tok, peer, room, &me, who)?;
                let setup = take_theirs(&mut p, signal, signal["s"].as_str().unwrap_or("actpass"))?;
                let cands = s.candidates();
                let a = pack(&mut p.rtc, "a", &setup, &cands);
                let line = format!("rtc {}: answered ({} candidates of theirs)", short(peer), their_candidates(signal).len());
                if let Some(mut old) = s.pairs.insert(k.clone(), p) {
                    old.rtc.disconnect();
                }
                s.note(line);
                s.drive(&k);
                Ok(json!({"signal": a}))
            }
            Some("a") => {
                let mut g = svc().lock().unwrap();
                let s = g.as_mut().ok_or("no WebRTC here")?;
                let p = s.pairs.get_mut(&k).filter(|p| p.offered && p.state == State::Connecting).ok_or("no offer of ours waits for this answer")?;
                p.offered = false;
                let theirs = signal["s"].as_str().unwrap_or("active");
                let setup = if theirs == "actpass" { "active" } else { theirs };
                take_theirs(p, signal, setup)?;
                p.rtc.direct_api().set_ice_controlling(true);
                let line = format!("rtc {}: their answer taken ({} candidates)", short(peer), their_candidates(signal).len());
                s.note(line);
                s.drive(&k);
                let ev = std::mem::take(&mut s.events);
                drop(g);
                hand_over(ev);
                Ok(json!({}))
            }
            _ => Err("signal: a compact description with t 'o' or 'a'".into()),
        },
        _ => Err("op: offer, signal or close".into()),
    }
}

/// realtime.media for players linked here: their pieces go on the link; the rest are returned for
/// the lane. A player whose link is still being made gets nothing meanwhile (a viewer that hears
/// a piece takes the player for one without WebRTC).
pub fn take_media(tok: &str, to: &[String], bytes: &[u8]) -> Vec<String> {
    route(tok, to, |s, k, now, out| {
        let p = s.pairs.get_mut(k).unwrap();
        if p.state == State::Up {
            p.piece_in(now, bytes, out);
        }
    })
}

/// realtime.fast for players linked here: on 'r' (reliable) or 'u'.
pub fn take_fast(tok: &str, to: &[String], bytes: &[u8], bin: bool, reliable: bool) -> Vec<String> {
    route(tok, to, |s, k, _, _| {
        let p = s.pairs.get_mut(k).unwrap();
        if p.state != State::Up {
            return;
        }
        if let Some(id) = p.chan(if reliable { 1 } else { 0 }) {
            if let Some(mut c) = p.rtc.channel(id) {
                let _ = c.write(bin, bytes);
            }
        }
    })
}

fn route(tok: &str, to: &[String], mut each: impl FnMut(&mut Svc, &str, Instant, &mut Vec<(String, Value)>)) -> Vec<String> {
    let mut g = svc().lock().unwrap();
    let Some(s) = g.as_mut() else { return to.to_vec() };
    let now = Instant::now();
    let mut rest = Vec::new();
    let mut out = Vec::new();
    for id in to {
        let k = key(tok, id);
        match s.pairs.get(&k).map(|p| p.state) {
            Some(State::Up) | Some(State::Connecting) => {
                each(s, &k, now, &mut out);
                s.drive(&k);
            }
            _ => rest.push(id.clone()),
        }
    }
    out.extend(std::mem::take(&mut s.events));
    drop(g);
    hand_over(out);
    rest
}

/// The seat's links: id -> (connecting / up / failed, direct / relay), for realtime.lanes.
pub fn states(tok: &str) -> BTreeMap<String, (&'static str, &'static str)> {
    let g = svc().lock().unwrap();
    let Some(s) = g.as_ref() else { return BTreeMap::new() };
    s.pairs
        .values()
        .filter(|p| p.tok == tok)
        .map(|p| {
            let st = match p.state {
                State::Connecting => "connecting",
                State::Up => "up",
                State::Failed => "failed",
            };
            (p.peer.clone(), (st, if p.relayed { "relay" } else { "direct" }))
        })
        .collect()
}

/// realtime.rtc_status: the socket, its candidates, the relays and every link.
pub fn status() -> Value {
    let g = svc().lock().unwrap();
    let Some(s) = g.as_ref() else { return json!({"on": false}) };
    let pairs: Vec<Value> = s
        .pairs
        .values()
        .map(|p| {
            json!({"peer": p.peer, "state": format!("{:?}", p.state).to_lowercase(), "route": if p.relayed { "relay" } else { "direct" },
                   "near": p.near, "far": p.far, "age_s": p.born.elapsed().as_secs(), "m_open": p.m_open,
                   "sending": p.tx.iter().filter(|(_, t)| t.active).map(|(k, _)| *k).collect::<Vec<u8>>(),
                   "receiving": p.rx.len(), "problems": p.refused, "frames_sent": p.sent, "frames_heard": p.heard, "held_for_key": p.held, "pieces_in": p.pieces_by_kind, "waiting": p.waiting,
                   "tx": p.tx.iter().map(|(k, t)| json!({"kind": k, "mid": t.mid.map(|m| m.to_string()), "active": t.active, "want": t.want, "need_key": t.need_key})).collect::<Vec<_>>(),
                   "negotiating": p.pending.is_some()})
        })
        .collect();
    let turns: Vec<Value> = s.turns.iter().map(|t| json!({"server": t.server.to_string(), "relayed": t.relayed.map(|a| a.to_string()), "failed": t.failed})).collect();
    json!({"on": true, "port": s.port, "host": s.host.map(|a| a.to_string()), "srflx": s.srflx.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
           "stun": s.stun.iter().map(|a| a.to_string()).collect::<Vec<_>>(), "relays": turns, "relay_only": s.relay_only, "links": pairs,
           "log": s.log.iter().rev().take(40).collect::<Vec<_>>()})
}

#[cfg(test)]
mod tests;
