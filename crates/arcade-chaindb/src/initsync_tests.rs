//! Init installs (initsync): headers agreed by several sources, blocks stored only and checked
//! against their headers, then the bulk build; the result equals a chain synced block by block.

use crate::db::*;
use crate::initsync::*;
use crate::schema::*;
use crate::sync::{BlockSource, Syncer};
use arcade_chain::encode::write_varint;
use arcade_chain::hash::{merkle_root, sha256d};
use arcade_chain::p2p::HeaderEntry;
use arcade_chain::params::{BlockRules, ChainParams, ValueRules, BITCOIN_BLOCK_RULES, BITCOIN_MAIN, SCRIPT_WITNESS};
use arcade_chain::pow::{meets_target, try_pow_hash, PowAlgo};
use arcade_chain::{Block, Format, Hash256, Header, Tx};
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
    /// Answer every headers request with nothing.
    mute: bool,
}

impl Src {
    fn new(blocks: &[Block]) -> Src {
        Src { headers: blocks.iter().map(|b| b.header).collect(), blocks: blocks.iter().map(|b| (b.hash(), b.encode())).collect(), corrupt: false, swap: HashMap::new(), asked: 0, mute: false }
    }
}

impl BlockSource for Src {
    fn get_headers(&mut self, locator: &[Hash256]) -> Result<Vec<HeaderEntry>, String> {
        if self.mute {
            return Ok(vec![]);
        }
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
    build_with(db, p, tmp, tail, usize::MAX).unwrap()
}

/// The build with a coin budget (0: the coin set is derived on disk).
fn build_with(db: &ChainDb, p: &'static ChainParams, tmp: &Path, tail: u32, budget: usize) -> arcade_store::Result<Built> {
    db.init_build(p, tail, 2, budget, tmp, &AtomicBool::new(false), &mut |_| {})
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
    // Stored in any order, numbered by height: the main chain's block numbers run in height
    // order, as a block-by-block sync leaves them (the compressed index needs that).
    let bs: Vec<u32> = blocks[1..].iter().map(|b| db.stored_b(c, &b.hash()).unwrap().unwrap()).collect();
    assert!(bs.windows(2).all(|w| w[1] == w[0] + 1), "{bs:?}");
    assert!(bs[0] > db.stored_b(c, &blocks[0].hash()).unwrap().unwrap());
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
    // Asked from our best header's parent: one block behind sends nothing, which never agrees.
    let parent = blocks[19].hash();
    let mut behind = Src::new(&blocks[..20]);
    assert!(!agrees(p, &ag.best, &behind.get_headers(&[parent]).unwrap()));
    // One ahead sends our best header first: it agrees.
    let next = mine(blocks[20].hash(), &[coinbase(77, None, false)], 21);
    let mut ahead = Src::new(&[blocks.clone(), vec![next]].concat());
    assert!(agrees(p, &ag.best, &ahead.get_headers(&[parent]).unwrap()));
    // A source that answers nothing to anything isn't heard as agreeing.
    let mut mute = Src::new(&blocks);
    mute.mute = true;
    let ag = round(&db, p, &mut [("a", &mut a), ("b", &mut b), ("m", &mut mute)]);
    assert_eq!(ag.agree, vec!["a".to_string(), "b".to_string()]);
    assert!(!ag.done(), "{}", ag.text());
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

/// The test chain's own rules, switched on: a subsidy (`subsidy`), coinbase maturity 0, sizes,
/// versions and lock times; scripts aren't run (the test scripts aren't signed).
fn ruled(g: &Block, subsidy: fn(u32, &Hash256) -> u64) -> &'static ChainParams {
    params(g, |p| {
        p.value_rules = Some(ValueRules { max_money: 21_000_000 * 100_000_000, subsidy, maturity: |_| 0, pegout_maturity: 0, coinbase_rule: None, stake_reward: None, enforced: true });
        p.block_rules = Some(BlockRules { script_flags: |_| 0, bip34_height: u32::MAX, min_version: |_| 1, version_rule: None, scripts: false, ..BITCOIN_BLOCK_RULES });
    })
}

/// Headers agreed, every block stored, the phase at build.
fn stored_all(db: &ChainDb, p: &'static ChainParams, blocks: &[Block]) {
    let mut a = Src::new(blocks);
    begin(db, p, &mut a);
    round(db, p, &mut [("a", &mut a)]);
    db.init_fetch_blocks(p, &mut [&mut a as &mut dyn BlockSource], 7, &AtomicBool::new(false)).unwrap();
    db.set_init_phase(p.chain_no, Phase::Build).unwrap();
}

#[test]
fn the_build_checks_the_rules_and_ends_as_a_normal_sync() {
    let blocks = chain(40);
    let p = ruled(&blocks[0], |_, _| 1_000_000);
    let c = p.chain_no;
    let d = tempfile::tempdir().unwrap();
    let r = reference(d.path(), p, &blocks);
    let db = open(d.path());
    stored_all(&db, p, &blocks);
    // Until it is built, its coins and addresses aren't answered.
    let op = arcade_chain::OutPoint { txid: blocks[1].txs[0].txid, vout: 0 };
    assert!(db.coin(p, &op).is_err());
    assert!(db.coins_for_script(p, &blocks[1].txs[0].outputs[0].script_pubkey).is_err());
    assert!(db.history_for_script(c, &blocks[1].txs[0].outputs[0].script_pubkey).is_err());
    let built = build(&db, p, &d.path().join("tmp"), 5);
    assert_eq!((built.height, built.refetch), (35, 0));
    assert!(db.coin(p, &op).is_ok());
    assert!(built.timing.iter().any(|l| l.starts_with("build rules:") && l.contains("blocks/s")), "{:?}", built.timing);
    assert!(built.rules_checked >= 35, "every block's rules checked: {}", built.rules_checked);
    assert!(db.init_built(c).contains(&"rules".to_string()));
    finish_normally(&db, p, &mut Src::new(&blocks));
    same_as(&db, &r, "init install with the rules on");
}

#[test]
fn a_block_the_rules_refuse_is_fetched_again_once_then_the_build_stops() {
    let blocks = chain(20);
    // Block 7's coinbase pays more than its subsidy.
    let p = ruled(&blocks[0], |h, _| if h == 7 { 1 } else { 1_000_000 });
    let c = p.chain_no;
    let d = tempfile::tempdir().unwrap();
    let tmp = d.path().join("tmp");
    let db = open(d.path());
    stored_all(&db, p, &blocks);
    let built = build(&db, p, &tmp, 0);
    assert_eq!(built.refetch, 1);
    assert_eq!(db.init_phase(c).unwrap(), Some(Phase::Blocks));
    assert_eq!(db.stored_b(c, &blocks[7].hash()).unwrap(), Some(0), "its body was dropped");
    assert_eq!(db.init_missing(c).unwrap(), vec![blocks[7].hash()]);
    assert!(db.init_built(c).is_empty(), "the build starts over");
    assert_eq!(db.tip(c).unwrap().unwrap().height, 0);
    assert!(table(&db, OUTPUTS).is_empty(), "no coin is written before the rules pass");
    // Fetched again (the same block: it is the chain's own), refused again: the build stops.
    let mut a = Src::new(&blocks);
    assert_eq!(db.init_fetch_blocks(p, &mut [&mut a as &mut dyn BlockSource], 7, &AtomicBool::new(false)).unwrap(), 1);
    db.set_init_phase(c, Phase::Build).unwrap();
    let e = build_with(&db, p, &tmp, 0, usize::MAX).unwrap_err();
    assert!(matches!(e, arcade_store::StoreError::InvalidBlock { .. }), "{e}");
    assert!(e.to_string().contains("break the chain's rules"), "{e}");
    assert_eq!(db.tip(c).unwrap().unwrap().height, 0);
    assert!(table(&db, OUTPUTS).is_empty());
}

#[test]
fn the_coin_set_derived_on_disk_ends_as_a_normal_sync() {
    let blocks = chain(40);
    let p = params(&blocks[0], |_| {});
    let d = tempfile::tempdir().unwrap();
    let r = reference(d.path(), p, &blocks);
    let db = open(d.path());
    stored_all(&db, p, &blocks);
    let built = build_with(&db, p, &d.path().join("tmp"), 5, 0).unwrap();
    assert!(built.on_disk);
    assert_eq!(built.height, 35);
    finish_normally(&db, p, &mut Src::new(&blocks));
    same_as(&db, &r, "coin set derived on disk");
}

#[test]
fn a_spend_of_an_output_made_later_in_its_block_is_refused_in_memory_and_on_disk() {
    let g = mine(Hash256::ZERO, &[coinbase(0, None, false)], 0);
    let b1 = mine(g.hash(), &[coinbase(1, None, false)], 1);
    let earlier = spend(b1.txs[0].txid, false);
    let later = spend(earlier.txid, false);
    for (in_order, budget) in [(true, usize::MAX), (true, 0), (false, usize::MAX), (false, 0)] {
        let txs = if in_order { vec![coinbase(2, None, false), earlier.clone(), later.clone()] } else { vec![coinbase(2, None, false), later.clone(), earlier.clone()] };
        let b2 = mine(b1.hash(), &txs, 2);
        let blocks = vec![g.clone(), b1.clone(), b2];
        let p = params(&g, |_| {});
        let d = tempfile::tempdir().unwrap();
        let db = open(d.path());
        stored_all(&db, p, &blocks);
        let r = build_with(&db, p, &d.path().join("tmp"), 0, budget);
        if in_order {
            let b = r.unwrap();
            assert_eq!((b.height, b.on_disk), (2, budget == 0));
        } else {
            let e = r.unwrap_err();
            assert!(matches!(e, arcade_store::StoreError::InvalidBlock { .. }), "budget {budget}: {e}");
            assert!(e.to_string().contains("never made"), "{e}");
            assert!(table(&db, OUTPUTS).is_empty(), "budget {budget}: nothing written");
            assert_eq!(db.tip(p.chain_no).unwrap().unwrap().height, 0);
        }
    }
}

#[test]
fn junk_after_the_transactions_or_a_bad_auxpow_is_refused() {
    let blocks = chain(3);
    let p = params(&blocks[0], |_| {});
    let d = tempfile::tempdir().unwrap();
    let db = open(d.path());
    let mut a = Src::new(&blocks);
    begin(&db, p, &mut a);
    round(&db, p, &mut [("a", &mut a)]);
    // Junk appended: same txids, same merkle root, byte-identical rebuild.
    let b = &blocks[2];
    let mut raw = b.encode();
    raw.extend_from_slice(&[7u8; 100]);
    let r = db.init_store(p, &[b.hash()], &[raw]).unwrap();
    assert_eq!(r.stored, 0);
    assert!(r.refused[0].1.contains("after its transactions"), "{:?}", r.refused);
    // A merge-mined block whose own AuxPoW doesn't prove anything.
    let pa = params(&blocks[0], |p| p.format = Format { auxpow: true, ..Format::BITCOIN });
    let cb = coinbase(9, None, false);
    let header = Header { version: 1 | 0x100, prev: b.hash(), merkle_root: merkle_root(&[cb.txid]), time: T0, bits: EASY, nonce: 0 };
    let mut raw = header.encode().to_vec();
    raw.extend_from_slice(&cb.raw);
    raw.extend_from_slice(&[0u8; 32]);
    for _ in 0..2 {
        write_varint(&mut raw, 0);
        raw.extend_from_slice(&0u32.to_le_bytes());
    }
    raw.extend_from_slice(&Header { version: 1, prev: Hash256::ZERO, merkle_root: Hash256::ZERO, time: T0, bits: EASY, nonce: 0 }.encode());
    write_varint(&mut raw, 1);
    raw.extend_from_slice(&cb.raw);
    let blk = Block::parse_with(&raw, &pa.format).unwrap();
    assert!(blk.auxpow.is_some());
    let r = db.init_store(pa, &[blk.hash()], &[raw]).unwrap();
    assert_eq!(r.stored, 0);
    assert!(r.refused[0].1.contains("AuxPoW"), "{:?}", r.refused);
}

#[test]
fn block_numbers_follow_heights_even_when_the_best_chain_moves() {
    let blocks = chain(12);
    let p = params(&blocks[0], |_| {});
    let c = p.chain_no;
    // A longer branch from 8: once its headers arrive it is the best chain.
    let mut fork = blocks[..=8].to_vec();
    for h in 9..=13 {
        let prev = fork.last().unwrap().hash();
        fork.push(mine(prev, &[coinbase(900 + h, None, false)], h));
    }
    let d = tempfile::tempdir().unwrap();
    let r = reference(d.path(), p, &fork);
    let db = open(d.path());
    let mut a = Src::new(&blocks);
    begin(&db, p, &mut a);
    round(&db, p, &mut [("a", &mut a)]);
    // Stored newest first.
    let mut todo = db.init_missing(c).unwrap();
    todo.reverse();
    let raws = a.get_blocks(&todo).unwrap();
    assert_eq!(db.init_store(p, &todo, &raws).unwrap().stored, 12);
    let b9 = db.stored_b(c, &blocks[9].hash()).unwrap().unwrap();
    // The branch: its blocks take the numbers of their heights, the old ones' bodies go.
    let mut f = Src::new(&fork);
    let ag = round(&db, p, &mut [("f", &mut f)]);
    assert_eq!(ag.height, 13);
    assert_eq!(db.init_fetch_blocks(p, &mut [&mut f as &mut dyn BlockSource], 2, &AtomicBool::new(false)).unwrap(), 5);
    assert_eq!(db.stored_b(c, &fork[9].hash()).unwrap(), Some(b9));
    for old in &blocks[9..=12] {
        assert_eq!(db.stored_b(c, &old.hash()).unwrap(), Some(0), "off the best chain: body dropped");
    }
    let bs: Vec<u32> = fork[1..].iter().map(|b| db.stored_b(c, &b.hash()).unwrap().unwrap()).collect();
    assert!(bs.windows(2).all(|w| w[1] == w[0] + 1), "{bs:?}");
    // And it builds as a block-by-block sync of the branch.
    db.set_init_phase(c, Phase::Build).unwrap();
    build(&db, p, &d.path().join("tmp"), 0);
    assert_eq!(db.tip(c).unwrap().unwrap().hash, fork[13].hash());
    assert_eq!(db.coin_totals(c).unwrap(), r.coin_totals(c).unwrap());
    for b in &fork[1..] {
        for t in &b.txs {
            assert_eq!(db.tx_location(p, &t.txid).unwrap().map(|x| x.0), db.stored_b(c, &b.hash()).unwrap());
        }
    }
}

/// A block on `prev` mined at `bits`.
fn mine_bits(prev: Hash256, txs: &[Tx], height: u32, bits: u32) -> Block {
    let ids: Vec<Hash256> = txs.iter().map(|t| t.txid).collect();
    let mut header = Header { version: 1, prev, merkle_root: merkle_root(&ids), time: T0 + 60 * height, bits, nonce: 0 };
    while try_pow_hash(PowAlgo::Sha256d, &header.encode()).is_some_and(|h| !meets_target(&h, bits)) {
        header.nonce += 1;
    }
    let mut raw = header.encode().to_vec();
    write_varint(&mut raw, txs.len() as u64);
    for t in txs {
        raw.extend_from_slice(&t.raw);
    }
    Block::parse(&raw).unwrap()
}

#[test]
fn a_source_feeding_a_low_work_branch_is_left_out_of_the_headers_round() {
    // Our chain at 16x the easiest work per block; a made-up branch from genesis at the easiest
    // (allowed: a block may be up to 4x easier than its parent), longer but with far less work.
    const HARD: u32 = 0x2007ffff;
    let g = mine(Hash256::ZERO, &[coinbase(0, None, false)], 0);
    let p = params(&g, |_| {});
    let mut good = vec![g.clone()];
    for h in 1..=40 {
        let prev = good.last().unwrap().hash();
        good.push(mine_bits(prev, &[coinbase(h, None, false)], h, HARD));
    }
    let mut low = vec![g.clone()];
    for h in 1..=(SIDE_BUDGET + 40) {
        let prev = low.last().unwrap().hash();
        low.push(mine_bits(prev, &[coinbase(5000 + h, None, false)], h, EASY));
    }
    // Ours first: the low branch is spam and its source is left out.
    let d = tempfile::tempdir().unwrap();
    let db = open(d.path());
    let (mut a, mut l) = (Src::new(&good), Src::new(&low));
    begin(&db, p, &mut a);
    let ag = round(&db, p, &mut [("a", &mut a), ("l", &mut l)]);
    assert_eq!(ag.best, good[40].hash());
    assert!(ag.differ.iter().any(|x| x.starts_with("l: sent") && x.contains("less work")), "{:?}", ag.differ);
    assert!(!ag.differ.iter().any(|x| x.starts_with("a:")), "{:?}", ag.differ);
    // The low branch sent first: our chain still wins and its source is never counted against.
    let d = tempfile::tempdir().unwrap();
    let db = open(d.path());
    let (mut a, mut l) = (Src::new(&good), Src::new(&low));
    begin(&db, p, &mut l);
    let ag = round(&db, p, &mut [("l", &mut l), ("a", &mut a)]);
    assert_eq!(ag.best, good[40].hash());
    assert!(!ag.differ.iter().any(|x| x.contains("less work")), "{:?}", ag.differ);
}

#[test]
fn bitcoin_has_a_checkpoint_at_its_minimum_chain_work_block() {
    assert_eq!(BITCOIN_MAIN.last_checkpoint(u32::MAX), Some(691_719));
    assert_eq!(BITCOIN_MAIN.checkpoint_at(691_719).unwrap().to_hex(), "00000000000000000008a89e854d57e5667df88f1cdef6fde2fbca1de5b639ad");
    // A header fence only: every script is still checked.
    assert_eq!(BITCOIN_MAIN.assume_valid(), None);
    assert!(crate::rules::script_flags(&BITCOIN_MAIN, 500_000, false).is_some());
}
