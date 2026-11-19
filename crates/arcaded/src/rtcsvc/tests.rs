//! rtcsvc: pieces, compact descriptions, and RTP both ways between this arcade's side of a link
//! and a second str0m playing the viewer, packets carried in memory on made-up addresses.

use super::*;
use str0m::crypto::Fingerprint;
use str0m::media::MediaData;
use str0m::rtp::RtpPacket;
use str0m::IceCreds;

const A: &str = "10.0.0.1:6000";
const B: &str = "10.0.0.2:5000";

fn addr(a: &str) -> SocketAddr {
    a.parse().unwrap()
}

fn blank(rtc: Rtc, polite: bool) -> Pair {
    Pair::new("tok", "guest-viewer01", "room".into(), json!({"id": "guest-viewer01"}), polite, rtc)
}

/// What one side said this step.
#[derive(Default)]
struct Heard {
    window: Vec<(String, Value)>,
    viewer: Vec<Event>,
}

/// Run both sides for `ms` of made-up time, carrying packets between them; `drop` may lose
/// packets going from this arcade to the viewer. Each side is polled after every input (str0m
/// keeps what an input produced only until the next one).
fn run(p: &mut Pair, v: &mut Rtc, now: &mut Instant, ms: u64, drop: &mut dyn FnMut(&[u8]) -> bool, heard: &mut Heard) {
    fn drain_p(p: &mut Pair, now: Instant, to_v: &mut Vec<Vec<u8>>, drop: &mut dyn FnMut(&[u8]) -> bool, heard: &mut Heard) {
        loop {
            match p.rtc.poll_output() {
                Ok(Output::Timeout(_)) | Err(_) => break,
                Ok(Output::Transmit(t)) => {
                    if !drop(&t.contents) {
                        to_v.push(t.contents.to_vec());
                    }
                }
                Ok(Output::Event(e)) => p.on_event(now, e, &mut heard.window),
            }
        }
    }
    fn drain_v(v: &mut Rtc, to_a: &mut Vec<Vec<u8>>, heard: &mut Heard) {
        loop {
            match v.poll_output() {
                Ok(Output::Timeout(_)) | Err(_) => break,
                Ok(Output::Transmit(t)) => to_a.push(t.contents.to_vec()),
                Ok(Output::Event(e)) => heard.viewer.push(e),
            }
        }
    }
    let end = *now + Duration::from_millis(ms);
    let (mut to_v, mut to_a): (Vec<Vec<u8>>, Vec<Vec<u8>>) = (Vec::new(), Vec::new());
    while *now < end {
        let _ = p.rtc.handle_input(Input::Timeout(*now));
        drain_p(p, *now, &mut to_v, drop, heard);
        p.negotiate(*now);
        drain_p(p, *now, &mut to_v, drop, heard);
        let _ = v.handle_input(Input::Timeout(*now));
        drain_v(v, &mut to_a, heard);
        for d in std::mem::take(&mut to_v) {
            if let Ok(r) = Receive::new(Protocol::Udp, addr(A), addr(B), &d) {
                let _ = v.handle_input(Input::Receive(*now, r));
                drain_v(v, &mut to_a, heard);
            }
        }
        for d in std::mem::take(&mut to_a) {
            if let Ok(r) = Receive::new(Protocol::Udp, addr(B), addr(A), &d) {
                let _ = p.rtc.handle_input(Input::Receive(*now, r));
                drain_p(p, *now, &mut to_v, drop, heard);
            }
        }
        *now += Duration::from_millis(2);
    }
}

fn keep(_: &[u8]) -> bool {
    false
}

/// The viewer's compact description, as sdpPack makes it from its own side.
fn viewer_compact(v: &mut Rtc, t: &str, setup: &str) -> Value {
    let creds = v.direct_api().local_ice_credentials();
    let f: String = v.direct_api().local_dtls_fingerprint().bytes.iter().map(|b| format!("{b:02x}")).collect();
    // A host hidden behind an mDNS name, as Chrome sends before the camera is allowed: skipped.
    json!({"t": t, "u": creds.ufrag, "p": creds.pass, "f": f, "s": setup, "c": ["7f2c1d3e-aaaa-bbbb.local 5000 h", "10.0.0.2 5000 h"]})
}

