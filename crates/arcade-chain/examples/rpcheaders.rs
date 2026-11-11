//! Walk a range of real headers from a coin node's JSON-RPC through our header rules (link,
//! checkpoints, difficulty, time, and which ones have their work checked) and print the first
//! header that fails. To check a rule against a chain before turning it on.
//!
//!   ARCADE_RPC_URL=http://127.0.0.1:22555 ARCADE_RPC_AUTH=user:pass \
//!     cargo run --release -p arcade-chain --example rpcheaders -- <chain key> <from> <to>
//!
//! ARCADE_RPC_COOKIE=<path to .cookie> instead of ARCADE_RPC_AUTH. ARCADE_DIGISHIELD_FROM=<height>
//! checks the Digishield bits from that height even where the chain's params have it off.
//! The time rule against the clock uses the node's own time of each header (no header is in
//! the future of a node that accepted it); the median rule starts 11 blocks after `from`.
use arcade_chain::params::{by_key, check_context, check_header, ChainParams, PowCheck, MEDIAN_TIME_SPAN};
use arcade_chain::{Hash256, Header};
use std::io::{Read, Write};
use std::net::TcpStream;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 4 {
        eprintln!("usage: rpcheaders <chain key> <from> <to>   (env ARCADE_RPC_URL, ARCADE_RPC_AUTH or ARCADE_RPC_COOKIE)");
        std::process::exit(2);
    }
    let base: &ChainParams = by_key(&args[1]).expect("chain key");
    let mut p = base.clone();
    if let Some(h) = std::env::var("ARCADE_DIGISHIELD_FROM").ok().and_then(|v| v.parse().ok()) {
        p.digishield_from = Some(h);
    }
    let from: u32 = args[2].parse().expect("from height");
    let to: u32 = args[3].parse().expect("to height");
    let rpc = Rpc::from_env();

    // The 11 headers below `from` (or as many as there are) start the window.
    let start = from.saturating_sub(MEDIAN_TIME_SPAN as u32);
    let mut below: Vec<Header> = Vec::new(); // parent first
    let mut counts = [0u64; 4];
    let mut h = start;
    let mut note = start + 10_000;
    while h <= to {
        let end = (h + 499).min(to);
        let headers = rpc.headers(&p, h, end);
        for (i, hdr) in headers.into_iter().enumerate() {
            let height = h + i as u32;
            if height > start {
                let parent = below[0];
                if height >= from {
                    match check_header(&p, &hdr, &parent, height) {
                        Ok(c) => counts[c as usize] += 1,
                        Err(e) => fail(&p, height, &hdr, &e.to_string()),
                    }
                    // Our clock would be at least the header's time when it arrived.
                    if let Err(e) = check_context(&p, &hdr, height, &below, hdr.time as u64) {
                        fail(&p, height, &hdr, &e.to_string());
                    }
                } else if hdr.prev != p.block_id(&parent) {
                    fail(&p, height, &hdr, "does not link to the header below");
                }
            }
            below.insert(0, hdr);
            below.truncate(MEDIAN_TIME_SPAN);
        }
        h = end + 1;
        if end >= note {
            eprintln!("  .. {end}");
            note += 10_000;
        }
    }
    println!(
        "{} {from}..={to}: all pass · work checked {} · AuxPoW (parent not checked here) {} · not checkable {} · needs its block {}",
        p.name, counts[PowCheck::Verified as usize], counts[PowCheck::AuxPow as usize], counts[PowCheck::NotCheckable as usize], counts[PowCheck::NeedsBlock as usize]
    );
}

fn fail(p: &ChainParams, height: u32, hdr: &Header, why: &str) -> ! {
    println!("{} height {height} ({}): {why}", p.name, p.block_id(hdr));
    println!("  header {hdr:?}");
    std::process::exit(1);
}

struct Rpc {
    host: String,
    path: String,
    auth: String,
}

