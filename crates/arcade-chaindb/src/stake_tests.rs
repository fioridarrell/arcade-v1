//! crate::stake on a made-up staked chain: mined blocks paying one coin each to a test key,
//! then staked blocks, each staking the next coin, its time ground until the kernel meets the
//! target (with the staked value small, about one slot in four does).

use crate::db::ChainDb;
use crate::schema::*;
use crate::stake::*;
use arcade_chain::encode::write_varint;
use arcade_chain::hash::merkle_root;
use arcade_chain::kotia_pos as pos;
use arcade_chain::params::{ChainParams, KOTIA_MAIN};
use arcade_chain::pow::{bits_from_target, PowAlgo, PowRule};
use arcade_chain::{Block, Format, Hash256, Header, Tx};
use arcade_store::StoreError;
use secp256k1::{Message, PublicKey, SecretKey, SECP256K1};

const T0: u32 = 1_700_000_000;
const LAST_POW: u32 = 25;
/// Small enough that target * value stays under 2^256 at the PoS limit.
const VALUE: u64 = 1 << 18;
const FMT: Format = Format::POS_TIMED;

fn tx(time: u32, inputs: &[(Hash256, u32, Vec<u8>)], outputs: &[(u64, Vec<u8>)]) -> Tx {
    let mut o = Vec::new();
    o.extend_from_slice(&1i32.to_le_bytes());
    o.extend_from_slice(&time.to_le_bytes());
    write_varint(&mut o, inputs.len() as u64);
    for (txid, vout, ss) in inputs {
        o.extend_from_slice(&txid.0);
        o.extend_from_slice(&vout.to_le_bytes());
        write_varint(&mut o, ss.len() as u64);
        o.extend_from_slice(ss);
        o.extend_from_slice(&u32::MAX.to_le_bytes());
    }
    write_varint(&mut o, outputs.len() as u64);
    for (v, s) in outputs {
        o.extend_from_slice(&v.to_le_bytes());
        write_varint(&mut o, s.len() as u64);
        o.extend_from_slice(s);
    }
    o.extend_from_slice(&0u32.to_le_bytes());
    Tx::parse_with(&o, &FMT).unwrap()
}

fn coinbase(time: u32, tag: u32, outputs: &[(u64, Vec<u8>)]) -> Tx {
    tx(time, &[(Hash256::ZERO, u32::MAX, tag.to_le_bytes().to_vec())], outputs)
}

fn p2pk(pk: &PublicKey) -> Vec<u8> {
    let mut s = vec![33];
    s.extend_from_slice(&pk.serialize());
    s.push(0xac);
    s
}

fn make(prev: Hash256, time: u32, bits: u32, txs: Vec<Tx>, signer: Option<&SecretKey>) -> Block {
    let header = Header { version: 7, prev, merkle_root: merkle_root(&txs.iter().map(|t| t.txid).collect::<Vec<_>>()), time, bits, nonce: 0 };
    let id = FMT.block_id(&header);
    let sig = signer.map(|k| SECP256K1.sign_ecdsa(&Message::from_digest(id.0), k).serialize_der().to_vec()).unwrap_or_default();
    let mut raw = header.encode().to_vec();
    write_varint(&mut raw, txs.len() as u64);
    for t in &txs {
        raw.extend_from_slice(&t.raw);
    }
    write_varint(&mut raw, sig.len() as u64);
    raw.extend_from_slice(&sig);
    Block::parse_with(&raw, &FMT).unwrap()
}

struct Chain {
    sk: SecretKey,
    blocks: Vec<Block>,
    /// The modifier after each block, worked out here as the staker does.
    mods: Vec<Hash256>,
    /// Mined coins: (txid, tx time).
    coins: Vec<(Hash256, u32)>,
    next_coin: usize,
}

impl Chain {
    fn new() -> Chain {
        let sk = SecretKey::from_slice(&[7u8; 32]).unwrap();
        let pk = PublicKey::from_secret_key(SECP256K1, &sk);
        let mut c = Chain { sk, blocks: Vec::new(), mods: Vec::new(), coins: Vec::new(), next_coin: 0 };
        for h in 0..=LAST_POW {
            let time = T0 + 120 * h;
            let cb = coinbase(time, h + 1, &[(VALUE, p2pk(&pk))]);
            let prev = c.blocks.last().map(|b| b.hash()).unwrap_or(Hash256::ZERO);
            if h > 0 {
                c.coins.push((cb.txid, time));
            }
            c.push(make(prev, time, 0x207fffff, vec![cb], None));
        }
        c
    }

