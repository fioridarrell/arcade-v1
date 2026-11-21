//! Init installs (initsync): headers agreed by several sources, blocks stored only and checked
//! against their headers, then the bulk build; the result equals a chain synced block by block.

use crate::db::*;
use crate::initsync::*;
use crate::schema::*;
use crate::sync::{BlockSource, Syncer};
use arcade_chain::encode::write_varint;
use arcade_chain::hash::{merkle_root, sha256d};
use arcade_chain::p2p::HeaderEntry;
use arcade_chain::params::{BlockRules, ChainParams, BITCOIN_BLOCK_RULES, BITCOIN_MAIN, SCRIPT_WITNESS};
use arcade_chain::pow::{meets_target, try_pow_hash, PowAlgo};
use arcade_chain::{Block, Hash256, Header, Tx};
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const T0: u32 = 1_600_000_000;
const EASY: u32 = 0x207fffff;

/// A block of `txs` on `prev`, mined (sha256d, easy target).
fn mine(prev: Hash256, txs: &[Tx], height: u32) -> Block {
    let ids: Vec<Hash256> = txs.iter().map(|t| t.txid).collect();
    let mut header = Header { version: 1, prev, merkle_root: merkle_root(&ids), time: T0 + 60 * height, bits: EASY, nonce: 0 };
    while try_pow_hash(PowAlgo::Sha256d, &header.encode()).is_some_and(|h| !meets_target(&h, EASY)) {
        header.nonce += 1;
    }
    let mut raw = header.encode().to_vec();
    write_varint(&mut raw, txs.len() as u64);
    for t in txs {
        raw.extend_from_slice(&t.raw);
    }
    Block::parse(&raw).unwrap()
}

/// The coin-set test chain (coreimport_tests::chain: coinbases, spends, OP_RETURNs), mined.
fn chain(n: u32) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    for (h, b) in crate::coreimport_tests::chain(n).into_iter().enumerate() {
        let prev = out.last().map(|x| x.hash()).unwrap_or(Hash256::ZERO);
        out.push(mine(prev, &b.txs, h as u32));
    }
    out
}

fn coinbase(tag: u32, extra_out: Option<Vec<u8>>, witness_nonce: bool) -> Tx {
    let mut o = 1i32.to_le_bytes().to_vec();
    if witness_nonce {
        o.extend_from_slice(&[0, 1]);
    }
    write_varint(&mut o, 1);
    o.extend_from_slice(&[0; 32]);
    o.extend_from_slice(&u32::MAX.to_le_bytes());
    write_varint(&mut o, 4);
    o.extend_from_slice(&tag.to_le_bytes());
    o.extend_from_slice(&u32::MAX.to_le_bytes());
    let outs: Vec<(u64, Vec<u8>)> = std::iter::once((50, vec![0x51, tag as u8])).chain(extra_out.map(|s| (0, s))).collect();
    write_varint(&mut o, outs.len() as u64);
    for (v, s) in &outs {
        o.extend_from_slice(&v.to_le_bytes());
        write_varint(&mut o, s.len() as u64);
        o.extend_from_slice(s);
    }
    if witness_nonce {
        write_varint(&mut o, 1);
        write_varint(&mut o, 32);
        o.extend_from_slice(&[0; 32]);
    }
    o.extend_from_slice(&0u32.to_le_bytes());
    Tx::parse(&o).unwrap()
}

/// A transaction spending `prev`:0, with a witness or (stripped) without one.
fn spend(prev: Hash256, witness: bool) -> Tx {
    let mut o = 2i32.to_le_bytes().to_vec();
    if witness {
        o.extend_from_slice(&[0, 1]);
    }
    write_varint(&mut o, 1);
    o.extend_from_slice(&prev.0);
    o.extend_from_slice(&0u32.to_le_bytes());
    write_varint(&mut o, 0);
    o.extend_from_slice(&u32::MAX.to_le_bytes());
    write_varint(&mut o, 1);
    o.extend_from_slice(&40u64.to_le_bytes());
    write_varint(&mut o, 2);
    o.extend_from_slice(&[0x51, 0x52]);
    if witness {
        write_varint(&mut o, 2);
        write_varint(&mut o, 3);
        o.extend_from_slice(&[1, 2, 3]);
        write_varint(&mut o, 1);
        o.push(0x51);
    }
    o.extend_from_slice(&0u32.to_le_bytes());
    Tx::parse(&o).unwrap()
}

