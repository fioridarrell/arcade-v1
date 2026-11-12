//! Sealed copies (docs/ARCADECHAIN.md §3, build step 3): this arcade seals a confirmed segment
//! of a chain it keeps with its own key, keeps only the sealed copy, and reads (and proves)
//! leaves from it.
//!
//! Files, under <home>/arc/<net>/sealed/<segment id hex>/:
//!   sealed.bin     every leaf sealed (sloth::seal_leaf), 128 bytes each, in leaf order
//!   sealed.hashes  the Merkle leaf hash of each sealed leaf (32 bytes each): the replica root's leaves
//!   data.hashes    the Merkle leaf hash of each data leaf: the catalog root's leaves
//!   replica.json   {chain, first, last, bytes, leaves, segment, root, replica_root, sealed_at}
//! While sealing, the segment's bytes are written to data.bin first (it goes once sealed: one
//! copy on disk). The work is split over every core.

use crate::node::Node;
use arcade_arc::block::Opened;
use arcade_arc::merkle::{self, Hash, RootBuilder};
use arcade_arc::params::Net;
use arcade_arc::sloth::{self, DATA_LEAF, SEALED_LEAF};
use arcade_chain::params::ChainParams;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// A replica this arcade holds.
#[derive(Clone, Debug)]
pub struct Replica {
    pub chain: String,
    pub first: u64,
    pub last: u64,
    pub bytes: u64,
    pub leaves: u64,
    pub segment: Hash,
    pub root: Hash,
    pub replica_root: Hash,
    pub dir: PathBuf,
}

/// What a sealing run is doing: (segment id hex, leaves done, leaves in all).
static SEALING: Mutex<Option<(String, u64, u64)>> = Mutex::new(None);

/// Positioned read/write. Linux uses the kernel's `pread`/`pwrite`. Windows has the same
/// operations under different names (`seek_read` / `seek_write`).
fn read_exact_at(f: &std::fs::File, buf: &mut [u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::read_exact_at(f, buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut rest = &mut *buf;
        let mut at = offset;
        while !rest.is_empty() {
            let n = f.seek_read(rest, at)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "short read"));
            }
            at += n as u64;
            rest = &mut rest[n..];
        }
        Ok(())
    }
}

fn write_all_at(f: &std::fs::File, buf: &[u8], offset: u64) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::FileExt::write_all_at(f, buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut rest = buf;
        let mut at = offset;
        while !rest.is_empty() {
            let n = f.seek_write(rest, at)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::WriteZero, "short write"));
            }
            at += n as u64;
            rest = &rest[n..];
        }
        Ok(())
    }
}

pub fn dir_of(node: &Node, net: &Net, segment: &Hash) -> PathBuf {
    node.home.join("arc").join(net.key).join("sealed").join(hex::encode(segment))
}

fn hash_hex(s: &str) -> Option<Hash> {
    hex::decode(s).ok()?.try_into().ok()
}

fn read_replica(dir: &std::path::Path) -> Option<Replica> {
    let v: Value = serde_json::from_slice(&std::fs::read(dir.join("replica.json")).ok()?).ok()?;
    Some(Replica {
        chain: v["chain"].as_str()?.to_string(),
        first: v["first"].as_u64()?,
        last: v["last"].as_u64()?,
        bytes: v["bytes"].as_u64()?,
        leaves: v["leaves"].as_u64()?,
        segment: hash_hex(v["segment"].as_str()?)?,
        root: hash_hex(v["root"].as_str()?)?,
        replica_root: hash_hex(v["replica_root"].as_str()?)?,
        dir: dir.to_path_buf(),
    })
}

/// Every finished replica this arcade holds on `net`.
pub fn replicas(node: &Node, net: &Net) -> Vec<Replica> {
    let base = node.home.join("arc").join(net.key).join("sealed");
    let Ok(rd) = std::fs::read_dir(&base) else { return Vec::new() };
    let mut out: Vec<Replica> = rd.filter_map(|e| read_replica(&e.ok()?.path())).collect();
    out.sort_by_key(|r| (r.chain.clone(), r.first));
    out
}

pub fn sealing() -> Option<(String, u64, u64)> {
    SEALING.lock().unwrap().clone()
}

/// Seal one segment (its catalog entry's fields) under this arcade's key. Blocks the caller
/// for as long as it takes (run it on its own thread). The replica, or why not.
pub fn seal(node: &Node, net: &Net, p: &ChainParams, segment: Hash, first: u64, last: u64, bytes: u64, root: Hash) -> Result<Replica, String> {
    let dir = dir_of(node, net, &segment);
    if let Some(r) = read_replica(&dir) {
        return Ok(r);
    }
    {
        let mut s = SEALING.lock().unwrap();
        if s.is_some() {
            return Err("another segment is being sealed".into());
        }
        *s = Some((hex::encode(segment), 0, bytes.div_ceil(DATA_LEAF as u64)));
    }
    let r = seal_into(node, p, &dir, segment, first, last, bytes, root);
    *SEALING.lock().unwrap() = None;
    if r.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    r
}