/// The viewer takes this arcade's compact description (what sdpUnpack does, on the direct API).
fn viewer_takes(v: &mut Rtc, ours: &Value, dtls_active: bool, controlling: bool) {
    let mut d = v.direct_api();
    d.set_remote_ice_credentials(IceCreds { ufrag: ours["u"].as_str().unwrap().into(), pass: ours["p"].as_str().unwrap().into() });
    d.set_remote_fingerprint(Fingerprint { hash_func: "sha-256".into(), bytes: hex::decode(ours["f"].as_str().unwrap()).unwrap() });
    d.set_ice_controlling(controlling);
    d.start_dtls(dtls_active).unwrap();
    d.start_sctp(dtls_active);
    for (label, id, ordered) in [("u", 0u16, false), ("r", 1, true), ("m", 2, true)] {
        let reliability = if ordered { Reliability::Reliable } else { Reliability::MaxRetransmits { retransmits: 0 } };
        d.create_data_channel(ChannelConfig { label: label.into(), ordered, reliability, negotiated: Some(id), protocol: String::new() });
    }
    for c in ours["c"].as_array().unwrap() {
        let f: Vec<&str> = c.as_str().unwrap().split(' ').collect();
        let a = SocketAddr::new(f[0].parse().unwrap(), f[1].parse().unwrap());
        v.add_remote_candidate(Candidate::host(a, "udp").unwrap());
    }
}

fn viewer() -> Rtc {
    let mut v = Rtc::builder().clear_codecs().enable_vp8(true).enable_opus(true, false).build(Instant::now());
    v.add_local_candidate(Candidate::host(addr(B), "udp").unwrap());
    v
}

fn our_side() -> Pair {
    let mut rtc = new_rtc();
    rtc.add_local_candidate(Candidate::host(addr(A), "udp").unwrap());
    blank(rtc, false)
}

fn connected(p: &Pair, v: &Rtc) -> bool {
    p.state == State::Up && v.is_connected()
}

// ---------------------------------------------------------------- pieces

#[test]
fn pieces_are_cut_as_media_js_cuts_them() {
    let body: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
    let ps = pieces(K_CAM, true, 7, 123_456, 640, 480, &body);
    assert_eq!(ps.len(), 3, "48000 bytes of frame a piece");
    let h = Head::parse(&ps[2]).unwrap();
    assert_eq!((h.kind, h.key, h.seq, h.t, h.part, h.parts), (K_CAM, true, 7, 123_456, 2, 3));
    assert_eq!(u16::from_be_bytes([ps[0][13], ps[0][14]]), 640);
    assert_eq!(u16::from_be_bytes([ps[0][15], ps[0][16]]), 480);
    let joined: Vec<u8> = ps.iter().flat_map(|p| p[HEAD..].to_vec()).collect();
    assert_eq!(joined, body);
    assert!(Head::parse(&[0xA2; 20]).is_none() && Head::parse(&[0xA1; 5]).is_none());
}

#[test]
fn a_vp8_key_frame_says_its_size() {
    let mut f = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a];
    f.extend_from_slice(&(320u16 | 0x4000).to_le_bytes()); // the top two bits are the scale
    f.extend_from_slice(&240u16.to_le_bytes());
    f.extend_from_slice(&[0; 20]);
    assert_eq!(vp8_size(&f), Some((320, 240)));
    f[0] |= 1; // a delta frame carries no size
    assert_eq!(vp8_size(&f), None);
}

fn key_frame(len: usize, w: u16, h: u16) -> Vec<u8> {
    let mut f = vec![0x50, 0x2a, 0x01, 0x9d, 0x01, 0x2a];
    f.extend_from_slice(&w.to_le_bytes());
    f.extend_from_slice(&h.to_le_bytes());
    f.extend((0..len - 10).map(|i| (i * 7 % 256) as u8));
    f
}

// ---------------------------------------------------------------- compact descriptions

#[test]
fn a_compact_description_unpacks_as_the_viewer_unpacks_it() {
    let mut v = viewer();
    let o = viewer_compact(&mut v, "o", "actpass");
    let sdp = unpack(&o, "actpass").unwrap();
    assert!(sdp.contains("a=mid:0\r\n") && sdp.contains("a=setup:actpass") && sdp.contains("m=application 9 UDP/DTLS/SCTP webrtc-datachannel"));
    assert!(sdp.contains(" 10.0.0.2 5000 typ host") && !sdp.contains(".local"), "an mDNS host is left out");
    assert!(SdpOffer::from_sdp_string(&sdp).is_ok());
    // What can't be trusted is refused.
    let mut bad = o.clone();
    bad["u"] = json!("x\r\na=evil");
    assert!(unpack(&bad, "actpass").is_err());
    let mut bad = o.clone();
    bad["f"] = json!("zz");
    assert!(unpack(&bad, "actpass").is_err());
    assert!(unpack(&o, "nonsense").is_err());
    let mut many = o.clone();
    many["c"] = json!((0..30).map(|i| format!("10.0.1.{i} 5000 h")).collect::<Vec<_>>());
    assert_eq!(their_candidates(&many).len(), MOST_CANDIDATES);
}