    fn push(&mut self, b: Block) {
        let m = match self.mods.last() {
            None => Hash256::ZERO,
            Some(prev) => pos::next_modifier(prev, &kernel_of(&b)),
        };
        self.mods.push(m);
        self.blocks.push(b);
    }

    fn height(&self) -> u32 {
        self.blocks.len() as u32 - 1
    }

    /// The next staked block (staking the next coin), with `spoil` applied to its time and bits
    /// before it is signed.
    fn staked(&mut self, spoil: impl FnOnce(&mut u32, &mut u32)) -> Block {
        let h = self.height() + 1;
        let prev = self.blocks.last().unwrap();
        let (ph, p2) = (prev.header, self.blocks[h as usize - 2].header);
        let mut bits = if h >= LAST_POW + 3 { pos::next_bits(ph.bits, ph.time, p2.time) } else { bits_from_target(pos::pos_limit()) };
        let (txid, coin_time) = self.coins[self.next_coin];
        self.next_coin += 1;
        let modifier = *self.mods.last().unwrap();
        let mut t = (ph.time + 128) & !pos::STAKE_TIMESTAMP_MASK;
        while !pos::check_kernel(&modifier, bits, coin_time, &txid, 0, VALUE, t) {
            t += 16;
        }
        spoil(&mut t, &mut bits);
        let pk = PublicKey::from_secret_key(SECP256K1, &self.sk);
        let cb = coinbase(t, h + 1, &[(0, vec![])]);
        let cs = tx(t, &[(txid, 0, vec![])], &[(0, vec![]), (VALUE + pos::STAKE_SUBSIDY, p2pk(&pk))]);
        make(prev.hash(), t, bits, vec![cb, cs], Some(&self.sk))
    }
}

fn params(g: &Block) -> &'static ChainParams {
    let mut p = KOTIA_MAIN.clone();
    p.value_rules = None;
    p.block_rules = None;
    p.genesis = Box::leak(g.hash().to_hex().into_boxed_str());
    p.genesis_header = "";
    p.checkpoints = &[];
    p.pow = PowRule::PowThenStake { algo: PowAlgo::Scrypt, last_pow_height: LAST_POW };
    Box::leak(Box::new(p))
}

/// A chain with `n` good staked blocks, connected one at a time.
fn synced(n: u32) -> (tempfile::TempDir, ChainDb, Chain, &'static ChainParams) {
    let mut ch = Chain::new();
    let p = params(&ch.blocks[0]);
    let dir = tempfile::tempdir().unwrap();
    let db = ChainDb::open(dir.path().join("chain.mdbx")).unwrap();
    for b in &ch.blocks {
        db.connect_blocks(p, std::slice::from_ref(b)).unwrap();
    }
    for _ in 0..n {
        let b = ch.staked(|_, _| {});
        db.connect_blocks(p, std::slice::from_ref(&b)).unwrap();
        ch.push(b);
    }
    (dir, db, ch, p)
}

fn run_pass(db: &ChainDb, p: &ChainParams, max: u32) -> (StakeStatus, u32) {
    let mut steps = 0;
    loop {
        steps += 1;
        let s = db.stake_step(p, max, std::time::Duration::from_secs(5)).unwrap();
        if s.at == Some(s.tip) {
            return (s, steps);
        }
        assert!(steps < 1000);
    }
}

/// As a chain synced before the checks existed: no stake rows, no pass.
fn clear_rows(db: &ChainDb, c: u16) {
    db.store(c)
        .write(|w| {
            for (k, _) in w.scan_prefix(STAKE, &k_chain(c))? {
                w.del(STAKE, &k)?;
            }
            w.del(STATE, &k_state(c, PASS))?;
            w.del(STATE, &k_state(c, ON))?;
            Ok(())
        })
        .unwrap();
}

fn turn_on(db: &ChainDb, c: u16) {
    db.store(c).write(|w| w.put(STATE, &k_state(c, ON), b"1")).unwrap();
}