fn params(genesis: &Block, tweak: impl FnOnce(&mut ChainParams)) -> &'static ChainParams {
    let mut p = BITCOIN_MAIN.clone();
    // Made-up chains: no coin or block rules (consensus has its own tests).
    p.value_rules = None;
    p.block_rules = None;
    p.genesis = Box::leak(genesis.hash().to_hex().into_boxed_str());
    p.genesis_header = "";
    p.pow_limit_bits = EASY;
    p.checkpoints = &[];
    p.min_chain_work = "";
    tweak(&mut p);
    Box::leak(Box::new(p))
}

/// A source holding some blocks: honest, or one that damages what it sends.
#[derive(Clone)]
struct Src {
    headers: Vec<Header>,
    blocks: HashMap<Hash256, Vec<u8>>,
    /// Send each block with one byte of its last transaction changed (merkle root no longer matches).
    corrupt: bool,
    /// Send this instead of the block with that hash.
    swap: HashMap<Hash256, Vec<u8>>,
    asked: usize,
}

impl Src {
    fn new(blocks: &[Block]) -> Src {
        Src { headers: blocks.iter().map(|b| b.header).collect(), blocks: blocks.iter().map(|b| (b.hash(), b.encode())).collect(), corrupt: false, swap: HashMap::new(), asked: 0 }
    }
}

impl BlockSource for Src {
    fn get_headers(&mut self, locator: &[Hash256]) -> Result<Vec<HeaderEntry>, String> {
        let ids: Vec<Hash256> = self.headers.iter().map(|h| h.hash()).collect();
        let from = locator.iter().find_map(|l| ids.iter().position(|x| x == l)).map(|i| i + 1).unwrap_or(0);
        Ok(self.headers[from..].iter().take(2000).map(|h| HeaderEntry { header: *h, auxpow: None }).collect())
    }
    fn get_blocks(&mut self, hashes: &[Hash256]) -> Result<Vec<Vec<u8>>, String> {
        self.asked += hashes.len();
        hashes
            .iter()
            .map(|h| {
                if let Some(x) = self.swap.get(h) {
                    return Ok(x.clone());
                }
                let mut raw = self.blocks.get(h).cloned().ok_or_else(|| format!("no block {h}"))?;
                if self.corrupt {
                    let n = raw.len();
                    raw[n - 5] ^= 1;
                }
                Ok(raw)
            })
            .collect()
    }
    fn describe(&self) -> String {
        "test".into()
    }
}

fn open(dir: &Path) -> ChainDb {
    ChainDb::open(dir.join("chain.mdbx")).unwrap()
}

fn table(db: &ChainDb, t: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
    db.scan_all(t).unwrap()
}

fn round(db: &ChainDb, p: &'static ChainParams, srcs: &mut [(&str, &mut Src)]) -> Agreement {
    let s = Syncer::new(db, p);
    let mut v: Vec<(String, &mut dyn BlockSource)> = srcs.iter_mut().map(|(k, s)| (k.to_string(), &mut **s as &mut dyn BlockSource)).collect();
    db.init_headers_round(&s, &mut v, &AtomicBool::new(false)).unwrap()
}

fn begin(db: &ChainDb, p: &'static ChainParams, src: &mut Src) {
    Syncer::new(db, p).ensure_genesis(src).unwrap();
    db.init_start(p.chain_no).unwrap();
}

fn build(db: &ChainDb, p: &'static ChainParams, tmp: &Path, tail: u32) -> Built {
    db.init_build(p, tail, 2, usize::MAX, tmp, &AtomicBool::new(false), &mut |_| {}).unwrap()
}

/// The tail the build leaves: connected the normal way.
fn finish_normally(db: &ChainDb, p: &'static ChainParams, src: &mut Src) {
    let s = Syncer::new(db, p);
    while s.sync_headers_once(src).unwrap() > 0 {}
    s.sync_blocks(src, 100_000, 50).unwrap();
}

fn same_as(db: &ChainDb, reference: &ChainDb, what: &str) {
    for t in [OUTPUTS, TXS, HEIGHT, HEADERS, BLOCK_TX, BLOCK_META] {
        assert!(table(db, t) == table(reference, t), "table {t} ({what})");
    }
}

fn reference(dir: &Path, p: &'static ChainParams, blocks: &[Block]) -> ChainDb {
    let r = ChainDb::open(dir.join("ref.mdbx")).unwrap();
    r.connect_blocks(p, blocks).unwrap();
    r
}