#[test]
fn viewer_offers_and_the_link_opens_with_its_channels() {
    let mut v = viewer();
    let mut p = our_side();
    let o = viewer_compact(&mut v, "o", "actpass");
    let setup = take_theirs(&mut p, &o, "actpass").unwrap();
    assert_eq!(setup, "passive", "str0m answers an actpass offer as the DTLS server");
    let ours = pack(&mut p.rtc, "a", &setup, &[Candidate::host(addr(A), "udp").unwrap()]);
    assert_eq!(ours["c"], json!(["10.0.0.1 6000 h"]));
    assert_eq!(ours["f"].as_str().unwrap().len(), 64);
    viewer_takes(&mut v, &ours, setup == "passive", true);
    let (mut now, mut heard) = (Instant::now(), Heard::default());
    for _ in 0..200 {
        run(&mut p, &mut v, &mut now, 20, &mut keep, &mut heard);
        if connected(&p, &v) && p.m_open {
            break;
        }
    }
    assert!(connected(&p, &v), "ICE and DTLS up");
    assert!(p.m_open, "the media channel 'm' is open");
    // Fast data: the viewer's 'u' and 'r' reach the window; the window's 'r' reaches the viewer.
    let (vu, vr) = (v.direct_api().channel_id_by_sctp_stream_id(0).unwrap(), v.direct_api().channel_id_by_sctp_stream_id(1).unwrap());
    v.channel(vu).unwrap().write(false, b"position").unwrap();
    v.channel(vr).unwrap().write(true, &[1, 2, 3]).unwrap();
    let r = p.chan(1).unwrap();
    p.rtc.channel(r).unwrap().write(false, b"from the window").unwrap();
    run(&mut p, &mut v, &mut now, 300, &mut keep, &mut heard);
    let fast: Vec<&Value> = heard.window.iter().map(|(_, e)| e).filter(|e| e["type"] == "fast").collect();
    assert!(fast.iter().any(|e| e["data"] == "position" && e["member"]["id"] == "guest-viewer01" && e["room"] == "room"), "{fast:?}");
    assert!(fast.iter().any(|e| e["b64"] == b64(&[1, 2, 3])), "bytes come as b64");
    assert!(heard.viewer.iter().any(|e| matches!(e, Event::ChannelData(d) if d.id == vr && d.data == b"from the window")));
}

#[test]
fn window_offers_when_its_id_is_lower() {
    let mut v = viewer();
    let mut p = our_side();
    let ours = pack(&mut p.rtc, "o", "actpass", &[Candidate::host(addr(A), "udp").unwrap()]);
    // The viewer answers as an answerer does: DTLS client (setup active), ICE controlled.
    viewer_takes(&mut v, &ours, true, false);
    let theirs = viewer_compact(&mut v, "a", "active");
    take_theirs(&mut p, &theirs, "active").unwrap();
    p.rtc.direct_api().set_ice_controlling(true);
    let (mut now, mut heard) = (Instant::now(), Heard::default());
    for _ in 0..200 {
        run(&mut p, &mut v, &mut now, 20, &mut keep, &mut heard);
        if connected(&p, &v) && p.m_open {
            break;
        }
    }
    assert!(connected(&p, &v) && p.m_open, "up with the viewer's answer read back as an offer");
}

// ---------------------------------------------------------------- RTP