#[allow(clippy::too_many_arguments)]
fn seal_into(node: &Node, p: &ChainParams, dir: &std::path::Path, segment: Hash, first: u64, last: u64, bytes: u64, root: Hash) -> Result<Replica, String> {
    let io = |e: std::io::Error| format!("{}: {e}", dir.display());
    std::fs::create_dir_all(dir).map_err(io)?;
    // 1. The segment's bytes as one file (table, then blocks), checked against the root.
    let data_path = dir.join("data.bin");
    {
        let mut sizes = Vec::new();
        for h in first..=last {
            sizes.push(block(node, p, h)?.len() as u64);
        }
        let mut f = std::io::BufWriter::new(std::fs::File::create(&data_path).map_err(io)?);
        use std::io::Write;
        f.write_all(&arcade_arc::segment::table(&sizes)).map_err(io)?;
        for h in first..=last {
            f.write_all(&block(node, p, h)?).map_err(io)?;
        }
        f.flush().map_err(io)?;
    }
    let data = std::fs::File::open(&data_path).map_err(io)?;
    let have = data.metadata().map_err(io)?.len();
    if have != bytes {
        return Err(format!("the blocks make {have} bytes, the catalog says {bytes}"));
    }
    let leaves = bytes.div_ceil(DATA_LEAF as u64);
    let leaf_at = |i: u64| -> std::io::Result<[u8; DATA_LEAF]> {
        let mut l = [0u8; DATA_LEAF];
        let at = i * DATA_LEAF as u64;
        let n = (bytes - at).min(DATA_LEAF as u64) as usize;
        read_exact_at(&data, &mut l[..n], at)?;
        Ok(l)
    };
    // 2. Seal every leaf, on every core; each leaf goes to its place in sealed.bin.
    let me = node.identity.verifying_key().to_bytes();
    let sealed = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(true).open(dir.join("sealed.bin")).map_err(io)?;
    sealed.set_len(leaves * SEALED_LEAF as u64).map_err(io)?;
    let done = AtomicU64::new(0);
    let next = AtomicU64::new(0);
    let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    const CHUNK: u64 = 256;
    let failed: Mutex<Option<String>> = Mutex::new(None);
    std::thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| loop {
                let start = next.fetch_add(CHUNK, Ordering::Relaxed);
                if start >= leaves || failed.lock().unwrap().is_some() {
                    return;
                }
                let end = (start + CHUNK).min(leaves);
                let mut out = Vec::with_capacity(((end - start) as usize) * SEALED_LEAF);
                for i in start..end {
                    match leaf_at(i) {
                        Ok(l) => out.extend_from_slice(&sloth::seal_leaf(&me, &segment, i, &l)),
                        Err(e) => {
                            *failed.lock().unwrap() = Some(e.to_string());
                            return;
                        }
                    }
                }
                if let Err(e) = write_all_at(&sealed, &out, start * SEALED_LEAF as u64) {
                    *failed.lock().unwrap() = Some(e.to_string());
                    return;
                }
                let d = done.fetch_add(end - start, Ordering::Relaxed) + end - start;
                if let Some(s) = SEALING.lock().unwrap().as_mut() {
                    s.1 = d;
                }
            });
        }
    });
    if let Some(e) = failed.into_inner().unwrap() {
        return Err(format!("sealing: {e}"));
    }
    sealed.sync_all().map_err(io)?;
    // 3. Both trees' leaf hashes, and their roots.
    let mut data_tree = RootBuilder::new();
    let mut sealed_tree = RootBuilder::new();
    let mut dh = std::io::BufWriter::new(std::fs::File::create(dir.join("data.hashes")).map_err(io)?);
    let mut sh = std::io::BufWriter::new(std::fs::File::create(dir.join("sealed.hashes")).map_err(io)?);
    use std::io::Write;
    let mut s = [0u8; SEALED_LEAF];
    for i in 0..leaves {
        let d = merkle::leaf_hash(&leaf_at(i).map_err(io)?);
        read_exact_at(&sealed, &mut s, i * SEALED_LEAF as u64).map_err(io)?;
        let h = merkle::leaf_hash(&s);
        data_tree.push_hash(d);
        sealed_tree.push_hash(h);
        dh.write_all(&d).map_err(io)?;
        sh.write_all(&h).map_err(io)?;
    }
    dh.flush().map_err(io)?;
    sh.flush().map_err(io)?;
    let data_root = data_tree.finish().map_err(|e| e.to_string())?;
    if data_root != root {
        return Err("the blocks here don't make the catalog's root".into());
    }
    let replica_root = sealed_tree.finish().map_err(|e| e.to_string())?;
    // 4. One copy on disk: the unsealed bytes go.
    drop(data);
    std::fs::remove_file(&data_path).map_err(io)?;
    let v = json!({"chain": p.key, "first": first, "last": last, "bytes": bytes, "leaves": leaves, "segment": hex::encode(segment),
                   "root": hex::encode(root), "replica_root": hex::encode(replica_root), "sealed_at": arcade_app::now()});
    std::fs::write(dir.join("replica.json"), v.to_string()).map_err(io)?;
    read_replica(dir).ok_or_else(|| "replica.json unreadable".into())
}