#[test]
fn an_init_install_ends_as_a_normal_sync() {
    let blocks = chain(40);
    let p = params(&blocks[0], |_| {});
    let c = p.chain_no;
    let d = tempfile::tempdir().unwrap();
    let r = reference(d.path(), p, &blocks);
    let db = open(d.path());
    let (mut a, mut b, mut x) = (Src::new(&blocks), Src::new(&blocks), Src::new(&blocks));
    begin(&db, p, &mut a);
    assert_eq!(db.init_phase(c).unwrap(), Some(Phase::Headers));
    let ag = round(&db, p, &mut [("a", &mut a), ("b", &mut b), ("x", &mut x)]);
    assert!(ag.done(), "{}", ag.text());
    assert_eq!((ag.height, ag.new_headers), (40, 40));
    db.set_init_phase(c, Phase::Blocks).unwrap();
    assert_eq!(db.init_missing(c).unwrap().len(), 40);
    assert_eq!(db.init_fetch_blocks(p, &mut [&mut a as &mut dyn BlockSource, &mut b as &mut dyn BlockSource], 7, &AtomicBool::new(false)).unwrap(), 40);
    assert!(db.init_missing(c).unwrap().is_empty());
    // Nothing connected, nothing indexed: the tip is still genesis.
    assert_eq!(db.tip(c).unwrap().unwrap().height, 0);
    assert!(table(&db, OUTPUTS).is_empty());
    // Headers again: nothing new, so on to the build.
    let ag = round(&db, p, &mut [("a", &mut a), ("b", &mut b), ("x", &mut x)]);
    assert!(ag.done() && ag.new_headers == 0);
    db.set_init_phase(c, Phase::Build).unwrap();
    let built = build(&db, p, &d.path().join("tmp"), 5);
    assert_eq!(built.height, 35);
    assert_eq!(db.init_phase(c).unwrap(), Some(Phase::Done));
    assert!(!db.init_active(c));
    assert_eq!(db.tip(c).unwrap().unwrap().height, 35);
    assert!(db.addresses_pending(c).unwrap());
    finish_normally(&db, p, &mut a);
    assert_eq!(db.tip(c).unwrap(), r.tip(c).unwrap());
    same_as(&db, &r, "init install");
    assert_eq!(db.coin_totals(c).unwrap(), r.coin_totals(c).unwrap());
}

#[test]
fn blocks_stored_in_any_order_give_the_same_coins() {
    let blocks = chain(30);
    let p = params(&blocks[0], |_| {});
    let c = p.chain_no;
    let d = tempfile::tempdir().unwrap();
    let r = reference(d.path(), p, &blocks);
    let db = open(d.path());
    let mut a = Src::new(&blocks);
    begin(&db, p, &mut a);
    round(&db, p, &mut [("a", &mut a)]);
    let mut todo = db.init_missing(c).unwrap();
    todo.reverse();
    todo.rotate_left(11);
    for ch in todo.chunks(4) {
        let raws = a.get_blocks(ch).unwrap();
        assert_eq!(db.init_store(p, ch, &raws).unwrap().stored, ch.len());
    }
    build(&db, p, &d.path().join("tmp"), 0);
    let (x, y) = (db.tip(c).unwrap().unwrap(), r.tip(c).unwrap().unwrap());
    assert_eq!((x.hash, x.height), (y.hash, y.height));
    assert_eq!(db.coin_totals(c).unwrap(), r.coin_totals(c).unwrap());
    for b in &blocks[1..] {
        for t in &b.txs {
            for v in 0..t.outputs.len() as u32 {
                let op = arcade_chain::OutPoint { txid: t.txid, vout: v };
                assert_eq!(db.coin(p, &op).unwrap(), r.coin(p, &op).unwrap());
            }
            assert!(db.tx_location(p, &t.txid).unwrap().is_some());
        }
    }
}

