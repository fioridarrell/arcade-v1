//! An existing arcade's Core node as a block source, borrowed over the mesh (coreshare "cr"
//! q/a batches of JSON-RPC calls, as mesh/coreshare.py). Headers come from the blocks
//! themselves (so AuxPoW and PoS headers carry what the checks need); each batch of blocks is
//! kept until the syncer asks for it. Everything is checked exactly as P2P data.

use arcade_chain::p2p::HeaderEntry;
use arcade_chain::params::ChainParams;
use arcade_chain::{Block, Hash256};
use arcade_chaindb::sync::BlockSource;
use arcade_chaindb::ChainDb;
use arcade_mesh::link::{Frame, Link};
use flate2::{read::ZlibDecoder, write::ZlibEncoder, Compression};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::time::Duration;

pub struct CoreShareSource<'a> {
    link: Link,
    addr: String,
    db: &'a ChainDb,
    p: &'static ChainParams,
    next_id: u64,
    cache: HashMap<Hash256, Vec<u8>>,
    /// Blocks per batch (smaller for chains with big blocks).
    batch: usize,
}

impl<'a> CoreShareSource<'a> {
    pub fn dial(node: &'a crate::node::Node, addr: &str, p: &'static ChainParams) -> Result<Self, String> {
        let mut link = Link::dial(addr, crate::meshsvc::NET, &node.identity, node.mesh_port).map_err(|e| e.to_string())?;
        link.set_read_timeout(Duration::from_secs(90)).map_err(|e| e.to_string())?;
        let batch = match p.key { "doge" | "ltc" | "btc" | "dgb" => 20, _ => 100 };
        Ok(CoreShareSource { link, addr: addr.into(), db: &node.chain, p, next_id: 1, cache: HashMap::new(), batch })
    }

    fn call(&mut self, calls: Value) -> Result<Vec<Value>, String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut e = ZlibEncoder::new(Vec::new(), Compression::new(6));
        e.write_all(calls.to_string().as_bytes()).map_err(|e| e.to_string())?;
        self.link
            .send(&Frame { head: json!({"t": "cr", "op": "q", "id": id, "net": self.p.key, "seq": 0, "end": true, "z": 1}), blob: e.finish().map_err(|e| e.to_string())? })
            .map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        #[allow(unused_assignments)]
        let mut z = true;
        loop {
            let f = self.link.recv().map_err(|e| e.to_string())?;
            match f.kind() {
                "ping" => {
                    let n = f.head["n"].clone();
                    self.link.send(&Frame::new(json!({"t": "pong", "n": n}))).map_err(|e| e.to_string())?;
                }
                "cr" if f.head["op"] == "a" && f.head["id"] == id => {
                    if let Some(err) = f.head["err"].as_str() {
                        return Err(format!("arcade {}: {err}", self.addr));
                    }
                    z = f.head["z"] == 1 || f.head["z"] == true || f.head["z"].is_null();
                    buf.extend_from_slice(&f.blob);
                    if buf.len() > 256 << 20 {
                        return Err("answer too large".into());
                    }
                    if f.head["end"] == true {
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = if z {
            let mut out = Vec::new();
            match ZlibDecoder::new(&buf[..]).read_to_end(&mut out) {
                Ok(_) => out,
                Err(_) => buf, // answered without compression
            }
        } else {
            buf
        };
        serde_json::from_slice(&body).map_err(|e| format!("bad answer: {e}"))
    }

    fn r<'v>(v: &'v Value) -> Result<&'v Value, String> {
        if let Some(e) = v.get("e") {
            return Err(format!("rpc error {e}"));
        }
        v.get("r").ok_or_else(|| "no result".to_string())
    }

    /// Their tip height.
    pub fn height(&mut self) -> Result<u64, String> {
        let a = self.call(json!([["getblockcount", []]]))?;
        Self::r(&a[0])?.as_u64().ok_or_else(|| "bad block count".into())
    }

    fn fetch(&mut self, hashes: &[String]) -> Result<Vec<Vec<u8>>, String> {
        let calls: Vec<Value> = hashes.iter().map(|h| json!(["getblock", [h, 0]])).collect();
        let a = self.call(json!(calls))?;
        a.iter().map(|x| hex::decode(Self::r(x)?.as_str().ok_or("bad block")?).map_err(|e| e.to_string())).collect()
    }
}

impl BlockSource for CoreShareSource<'_> {
    fn get_headers(&mut self, locator: &[Hash256]) -> Result<Vec<HeaderEntry>, String> {
        let c = self.p.chain_no;
        // Where our chain and theirs agree: ask their hash at each locator height.
        let heights: Vec<(Hash256, u32)> = locator.iter().filter_map(|h| self.db.header(c, h).ok().flatten().map(|r| (*h, r.height))).collect();
        let start = if heights.is_empty() {
            0
        } else {
            let calls: Vec<Value> = heights.iter().map(|(_, ht)| json!(["getblockhash", [ht]])).collect();
            let a = self.call(json!(calls))?;
            let mut s = None;
            for ((h, ht), x) in heights.iter().zip(a.iter()) {
                if Self::r(x).ok().and_then(|v| v.as_str()) == Some(h.to_hex().as_str()) {
                    s = Some(ht + 1);
                    break;
                }
            }
            s.ok_or("this arcade's chain shares no recent block with ours")?
        };
        let top = self.height()?;
        if start as u64 > top {
            return Ok(vec![]);
        }
        let n = ((top - start as u64 + 1) as usize).min(self.batch);
        let calls: Vec<Value> = (start..start + n as u32).map(|h| json!(["getblockhash", [h]])).collect();
        let a = self.call(json!(calls))?;
        let hashes: Vec<String> = a.iter().map(|x| Self::r(x).and_then(|v| v.as_str().map(String::from).ok_or_else(|| "bad hash".to_string()))).collect::<Result<_, _>>()?;
        let raws = self.fetch(&hashes)?;
        let mut out = Vec::with_capacity(raws.len());
        for raw in raws {
            let b = Block::parse_with(&raw, &self.p.format).map_err(|e| e.to_string())?;
            out.push(HeaderEntry { header: b.header, auxpow: b.auxpow.clone() });
            self.cache.insert(b.id, raw);
        }
        Ok(out)
    }

    fn get_blocks(&mut self, hashes: &[Hash256]) -> Result<Vec<Vec<u8>>, String> {
        let missing: Vec<String> = hashes.iter().filter(|h| !self.cache.contains_key(h)).map(|h| h.to_hex()).collect();
        for chunk in missing.chunks(self.batch) {
            for raw in self.fetch(chunk)? {
                let b = Block::parse_with(&raw, &self.p.format).map_err(|e| e.to_string())?;
                self.cache.insert(b.id, raw);
            }
        }
        hashes.iter().map(|h| self.cache.remove(h).ok_or_else(|| format!("arcade {} did not send block {h}", self.addr))).collect()
    }

    fn describe(&self) -> String {
        format!("arcade {} (its Core node)", self.addr)
    }
}