impl Rpc {
    fn from_env() -> Rpc {
        let url = std::env::var("ARCADE_RPC_URL").expect("ARCADE_RPC_URL=http://host:port");
        let rest = url.strip_prefix("http://").expect("only http:// URLs");
        let (host, path) = match rest.find('/') {
            Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
            None => (rest.to_string(), "/".to_string()),
        };
        let cred = match std::env::var("ARCADE_RPC_AUTH") {
            Ok(a) => a,
            Err(_) => {
                let f = std::env::var("ARCADE_RPC_COOKIE").expect("ARCADE_RPC_AUTH=user:pass or ARCADE_RPC_COOKIE=path");
                std::fs::read_to_string(f).expect("read cookie").trim().to_string()
            }
        };
        Rpc { host, path, auth: base64(cred.as_bytes()) }
    }

    /// One JSON-RPC batch; the string results, in order.
    fn batch(&self, calls: &[(String, String)]) -> Vec<String> {
        let body: Vec<String> = calls.iter().enumerate().map(|(i, (m, params))| format!(r#"{{"jsonrpc":"1.0","id":{i},"method":"{m}","params":{params}}}"#)).collect();
        let body = format!("[{}]", body.join(","));
        let mut s = TcpStream::connect(&self.host).expect("connect to the node");
        write!(
            s,
            "POST {} HTTP/1.1\r\nHost: {}\r\nAuthorization: Basic {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            self.path, self.host, self.auth, body.len(), body
        )
        .expect("send");
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).expect("read answer");
        let text = String::from_utf8_lossy(&raw).to_string();
        let (head, mut body) = text.split_once("\r\n\r\n").map(|(a, b)| (a.to_string(), b.to_string())).expect("http answer");
        if !head.starts_with("HTTP/1.1 200") && !head.starts_with("HTTP/1.0 200") {
            panic!("node answered: {}", head.lines().next().unwrap_or(""));
        }
        if head.to_ascii_lowercase().contains("transfer-encoding: chunked") {
            body = unchunk(&body);
        }
        // Every reply object is {"result":"<hex>","error":null,"id":N}; take them by id.
        let mut out = vec![String::new(); calls.len()];
        for part in body.split(r#""result":"#).skip(1) {
            let Some(v) = part.strip_prefix('"').and_then(|x| x.split('"').next()) else { panic!("no result in: {}", &part[..part.len().min(200)]) };
            let id: usize = part.split(r#""id":"#).nth(1).and_then(|x| x.trim_start().split(|c: char| !c.is_ascii_digit()).next()).and_then(|x| x.parse().ok()).expect("reply id");
            out[id] = v.to_string();
        }
        assert!(out.iter().all(|x| !x.is_empty()), "the node left out an answer: {}", &body[..body.len().min(300)]);
        out
    }

    /// Headers at heights from..=to, in order (the first 80 bytes of each serialized header;
    /// merge-mined chains append the AuxPoW).
    fn headers(&self, p: &ChainParams, from: u32, to: u32) -> Vec<Header> {
        let hashes = self.batch(&(from..=to).map(|h| ("getblockhash".to_string(), format!("[{h}]"))).collect::<Vec<_>>());
        let raws = self.batch(&hashes.iter().map(|h| ("getblockheader".to_string(), format!(r#"["{h}",false]"#))).collect::<Vec<_>>());
        raws.iter()
            .zip(&hashes)
            .map(|(r, want)| {
                let b = hex::decode(r).expect("header hex");
                let hdr = Header::parse(b.get(..80).expect("80 bytes").try_into().unwrap());
                assert_eq!(p.block_id(&hdr), Hash256::from_hex(want).expect("hash hex"), "our block id differs from the node's");
                hdr
            })
            .collect()
    }
}

fn unchunk(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some((len, after)) = rest.split_once("\r\n") {
        let n = usize::from_str_radix(len.trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.push_str(&after[..n]);
        rest = after[n..].trim_start_matches("\r\n");
    }
    out
}

fn base64(b: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut o = String::new();
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                o.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                o.push('=');
            }
        }
    }
    o
}