#[test]
fn staked_blocks_are_checked_as_they_connect() {
    let (_d, db, ch, p) = synced(12);
    let c = p.chain_no;
    for h in 0..=ch.height() {
        let r = db.stake_row_at(c, h).unwrap().unwrap();
        assert_eq!(r.modifier, ch.mods[h as usize], "modifier at {h}");
        assert_eq!(r.mark, if h > LAST_POW { MARK_OK } else { MARK_NONE }, "mark at {h}");
    }
    assert_eq!(db.stake_modifier_at(c, ch.height()).unwrap(), Some(*ch.mods.last().unwrap()));
    // Not on until the pass has looked: no confirmations.
    assert!(!db.stake_on(p).unwrap());
    let m = db.money_depth(p).unwrap();
    assert!(!m.confirms() && m.at(ch.height()) == 0);
    let (s, _) = run_pass(&db, p, 1000);
    assert!(s.on && s.why.is_none() && s.bad == 0, "{s:?}");
    let m = db.money_depth(p).unwrap();
    assert!(m.confirms());
    assert_eq!(m.at(ch.height()), 1);
    assert_eq!(m.at(ch.height() - 5), 6);
    assert_eq!(m.at(3), ch.height() - 2, "mined, under a checked staked stretch");
    assert_eq!(m.at(ch.height() + 1), 0);
}

#[test]
fn a_broken_stake_is_marked_until_on_then_invalid() {
    let (_d, db, mut ch, p) = synced(10);
    let c = p.chain_no;
    // Off: a block whose kernel misses (one slot later, still a good time) is kept, marked.
    let bad = loop {
        let (txid, ct) = ch.coins[ch.next_coin];
        let modifier = *ch.mods.last().unwrap();
        let b = ch.staked(|t, _| *t += 16);
        if !pos::check_kernel(&modifier, b.header.bits, ct, &txid, 0, VALUE, b.header.time) {
            break b;
        }
        // That slot met the target too: a good block; try again on top of it.
        db.connect_blocks(p, std::slice::from_ref(&b)).unwrap();
        ch.push(b);
        assert!(ch.next_coin < 20);
    };
    db.connect_blocks(p, std::slice::from_ref(&bad)).unwrap();
    assert_eq!(db.stake_row_at(c, ch.height() + 1).unwrap().unwrap().mark, MARK_BAD);
    ch.push(bad);
    // The gate stays shut while it is among the newest.
    let (s, _) = run_pass(&db, p, 1000);
    assert!(!s.on && s.bad == 1 && s.last_bad == Some(ch.height()), "{s:?}");
    assert!(s.why.unwrap().contains("broke a stake rule"));
    let good = ch.staked(|_, _| {});
    db.connect_blocks(p, std::slice::from_ref(&good)).unwrap();
    ch.push(good);
    // On (the gate would turn it on after GATE good blocks): the run stops at the broken block.
    turn_on(&db, c);
    let m = db.money_depth(p).unwrap();
    assert_eq!(m.at(ch.height()), 1);
    assert_eq!(m.at(ch.height() - 1), 0, "the broken block is in the run");
    // Now a broken stake is an invalid block.
    let (modifier, (txid, ct)) = (*ch.mods.last().unwrap(), ch.coins[ch.next_coin]);
    type Spoil = Box<dyn FnOnce(&mut u32, &mut u32)>;
    let miss: Spoil = Box::new(move |t, b| {
        *t += 16;
        while pos::check_kernel(&modifier, *b, ct, &txid, 0, VALUE, *t) {
            *t += 16;
        }
    });
    let spoils: Vec<(&str, Spoil)> = vec![("bits", Box::new(|_, b| *b -= 1)), ("time", Box::new(|t, _| *t += 1)), ("kernel", miss)];
    for (what, spoil) in spoils {
        let b = ch.staked(spoil);
        ch.next_coin -= 1;
        let r = db.connect_blocks(p, std::slice::from_ref(&b));
        assert!(matches!(r, Err(StoreError::InvalidBlock { .. })), "{what}: {r:?}");
    }
    // Signed by another key.
    let b = ch.staked(|_, _| {});
    ch.next_coin -= 1;
    let forged = make(b.header.prev, b.header.time, b.header.bits, b.txs.clone(), Some(&SecretKey::from_slice(&[9u8; 32]).unwrap()));
    assert!(matches!(db.connect_blocks(p, std::slice::from_ref(&forged)), Err(StoreError::InvalidBlock { .. })));
    // Mined, after the last proof-of-work height.
    let prev = ch.blocks.last().unwrap();
    let t = prev.header.time + 128;
    let mined = make(prev.hash(), t, 0x207fffff, vec![coinbase(t, 999, &[(1, vec![0x51])])], None);
    assert!(matches!(db.connect_blocks(p, std::slice::from_ref(&mined)), Err(StoreError::InvalidBlock { .. })));
    // The good one still connects, and counts.
    db.connect_blocks(p, std::slice::from_ref(&b)).unwrap();
    assert_eq!(db.stake_row_at(c, ch.height() + 1).unwrap().unwrap().mark, MARK_OK);
}

