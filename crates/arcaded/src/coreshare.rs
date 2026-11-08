//! Serving our chain to existing arcades that borrow it (t:"cr", mesh/coreshare.py).
//!
//!   offer {nets:{net:{h, b, l}}}                  sent on link-up
//!   q {id, net, seq, end, z} + slice              a batch [[method, params], ...] (zlib when z)
//!   a {id, seq, end[, err][, z]} + slice          answers [{"r": result} | {"e": [code, msg]}]
//!   tx {net} + raw tx                              relay a transaction
//! Limits as the existing arcade: CHUNK 48 KB, MAX_CALLS 500, MAX_ASKED 8 MB.

use crate::node::Node;
use arcade_mesh::link::{Frame, Link, LinkError};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};

pub const CHUNK: usize = 48 * 1024;
pub const MAX_CALLS: usize = 500;
pub const MAX_ASKED: usize = 8 << 20;

pub fn offer(node: &Node) -> Option<Frame> {
    let tip = node.chain.tip(node.params.chain_no).ok()??;
    Some(Frame::new(json!({
        "t": "cr", "op": "offer",
        "nets": { node.params.net: {"h": tip.height, "b": tip.hash.to_hex(), "l": 1} }
    })))
}

#[derive(Default)]
pub struct Requests {
    parts: HashMap<u64, Vec<u8>>,
}

fn inflate(b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    ZlibDecoder::new(b).take(MAX_ASKED as u64 + 1).read_to_end(&mut out).map_err(|e| e.to_string())?;
    if out.len() > MAX_ASKED {
        return Err("request too large".into());
    }
    Ok(out)
}

fn deflate(b: &[u8]) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

/// Answer the calls in a batch.
pub fn answer(node: &Node, calls: &[Value]) -> Vec<Value> {
    calls
        .iter()
        .map(|c| {
            let method = c.get(0).and_then(Value::as_str).unwrap_or("");
            let params: Vec<Value> = c.get(1).and_then(Value::as_array).cloned().unwrap_or_default();
            match crate::rpc::call(node, method, &params) {
                Ok(r) => json!({"r": r}),
                Err((code, m)) => json!({"e": [code, m]}),
            }
        })
        .collect()
}

pub fn handle(node: &Node, link: &mut Link, reqs: &mut Requests, f: &Frame) -> Result<(), LinkError> {
    let h = &f.head;
    match h["op"].as_str().unwrap_or("") {
        "q" if h["net"] == node.params.net => {
            let id = h["id"].as_u64().unwrap_or(0);
            let buf = reqs.parts.entry(id).or_default();
            buf.extend_from_slice(&f.blob);
            if buf.len() > MAX_ASKED {
                reqs.parts.remove(&id);
                return send(link, id, Err("request too large".into()), true);
            }
            if h["end"] != true {
                return Ok(());
            }
            let raw = reqs.parts.remove(&id).unwrap_or_default();
            let z = h["z"] == 1 || h["z"] == true;
            let body = if z { inflate(&raw) } else { Ok(raw) };
            let res = body.and_then(|b| {
                let calls: Vec<Value> = serde_json::from_slice(&b).map_err(|e| e.to_string())?;
                if calls.len() > MAX_CALLS {
                    return Err(format!("at most {MAX_CALLS} calls"));
                }
                Ok(serde_json::to_vec(&answer(node, &calls)).unwrap())
            });
            send(link, id, res, z)
        }
        "tx" if h["net"] == node.params.net => {
            if let Ok(tx) = arcade_chain::Tx::parse_with(&f.blob, &node.params.format) {
                let _ = node.chain.mempool_add(node.params, &tx, arcade_app::now());
                node.tx_outbox.lock().unwrap().push(f.blob.clone());
            }
            Ok(())
        }
        _ => Ok(()), // offers and subs from others: not needed by a node that has its own chain
    }
}

fn send(link: &mut Link, id: u64, res: Result<Vec<u8>, String>, z: bool) -> Result<(), LinkError> {
    match res {
        Err(e) => link.send(&Frame::new(json!({"t": "cr", "op": "a", "id": id, "seq": 0, "end": true, "err": e}))),
        Ok(body) => {
            let data = if z { deflate(&body) } else { body };
            let chunks: Vec<&[u8]> = if data.is_empty() { vec![&[][..]] } else { data.chunks(CHUNK).collect() };
            let n = chunks.len();
            for (i, c) in chunks.into_iter().enumerate() {
                let mut head = json!({"t": "cr", "op": "a", "id": id, "seq": i, "end": i + 1 == n});
                if z {
                    head["z"] = json!(1);
                }
                link.send(&Frame { head, blob: c.to_vec() })?;
            }
            Ok(())
        }
    }
}
