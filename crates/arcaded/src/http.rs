//! Local HTTP server for inscription content (127.0.0.1 only).
//!   GET /content/<txid|number>       the inscription's bytes, rebuilt from the chain
//!   GET /inscription/<txid|number>   its details (JSON)
//! Content is from strangers: served with a sandbox CSP and nosniff so it cannot reach the
//! arcade's API or the wallet.

use crate::node::Node;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

pub fn serve(node: Arc<Node>, port: u16) {
    let l = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) => {
            let _ = node.app.event("http", &format!("cannot serve content on 127.0.0.1:{port}: {e}"));
            return;
        }
    };
    for s in l.incoming().flatten() {
        let node = node.clone();
        std::thread::spawn(move || handle(&node, s));
    }
}

fn respond(s: &mut TcpStream, status: &str, ctype: &str, extra: &[(&str, String)], body: &[u8]) {
    let mut h = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n", body.len());
    for (k, v) in extra {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    h.push_str("\r\n");
    let _ = s.write_all(h.as_bytes());
    let _ = s.write_all(body);
}

fn handle(node: &Node, mut s: TcpStream) {
    let mut line = String::new();
    if BufReader::new(&s).read_line(&mut line).is_err() {
        return;
    }
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if method != "GET" && method != "HEAD" {
        return respond(&mut s, "405 Method Not Allowed", "text/plain", &[], b"GET only\n");
    }
    let p = node.params;
    let seg: Vec<&str> = path.trim_start_matches('/').splitn(2, '/').collect();
    match seg.as_slice() {
        ["content", id] => match arcade_ledger::content::content(&node.chain, p, id) {
            Ok(c) => respond(
                &mut s,
                "200 OK",
                if c.content_type.is_empty() { "application/octet-stream" } else { &c.content_type },
                &[
                    ("ETag", format!("\"{}\"", c.sha256)),
                    ("Cache-Control", "public, max-age=31536000, immutable".into()),
                    ("Content-Security-Policy", "sandbox allow-scripts; default-src 'self' data: blob:; connect-src 'none'".into()),
                ],
                if method == "HEAD" { &[] } else { &c.bytes },
            ),
            Err(e) => respond(&mut s, "404 Not Found", "text/plain", &[], format!("{e}\n").as_bytes()),
        },
        ["inscription", id] => match arcade_ledger::content::inscription(&node.chain, p.chain_no, id) {
            Some(mut v) => {
                v["content"] = serde_json::json!(format!("/content/{}", v["txid"].as_str().unwrap_or("")));
                respond(&mut s, "200 OK", "application/json", &[], v.to_string().as_bytes())
            }
            None => respond(&mut s, "404 Not Found", "text/plain", &[], b"no such inscription\n"),
        },
        _ => respond(&mut s, "404 Not Found", "text/plain", &[], b"try /content/<txid|number> or /inscription/<txid|number>\n"),
    }
}