/// A chain synced before the checks existed has no rows: the pass works them out in steps, from
/// block 0 or from a seed below the staked stretch, and they match what connecting writes.
#[test]
fn the_pass_fills_in_a_chain_synced_before() {
    let (_d, db, ch, p) = synced(15);
    let c = p.chain_no;
    let rows: Vec<Row> = (0..=ch.height()).map(|h| db.stake_row_at(c, h).unwrap().unwrap()).collect();
    clear_rows(&db, c);
    let s = db.stake_status(p).unwrap();
    assert!(!s.on && s.at.is_none() && s.why.is_some());
    let (s, steps) = run_pass(&db, p, 4);
    assert!(s.on, "{s:?}");
    assert!(steps > 5, "resumable, a few blocks a step ({steps})");
    for h in 0..=ch.height() {
        assert_eq!(db.stake_row_at(c, h).unwrap().unwrap(), rows[h as usize], "row at {h}");
    }
    // From a seed: the modifier after the last mined block.
    clear_rows(&db, c);
    let hash = db.hash_at(c, LAST_POW).unwrap().unwrap();
    let next = db.hash_at(c, LAST_POW + 1).unwrap().unwrap();
    assert!(!db.stake_seed(p, LAST_POW + 1, &next, &rows[LAST_POW as usize + 1].modifier).unwrap(), "not inside the staked stretch");
    assert!(!db.stake_seed(p, LAST_POW, &Hash256([1; 32]), &rows[LAST_POW as usize].modifier).unwrap(), "not off the main chain");
    assert!(db.stake_seed(p, LAST_POW, &hash, &rows[LAST_POW as usize].modifier).unwrap());
    let (s, _) = run_pass(&db, p, 1000);
    assert!(s.on);
    assert!(db.stake_row_at(c, 3).unwrap().is_none(), "nothing worked out under the seed");
    for h in LAST_POW..=ch.height() {
        assert_eq!(db.stake_row_at(c, h).unwrap().unwrap(), rows[h as usize]);
    }
    // A wrong seed: every staked block fails, and the checks never go on (nothing is thrown out).
    clear_rows(&db, c);
    assert!(db.stake_seed(p, LAST_POW, &hash, &Hash256([5; 32])).unwrap());
    let (s, _) = run_pass(&db, p, 1000);
    assert!(!s.on && s.bad >= 10, "most miss (a wrong modifier still meets the test's easy target now and then): {s:?}");
    assert_eq!(db.tip(c).unwrap().unwrap().height, ch.height());
}

/// Rows written while connecting leave with their block; the pass follows the chain back.
#[test]
fn rows_follow_a_reorg() {
    let (_d, db, ch, p) = synced(6);
    let c = p.chain_no;
    run_pass(&db, p, 1000);
    let top = db.tip(c).unwrap().unwrap();
    assert!(db.stake_row(c, top.b).unwrap().is_some());
    db.disconnect_tip(c).unwrap();
    db.disconnect_tip(c).unwrap();
    assert!(db.stake_row(c, top.b).unwrap().is_none(), "undone with its block");
    let s = db.stake_step(p, 1000, std::time::Duration::from_secs(5)).unwrap();
    assert_eq!(s.at, Some(ch.height() - 2));
    assert!(s.on);
    // And connected again: checked again.
    db.connect_blocks(p, &ch.blocks[ch.height() as usize - 1..]).unwrap();
    assert_eq!(db.stake_row_at(c, ch.height()).unwrap().unwrap().mark, MARK_OK);
}

#[test]
fn block_signatures() {
    let mut ch = Chain::new();
    let b = ch.staked(|_, _| {});
    assert!(block_sig_ok(&b));
    let mut nosig = b.clone();
    nosig.sig = Some(vec![]);
    assert!(!block_sig_ok(&nosig));
    // The key in coinstake output 1, here as P2PK.
    assert_eq!(pos::block_signing_key(&b.txs[1].outputs[1].script_pubkey).map(|k| k.len()), Some(33));
}