#[test]
fn agreement_needs_three_independent_sources_on_one_tip() {
    let blocks = chain(20);
    let p = params(&blocks[0], |_| {});
    let d = tempfile::tempdir().unwrap();
    let db = open(d.path());
    // A shorter branch from 17: its source has another tip.
    let mut fork = blocks[..=17].to_vec();
    for h in 18..=19 {
        let prev = fork.last().unwrap().hash();
        fork.push(mine(prev, &[coinbase(1000 + h, None, false)], h));
    }
    let (mut a, mut b, mut f, mut a2, mut e) = (Src::new(&blocks), Src::new(&blocks), Src::new(&fork), Src::new(&blocks), Src::new(&blocks));
    begin(&db, p, &mut a);
    let ag = round(&db, p, &mut [("a", &mut a), ("f", &mut f), ("b", &mut b)]);
    assert_eq!(ag.height, 20);
    assert_eq!(ag.agree, vec!["a".to_string(), "b".to_string()]);
    assert!(ag.differ.iter().any(|x| x.starts_with("f:")), "{:?}", ag.differ);
    assert!(!ag.done());
    // The same source twice (one key) still counts once.
    let ag = round(&db, p, &mut [("a", &mut a), ("f", &mut f), ("b", &mut b), ("a", &mut a2)]);
    assert!(!ag.done());
    let ag = round(&db, p, &mut [("a", &mut a), ("f", &mut f), ("b", &mut b), ("e", &mut e)]);
    assert!(ag.done(), "{}", ag.text());
    // A source one block behind doesn't know our best header: it doesn't agree either.
    let mut behind = Src::new(&blocks[..20]);
    assert!(!agrees(&ag.best, &behind.get_headers(&[ag.best]).unwrap()));
    // One ahead extends it: it agrees.
    let next = mine(blocks[20].hash(), &[coinbase(77, None, false)], 21);
    let mut ahead = Src::new(&[blocks.clone(), vec![next]].concat());
    assert!(agrees(&ag.best, &ahead.get_headers(&[ag.best]).unwrap()));
    // Total work below the chain's minimum: not done however many agree.
    let p2 = params(&blocks[0], |p| p.min_chain_work = "00000000000000000000000000000000000000000000000000000000ffffffff");
    let d2 = tempfile::tempdir().unwrap();
    let db2 = open(d2.path());
    begin(&db2, p2, &mut a);
    let ag = round(&db2, p2, &mut [("a", &mut a), ("b", &mut b), ("e", &mut e)]);
    assert_eq!(ag.agree.len(), 3);
    assert!(!ag.work_ok && !ag.done());
}

#[test]
fn bad_blocks_are_refused_and_fetched_again() {
    let blocks = chain(12);
    let p = params(&blocks[0], |_| {});
    let c = p.chain_no;
    let d = tempfile::tempdir().unwrap();
    let db = open(d.path());
    let mut good = Src::new(&blocks);
    begin(&db, p, &mut good);
    round(&db, p, &mut [("a", &mut good)]);
    // A wrong merkle root, and a block not asked for: refused, nothing stored.
    let mut bad = Src::new(&blocks);
    bad.corrupt = true;
    let want = [blocks[3].hash()];
    let r = db.init_store(p, &want, &bad.get_blocks(&want).unwrap()).unwrap();
    assert_eq!((r.stored, r.again.len(), r.refused.len()), (0, 1, 1));
    assert!(r.refused[0].1.contains("merkle"), "{:?}", r.refused);
    let r = db.init_store(p, &want, &[blocks[4].encode()]).unwrap();
    assert_eq!((r.stored, r.refused.len()), (0, 1));
    assert_eq!(db.stored_b(c, &blocks[3].hash()).unwrap(), Some(0));
    // The blocks phase takes each batch again from the next source.
    let mut liar = Src::new(&blocks);
    liar.swap.insert(blocks[7].hash(), blocks[8].encode());
    let n = db.init_fetch_blocks(p, &mut [&mut bad as &mut dyn BlockSource, &mut liar as &mut dyn BlockSource, &mut good as &mut dyn BlockSource], 3, &AtomicBool::new(false)).unwrap();
    assert_eq!(n, 12);
    assert!(db.init_missing(c).unwrap().is_empty());
    for b in &blocks[1..] {
        assert_eq!(db.block_raw(c, &b.hash()).unwrap(), Some(b.encode()));
    }
}