fn block(node: &Node, p: &ChainParams, h: u64) -> Result<Vec<u8>, String> {
    let id = node.chain.hash_at(p.chain_no, h as u32).ok().flatten().ok_or_else(|| format!("{} block {h} isn't here", p.name))?;
    node.chain.block_raw(p.chain_no, &id).ok().flatten().ok_or_else(|| format!("{} block {h} isn't here", p.name))
}

fn hashes(path: &std::path::Path) -> Result<Vec<Hash>, String> {
    let b = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(b.chunks_exact(32).map(|c| c.try_into().unwrap()).collect())
}

impl Replica {
    /// Leaf `i` of the data, read from the sealed copy (one squaring).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn data_leaf(&self, owner: &[u8; 32], i: u64) -> Result<[u8; DATA_LEAF], String> {
        let mut s = [0u8; SEALED_LEAF];
        let f = std::fs::File::open(self.dir.join("sealed.bin")).map_err(|e| e.to_string())?;
        read_exact_at(&f, &mut s, i * SEALED_LEAF as u64).map_err(|e| e.to_string())?;
        sloth::unseal_leaf(owner, &self.segment, i, &s).map_err(|e| format!("leaf {i} doesn't unseal: {e:?}"))
    }

    /// Leaves `picks` opened: each sealed leaf with its path to the replica root, and its data
    /// with its path to the catalog root.
    pub fn open(&self, owner: &[u8; 32], picks: &[u64]) -> Result<Vec<Opened>, String> {
        let sh = hashes(&self.dir.join("sealed.hashes"))?;
        let dh = hashes(&self.dir.join("data.hashes"))?;
        let f = std::fs::File::open(self.dir.join("sealed.bin")).map_err(|e| e.to_string())?;
        picks
            .iter()
            .map(|&i| {
                let mut sealed = [0u8; SEALED_LEAF];
                read_exact_at(&f, &mut sealed, i * SEALED_LEAF as u64).map_err(|e| e.to_string())?;
                let data = sloth::unseal_leaf(owner, &self.segment, i, &sealed).map_err(|e| format!("leaf {i} doesn't unseal: {e:?}"))?;
                Ok(Opened {
                    sealed,
                    sealed_path: merkle::path(&sh, i).map_err(|e| e.to_string())?,
                    data,
                    data_path: merkle::path(&dh, i).map_err(|e| e.to_string())?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An opened leaf proves itself against both roots, and what unseals is the data.
    #[test]
    fn opened_leaves_prove_against_both_roots() {
        let dir = std::env::temp_dir().join(format!("arcseal-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let owner = [7u8; 32];
        let segment = [9u8; 32];
        let leaves = 37u64;
        let data: Vec<[u8; DATA_LEAF]> = (0..leaves).map(|i| [i as u8; DATA_LEAF]).collect();
        let sealed: Vec<[u8; SEALED_LEAF]> = data.iter().enumerate().map(|(i, d)| sloth::seal_leaf(&owner, &segment, i as u64, d)).collect();
        std::fs::write(dir.join("sealed.bin"), sealed.concat()).unwrap();
        let dh: Vec<Hash> = data.iter().map(|d| merkle::leaf_hash(d)).collect();
        let sh: Vec<Hash> = sealed.iter().map(|s| merkle::leaf_hash(s)).collect();
        std::fs::write(dir.join("data.hashes"), dh.concat()).unwrap();
        std::fs::write(dir.join("sealed.hashes"), sh.concat()).unwrap();
        let r = Replica { chain: "peptest".into(), first: 0, last: 9, bytes: leaves * DATA_LEAF as u64, leaves, segment,
                          root: merkle::root(&dh).unwrap(), replica_root: merkle::root(&sh).unwrap(), dir: dir.clone() };
        let opened = r.open(&owner, &[0, 5, 36]).unwrap();
        for (o, i) in opened.iter().zip([0u64, 5, 36]) {
            assert_eq!(o.data, data[i as usize]);
            assert!(merkle::verify(&r.replica_root, leaves, i, &merkle::leaf_hash(&o.sealed), &o.sealed_path));
            assert!(merkle::verify(&r.root, leaves, i, &merkle::leaf_hash(&o.data), &o.data_path));
        }
        assert_eq!(r.data_leaf(&owner, 5).unwrap(), data[5]);
        assert!(r.open(&[8u8; 32], &[1]).is_err(), "another owner's key doesn't unseal it");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