/// This arcade's side and a viewer with media negotiated by whole descriptions (as on 'm').
/// `send`: this arcade sends camera and microphone; else the viewer does.
fn media_pair(send: bool, viewer_rtp_mode: bool) -> (Pair, Rtc, Vec<Mid>, Instant, Heard) {
    let mut v = Rtc::builder().clear_codecs().enable_vp8(true).enable_opus(true, false).set_rtp_mode(viewer_rtp_mode).build(Instant::now());
    v.add_local_candidate(Candidate::host(addr(B), "udp").unwrap());
    let mut p = our_side();
    let mids;
    if send {
        let mut api = p.rtc.sdp_api();
        let cam = api.add_media(MediaKind::Video, Direction::SendOnly, Some("arcade-s".into()), Some("cam".into()), None);
        let mic = api.add_media(MediaKind::Audio, Direction::SendOnly, Some("arcade-s".into()), Some("mic".into()), None);
        let (offer, pending) = api.apply().unwrap();
        let answer = v.sdp_api().accept_offer(offer).unwrap();
        p.rtc.sdp_api().accept_answer(pending, answer).unwrap();
        for (k, mid) in [(K_CAM, cam), (K_MIC, mic)] {
            p.tx.insert(k, Tx { mid: Some(mid), active: true, want: true, need_key: k == K_CAM });
        }
        mids = vec![cam, mic];
    } else {
        let mut api = v.sdp_api();
        let cam = api.add_media(MediaKind::Video, Direction::SendOnly, Some("viewer-s".into()), Some("cam".into()), None);
        let mic = api.add_media(MediaKind::Audio, Direction::SendOnly, Some("viewer-s".into()), Some("mic".into()), None);
        let (offer, pending) = api.apply().unwrap();
        let answer = p.rtc.sdp_api().accept_offer(offer).unwrap();
        v.sdp_api().accept_answer(pending, answer).unwrap();
        mids = vec![cam, mic];
    }
    p.rtc.add_remote_candidate(Candidate::host(addr(B), "udp").unwrap());
    v.add_remote_candidate(Candidate::host(addr(A), "udp").unwrap());
    let (mut now, mut heard) = (Instant::now(), Heard::default());
    for _ in 0..200 {
        run(&mut p, &mut v, &mut now, 20, &mut keep, &mut heard);
        if connected(&p, &v) {
            break;
        }
    }
    assert!(connected(&p, &v));
    (p, v, mids, now, heard)
}

fn feed(p: &mut Pair, now: Instant, kind: u8, key: bool, seq: u32, t: u32, frame: &[u8], out: &mut Vec<(String, Value)>) {
    for piece in pieces(kind, key, seq, t, 320, 240, frame) {
        p.piece_in(now, &piece, out);
    }
}