#[test]
fn a_block_with_its_witnesses_stripped_is_refused() {
    let g = mine(Hash256::ZERO, &[coinbase(0, None, false)], 0);
    let b1 = mine(g.hash(), &[coinbase(1, None, false)], 1);
    // Block 2: a transaction with a witness, committed to in the coinbase (BIP141).
    let full = spend(b1.txs[0].txid, true);
    let stripped = spend(b1.txs[0].txid, false);
    assert_eq!(full.txid, stripped.txid);
    let nonce = [0u8; 32];
    let mut buf = merkle_root(&[Hash256::ZERO, sha256d(&full.raw)]).0.to_vec();
    buf.extend_from_slice(&nonce);
    let mut commit = vec![0x6a, 0x24, 0xaa, 0x21, 0xa9, 0xed];
    commit.extend_from_slice(&sha256d(&buf).0);
    let cb_full = coinbase(2, Some(commit.clone()), true);
    let cb_stripped = coinbase(2, Some(commit), false);
    let b2 = mine(b1.hash(), &[cb_full.clone(), full], 2);
    let b2_stripped = {
        let mut raw = b2.header.encode().to_vec();
        write_varint(&mut raw, 2);
        raw.extend_from_slice(&cb_stripped.raw);
        raw.extend_from_slice(&stripped.raw);
        raw
    };
    let blocks = vec![g.clone(), b1, b2.clone()];
    let witness_rules = BlockRules { script_flags: |_| SCRIPT_WITNESS, ..BITCOIN_BLOCK_RULES };
    let p = params(&g, |p| p.block_rules = Some(witness_rules));
    let c = p.chain_no;
    let d = tempfile::tempdir().unwrap();
    let db = open(d.path());
    let mut good = Src::new(&blocks);
    begin(&db, p, &mut good);
    round(&db, p, &mut [("a", &mut good)]);
    // Same txids, same merkle root: only the witness commitment tells.
    assert!(Block::parse(&b2_stripped).unwrap().check().is_ok());
    let want = [b2.hash()];
    let r = db.init_store(p, &want, &[b2_stripped.clone()]).unwrap();
    assert_eq!(r.stored, 0, "a stripped block must not be stored");
    assert!(r.refused[0].1.contains("witness"), "{:?}", r.refused);
    let mut stripper = Src::new(&blocks);
    stripper.swap.insert(b2.hash(), b2_stripped);
    db.init_fetch_blocks(p, &mut [&mut stripper as &mut dyn BlockSource, &mut good as &mut dyn BlockSource], 2, &AtomicBool::new(false)).unwrap();
    assert_eq!(db.block_raw(c, &b2.hash()).unwrap(), Some(b2.encode()));
}

#[test]
fn an_init_install_resumes_in_each_phase() {
    let blocks = chain(40);
    let p = params(&blocks[0], |_| {});
    let c = p.chain_no;
    let d = tempfile::tempdir().unwrap();
    let r = reference(d.path(), p, &blocks);
    let tmp = d.path().join("tmp");
    let (mut a, mut b, mut x) = (Src::new(&blocks), Src::new(&blocks), Src::new(&blocks));
    {
        // Headers: a source with only the first 25 stops the round short.
        let db = open(d.path());
        begin(&db, p, &mut a);
        let mut short = Src::new(&blocks[..=25]);
        let ag = round(&db, p, &mut [("s", &mut short)]);
        assert_eq!(ag.height, 25);
    }
    {
        let db = open(d.path());
        assert_eq!(db.init_phase(c).unwrap(), Some(Phase::Headers));
        let ag = round(&db, p, &mut [("a", &mut a), ("b", &mut b), ("x", &mut x)]);
        assert!(ag.done());
        assert_eq!(ag.new_headers, 15);
        db.set_init_phase(c, Phase::Blocks).unwrap();
        // Blocks: a source with only blocks up to 20; the rest stay missing.
        let mut half = Src::new(&blocks[..=20]);
        assert!(db.init_fetch_blocks(p, &mut [&mut half as &mut dyn BlockSource], 5, &AtomicBool::new(false)).is_err());
        assert_eq!(db.init_missing(c).unwrap().len(), 20);
    }
    {
        let db = open(d.path());
        assert_eq!(db.init_phase(c).unwrap(), Some(Phase::Blocks));
        assert_eq!(db.init_fetch_blocks(p, &mut [&mut a as &mut dyn BlockSource], 5, &AtomicBool::new(false)).unwrap(), 20);
        db.set_init_phase(c, Phase::Build).unwrap();
        // Build: stopped while deriving the coin set.
        let stop = AtomicBool::new(false);
        let e = db.init_build(p, 3, 2, usize::MAX, &tmp, &stop, &mut |s| {
            if s.contains("coin set") {
                stop.store(true, Ordering::Relaxed);
            }
        });
        assert!(e.is_err());
        assert_eq!(db.tip(c).unwrap().unwrap().height, 0, "the tip moves only at the end");
        assert_eq!(db.init_built(c), vec!["main".to_string(), "txs".to_string()]);
    }
    let db = open(d.path());
    assert_eq!(db.init_phase(c).unwrap(), Some(Phase::Build));
    assert_eq!(build(&db, p, &tmp, 3).height, 37);
    finish_normally(&db, p, &mut a);
    same_as(&db, &r, "resumed init install");
}