#[test]
fn window_frames_go_out_as_vp8_and_opus_rtp() {
    let (mut p, mut v, _, mut now, mut heard) = media_pair(true, true);
    let mut out = Vec::new();
    // A delta before any key frame is held back, and the window is asked for a key frame.
    feed(&mut p, now, K_CAM, false, 0, 1000, &[0x31; 900], &mut out);
    assert!(out.iter().any(|(_, e)| e["type"] == "mchunk" && Head::parse(&unb64(e)).is_some_and(|h| h.kind == K_NOTE)), "kf asked");
    let key = key_frame(3000, 320, 240);
    feed(&mut p, now, K_CAM, true, 1, 1050, &key, &mut out);
    feed(&mut p, now, K_CAM, false, 2, 1100, &[0x31; 900], &mut out);
    let opus: Vec<Vec<u8>> = (0..3).map(|i| vec![0xfc, i as u8, 0x11, 0x22, 0x33]).collect();
    for (i, o) in opus.iter().enumerate() {
        // The width field of a sound piece: 1 + its level in -dBov (media.js levelWord), here -20 dBov.
        for piece in pieces(K_MIC, true, i as u32, 2000 + 20 * i as u32, 21, 0, o) {
            p.piece_in(now, &piece, &mut out);
        }
    }
    run(&mut p, &mut v, &mut now, 500, &mut keep, &mut heard);
    let rtp: Vec<&RtpPacket> = heard.viewer.iter().filter_map(|e| if let Event::RtpPacket(r) = e { Some(r) } else { None }).collect();
    let pt_of = |codec: Codec| v.codec_config().params().iter().find(|p| p.spec().codec == codec).unwrap().pt();
    let (vp8, op) = (pt_of(Codec::Vp8), pt_of(Codec::Opus));
    let video: Vec<&&RtpPacket> = rtp.iter().filter(|r| r.header.payload_type == vp8).collect();
    let audio: Vec<&&RtpPacket> = rtp.iter().filter(|r| r.header.payload_type == op).collect();
    // The key frame: several packets, the first with the descriptor's S bit (start of partition 0),
    // the rest without, the marker on its last; put back together it is the frame.
    let first_ts = video[0].header.timestamp;
    let frame: Vec<&&&RtpPacket> = video.iter().filter(|r| r.header.timestamp == first_ts).collect();
    assert!(frame.len() >= 3, "3000 bytes take more than one packet ({})", frame.len());
    let desc = |r: &RtpPacket| -> usize {
        // RFC 7741 4.2: X, R, N, S, R, PID; X adds a byte and its I/L/T/K fields.
        let b = r.payload[0];
        if b & 0x80 == 0 {
            return 1;
        }
        let x = r.payload[1];
        let mut n = 2;
        if x & 0x80 != 0 {
            n += if r.payload[n] & 0x80 != 0 { 2 } else { 1 };
        }
        if x & 0x40 != 0 {
            n += 1;
        }
        if x & 0x30 != 0 {
            n += 1;
        }
        n
    };
    assert!(frame[0].payload[0] & 0x10 != 0 && frame[0].payload[0] & 0x07 == 0, "S set, partition 0");
    assert!(frame[1..].iter().all(|r| r.payload[0] & 0x10 == 0), "S only on the first packet");
    assert!(frame.last().unwrap().header.marker && !frame[0].header.marker);
    let joined: Vec<u8> = frame.iter().flat_map(|r| r.payload[desc(r)..].to_vec()).collect();
    assert_eq!(joined, key, "the key frame comes out whole");
    let next_ts = video.iter().map(|r| r.header.timestamp).find(|t| *t != first_ts).unwrap();
    assert_eq!(next_ts.wrapping_sub(first_ts), 50 * 90, "50 ms apart on the 90 kHz clock");
    // Opus: one packet a frame, the payload is the frame itself, 20 ms (960) apart.
    assert_eq!(audio.len(), 3);
    for (r, o) in audio.iter().zip(&opus) {
        assert_eq!(&r.payload[..], &o[..]);
    }
    assert_eq!(audio[1].header.timestamp.wrapping_sub(audio[0].header.timestamp), 960);
    assert_eq!(audio[0].header.ext_vals.audio_level, Some(-20), "the voice's level rides the audio-level extension");
    assert_eq!(audio[0].header.ext_vals.voice_activity, Some(true));
}

fn unb64(e: &Value) -> Vec<u8> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.decode(e["b64"].as_str().unwrap()).unwrap()
}

/// Pieces heard by the window, put back together: (kind, key, w, h, frame).
fn frames_heard(window: &[(String, Value)]) -> Vec<(u8, bool, u16, u16, Vec<u8>)> {
    let mut out = Vec::new();
    let mut parts: HashMap<(u8, u32), Vec<Vec<u8>>> = HashMap::new();
    for (_, e) in window.iter().filter(|(_, e)| e["type"] == "mchunk") {
        let u = unb64(e);
        let h = Head::parse(&u).unwrap();
        let got = parts.entry((h.kind, h.seq)).or_default();
        got.push(u[HEAD..].to_vec());
        if got.len() == h.parts as usize {
            let (w, hh) = (u16::from_be_bytes([u[13], u[14]]), u16::from_be_bytes([u[15], u[16]]));
            out.push((h.kind, h.key, w, hh, got.concat()));
        }
    }
    out
}

#[test]
fn viewer_rtp_comes_to_the_window_as_whole_frames() {
    let (mut p, mut v, mids, mut now, mut heard) = media_pair(false, false);
    let (cam, mic) = (mids[0], mids[1]);
    let key = key_frame(60_000, 640, 360); // more than one piece, too
    let delta = vec![0x31; 1200];
    let pt = |v: &mut Rtc, mid: Mid, c: Codec| v.writer(mid).unwrap().payload_params().find(|p| p.spec().codec == c).unwrap().pt();
    let (vp8, op) = (pt(&mut v, cam, Codec::Vp8), pt(&mut v, mic, Codec::Opus));
    v.writer(cam).unwrap().write(vp8, now, MediaTime::from_90khz(90_000), key.clone()).unwrap();
    run(&mut p, &mut v, &mut now, 100, &mut keep, &mut heard);
    v.writer(cam).unwrap().write(vp8, now, MediaTime::from_90khz(90_000 + 4500), delta.clone()).unwrap();
    v.writer(mic).unwrap().write(op, now, MediaTime::new(48_000, Frequency::FORTY_EIGHT_KHZ), vec![0xfc, 9, 9]).unwrap();
    run(&mut p, &mut v, &mut now, 300, &mut keep, &mut heard);
    let got = frames_heard(&heard.window);
    let video: Vec<_> = got.iter().filter(|f| f.0 == K_CAM).collect();
    assert_eq!(video.len(), 2, "{:?}", got.iter().map(|f| (f.0, f.1, f.4.len())).collect::<Vec<_>>());
    assert_eq!(video[0], &(K_CAM, true, 640, 360, key), "the key frame, whole, with its size");
    assert_eq!(video[1], &(K_CAM, false, 640, 360, delta.clone()), "the delta keeps the size the decoder was set up with");
    assert!(got.iter().any(|f| f.0 == K_MIC && f.1 && f.4 == vec![0xfc, 9, 9]), "sound as microphone pieces");
    // Their second picture, once they named its stream: screen pieces.
    p.xid = "viewer-s".into();
    v.writer(cam).unwrap().write(vp8, now, MediaTime::from_90khz(90_000 + 9000), delta.clone()).unwrap();
    run(&mut p, &mut v, &mut now, 200, &mut keep, &mut heard);
    assert!(frames_heard(&heard.window).iter().any(|f| f.0 == K_SCR), "kind 3 for the stream named by x");
}

#[test]
fn key_frame_requests_cross_both_ways() {
    let (mut p, mut v, mids, mut now, mut heard) = media_pair(true, false);
    let mut out = Vec::new();
    feed(&mut p, now, K_CAM, true, 1, 1000, &key_frame(2000, 320, 240), &mut out);
    run(&mut p, &mut v, &mut now, 200, &mut keep, &mut heard);
    // The viewer's PLI: the window hears its own key-frame note from the viewer.
    let before = heard.window.len();
    v.writer(mids[0]).unwrap().request_keyframe(None, KeyframeRequestKind::Pli).unwrap();
    run(&mut p, &mut v, &mut now, 300, &mut keep, &mut heard);
    let notes: Vec<Vec<u8>> = heard.window[before..].iter().filter(|(_, e)| e["type"] == "mchunk").map(|(_, e)| unb64(e)).filter(|u| u[1] == K_NOTE).collect();
    assert!(notes.iter().any(|u| &u[HEAD..] == br#"{"kf":1}"#), "PLI -> the window's kf note");

    // The other way: a window's kf note, for a viewer that sends, becomes a PLI to it.
    let (mut p, mut v, mids, mut now, mut heard) = media_pair(false, false);
    let cam = mids[0];
    let pt = v.writer(cam).unwrap().payload_params().find(|x| x.spec().codec == Codec::Vp8).unwrap().pt();
    v.writer(cam).unwrap().write(pt, now, MediaTime::from_90khz(1000), key_frame(1500, 320, 240)).unwrap();
    run(&mut p, &mut v, &mut now, 200, &mut keep, &mut heard);
    assert!(p.rx.contains_key(&cam), "the viewer's camera is known");
    let mut out = Vec::new();
    p.piece_in(now, &pieces(K_NOTE, false, 0, 0, 0, 0, br#"{"kf":1}"#)[0], &mut out);
    run(&mut p, &mut v, &mut now, 300, &mut keep, &mut heard);
    assert!(heard.viewer.iter().any(|e| matches!(e, Event::KeyframeRequest(_))), "the window's note -> a PLI to the viewer");
}

#[test]
fn a_lost_packet_is_resent_when_the_viewer_asks() {
    let (mut p, mut v, _, mut now, mut heard) = media_pair(true, false);
    let mut out = Vec::new();
    let key = key_frame(5000, 320, 240);
    feed(&mut p, now, K_CAM, true, 1, 1000, &key, &mut out);
    // Lose the second big packet on its way (one of the key frame's).
    let mut big = 0;
    let mut lose = |d: &[u8]| {
        if d.len() > 600 {
            big += 1;
            return big == 2;
        }
        false
    };
    run(&mut p, &mut v, &mut now, 1500, &mut lose, &mut heard);
    assert!(big >= 5, "the frame was sent ({big} big packets)");
    let whole: Vec<&MediaData> = heard.viewer.iter().filter_map(|e| if let Event::MediaData(d) = e { Some(d) } else { None }).collect();
    assert!(whole.iter().any(|d| d.data.as_ref() == key.as_slice()), "NACKed and resent: the viewer has the whole key frame");
}

#[test]
fn h264_only_video_is_refused_and_sound_still_comes() {
    let mut v = Rtc::builder().clear_codecs().enable_h264(true).enable_opus(true, false).build(Instant::now());
    let mut p = our_side();
    let mut api = v.sdp_api();
    api.add_media(MediaKind::Video, Direction::SendOnly, Some("s".into()), Some("cam".into()), None);
    api.add_media(MediaKind::Audio, Direction::SendOnly, Some("s".into()), Some("mic".into()), None);
    let (offer, _) = api.apply().unwrap();
    let answer = p.rtc.sdp_api().accept_offer(offer).unwrap().to_sdp_string();
    p.refused_video(&answer);
    assert!(p.refused.iter().any(|r| r.contains("no VP8")), "{answer}");
    assert!(answer.lines().any(|l| l.starts_with("m=audio ") && !l.starts_with("m=audio 0 ")), "sound answered");
}

#[test]
fn renegotiation_on_m_sets_the_window_media_up() {
    // Our side offers its camera over 'm' when the window starts sending; the viewer (str0m,
    // SDP) answers; once accepted the frames flow.
    let mut v = viewer();
    let mut p = our_side();
    let o = viewer_compact(&mut v, "o", "actpass");
    let setup = take_theirs(&mut p, &o, "actpass").unwrap();
    let ours = pack(&mut p.rtc, "a", &setup, &[Candidate::host(addr(A), "udp").unwrap()]);
    viewer_takes(&mut v, &ours, setup == "passive", true);
    let (mut now, mut heard) = (Instant::now(), Heard::default());
    for _ in 0..200 {
        run(&mut p, &mut v, &mut now, 20, &mut keep, &mut heard);
        if connected(&p, &v) && p.m_open {
            break;
        }
    }
    assert!(p.m_open);
    let mut out = Vec::new();
    feed(&mut p, now, K_CAM, true, 0, 1000, &key_frame(1000, 320, 240), &mut out);
    assert!(p.offer_at.is_some(), "an offer is due");
    run(&mut p, &mut v, &mut now, 300, &mut keep, &mut heard);
    assert!(p.pending.is_some(), "our offer is out");
    let vm = v.direct_api().channel_id_by_sctp_stream_id(2).unwrap();
    let said: Vec<Value> = heard.viewer.iter().filter_map(|e| match e {
        Event::ChannelData(d) if d.id == vm => serde_json::from_slice(&d.data).ok(),
        _ => None,
    }).collect();
    let offer = said.iter().find(|m| m["d"]["type"] == "offer").expect("an offer on 'm'");
    let sdp = offer["d"]["sdp"].as_str().unwrap();
    assert!(sdp.contains("m=video ") && sdp.contains("VP8/90000") && sdp.contains("a=sendonly"), "{sdp}");
    assert!(sdp.contains("a=mid:0\r\n"), "the data section keeps the viewer's mid 0");
    assert!(sdp.contains(&format!("a=msid:{} ", p.stream)), "the camera's stream id");
    // A collision: the viewer's offer while ours is out; we are not the polite one (polite=false):
    // theirs is ignored and ours stands.
    let theirs = json!({"d": {"type": "offer", "sdp": sdp}}).to_string();
    p.on_m(now, &theirs, &mut out);
    assert!(p.pending.is_some());
    // "yours": they could not take ours: ours is undone, and we wait.
    p.on_m(now, r#"{"yours":1}"#, &mut out);
    assert!(p.pending.is_none() && p.wait_until.is_some());
}


#[test]
fn the_viewers_own_sections_are_answered_receive_only() {
    let offer = "v=0\r\na=group:BUNDLE 0 1 abc\r\nm=application 9 UDP/DTLS/SCTP webrtc-datachannel\r\na=mid:0\r\n\
                 m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=mid:1\r\na=sendrecv\r\n\
                 m=video 9 UDP/TLS/RTP/SAVPF 96\r\na=sendrecv\r\na=mid:abc\r\n";
    let ours: HashSet<String> = ["abc".to_string()].into();
    let got = theirs_recv_only(offer, &ours);
    assert!(got.contains("a=mid:1\r\na=sendonly\r\n"), "{got}");
    assert!(got.contains("a=sendrecv\r\na=mid:abc"), "ours keeps both ways: {got}");
    assert!(got.ends_with("\r\n") && got.matches("\r\n").count() == offer.matches("\r\n").count());
}
