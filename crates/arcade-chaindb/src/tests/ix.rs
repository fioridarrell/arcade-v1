//! Index files against the classic tables (docs/INDEXFILES.md): the same answers from
//! every reader whether a chain's settled rows are read from the tables or from files.

use super::*;
use crate::ixconv::{Layout, Options};
use crate::ixfile::Kind;
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;

fn script(i: u32) -> Vec<u8> {
    let tag = (i % 40) as u8;
    if tag % 4 == 3 {
        let mut s = vec![0xa9, 0x14];
        s.extend_from_slice(&[tag; 20]);
        s.push(0x87);
        s
    } else {
        p2pkh(tag)
    }
}

/// A chain over 40 addresses: three spending transactions a block, coins spent in the block
/// that made them and coins spent hundreds of blocks later, addresses paid and spending in
/// one transaction.
fn ix_chain(g: &Block, n: u32) -> Vec<Block> {
    let mut coins: Vec<(Hash256, u32, u64, u32)> = Vec::new();
    let mut prev = g.hash();
    let mut out = Vec::new();
    let mut seed = 11u32;
    let mut rnd = || {
        seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
        seed >> 8
    };
    for h in 1..=n {
        let cb = tx(&[], &[(1000 + h as u64, script(h))], h);
        let mut txs = vec![cb.clone()];
        let mut fresh = vec![(cb.txid, 0, 1000 + h as u64, h)];
        for k in 0..3 {
            if coins.len() < 2 {
                break;
            }
            // Mostly a recent coin (as on real chains), sometimes any coin.
            let mut pick = |coins: &mut Vec<(Hash256, u32, u64, u32)>| {
                let i = if rnd() % 3 == 0 { rnd() as usize % coins.len() } else { coins.len() - 1 - (rnd() as usize % coins.len().min(6)) };
                coins.remove(i)
            };
            let a = pick(&mut coins);
            let b = pick(&mut coins);
            let total = a.2 + b.2;
            let t1 = rnd() % 40;
            let t2 = if k == 0 { a.3 } else { t1 };
            let s = tx(&[(a.0, a.1), (b.0, b.1)], &[(total / 2, script(t1)), (total - total / 2, script(t2))], 0);
            if k == 1 {
                // Spendable by the next transaction of this same block.
                coins.push((s.txid, 0, total / 2, t1));
                coins.push((s.txid, 1, total - total / 2, t2));
            } else {
                fresh.push((s.txid, 0, total / 2, t1));
                fresh.push((s.txid, 1, total - total / 2, t2));
            }
            txs.push(s);
        }
        coins.extend(fresh);
        let blk = block(prev, txs, 10 + h);
        prev = blk.hash();
        out.push(blk);
    }
    out
}

/// The usual fixture, on params without block rules: these chains are longer than the
/// height from which Pepecoin's testnet wants newer block versions.
fn fixture() -> Fixture {
    let mut f = super::fixture();
    let mut p = f.p.clone();
    p.block_rules = None;
    f.p = Box::leak(Box::new(p));
    f
}

fn opts() -> Options {
    Options { layout: Layout { range: 100, era_ranges: 2, depth: UNDO_DEPTH }, level: 3, threads: 2, chunk_rows: 500, checkpoint_rows: 2000, free_bytes: None, floor_bytes: 0 }
}

fn convert(f: &Fixture, o: &Options) -> Vec<String> {
    let mut notes = Vec::new();
    assert!(f.db.convert_to_index_files(f.p, o, &AtomicBool::new(false), &mut |s| notes.push(s)).unwrap());
    notes
}

/// Every reader's answer about every address, transaction and output of `blocks` (and
/// about some that do not exist).
fn answers(f: &Fixture, blocks: &[Block]) -> Vec<String> {
    let c = f.p.chain_no;
    let mut out = Vec::new();
    for i in 0..41 {
        let s = if i == 40 { vec![0x51] } else { script(i) };
        let h = f.db.history_for_script(c, &s).unwrap();
        assert_eq!(f.db.history_count_for_script(c, &s).unwrap(), h.len());
        out.push(format!("hist {i}: {h:?}"));
    }
    let mut txids: Vec<(Hash256, usize)> = blocks.iter().flat_map(|b| b.txs.iter().map(|t| (t.txid, t.outputs.len()))).collect();
    txids.push((Hash256([0xee; 32]), 1));
    for (id, outs) in txids {
        out.push(format!("raw {id}: {:?}", f.db.tx_raw(f.p, &id).unwrap()));
        out.push(format!("loc {id}: {:?} {:?}", f.db.tx_location(f.p, &id).unwrap(), f.db.txids_by_t12(f.p, &id.t12()).unwrap()));
        for vout in 0..outs as u32 + 1 {
            let op = OutPoint { txid: id, vout };
            out.push(format!("out {id}:{vout}: {:?} {:?}", f.db.spender(f.p, &op).unwrap(), f.db.coin(f.p, &op).unwrap().map(|x| (x.value, x.height, x.script))));
        }
    }
    out
}

fn same(a: &[String], b: &[String], what: &str) {
    assert_eq!(a.len(), b.len(), "{what}");
    for (x, y) in a.iter().zip(b) {
        assert_eq!(x, y, "{what}");
    }
}

fn files(f: &Fixture) -> BTreeMap<String, Vec<u8>> {
    std::fs::read_dir(f.db.index_dir(f.p.chain_no))
        .map(|rd| rd.flatten().map(|e| (e.file_name().to_string_lossy().to_string(), std::fs::read(e.path()).unwrap())).collect())
        .unwrap_or_default()
}

/// The files in the index dir (not the marks of a whole read, `<file>.ok`).
fn names(f: &Fixture) -> Vec<String> {
    files(f).into_keys().filter(|n| !n.ends_with(".ok")).collect()
}

/// What removing converted rows from the tables will do (a later step): every settled row
/// of the three tables goes, so only the files can answer for those heights, and the tx
/// index, spends and history tables are flagged trimmed (their readers use the files).
fn remove_settled_rows(f: &Fixture, upto: u32) -> [usize; 3] {
    let c = f.p.chain_no;
    let settled = |b: u32| f.db.block_height_of(c, b).unwrap().is_some_and(|h| h <= upto);
    let hist: Vec<_> = f.db.scan_all(SCRIPT_HIST).unwrap().into_iter().filter(|(k, _)| be(k, 23) <= upto).collect();
    let spends: Vec<_> = f.db.scan_all(SPENDS).unwrap().into_iter().filter(|(_, v)| settled(be(v, 0))).collect();
    let txs: Vec<_> = f.db.scan_all(TXS).unwrap().into_iter().filter(|(_, v)| settled(be(v, 0))).collect();
    f.db.wstore(c)
        .unwrap()
        .write(|w| {
            for (k, _) in &hist {
                w.del(SCRIPT_HIST, k)?;
            }
            for (k, _) in &spends {
                w.del(SPENDS, k)?;
            }
            for (k, v) in &txs {
                w.del_dup(TXS, k, v)?;
            }
            for k in Kind::ALL {
                w.put(STATE, &k_state(c, &crate::ixset::trimmed_name(k)), b"1")?;
            }
            Ok(())
        })
        .unwrap();
    [hist.len(), spends.len(), txs.len()]
}

/// Flag the tables trimmed without removing rows: the readers use the files (where both
/// hold the same rows, the answers must not change).
fn flag_trimmed(f: &Fixture, kinds: &[Kind]) {
    let c = f.p.chain_no;
    f.db.wstore(c)
        .unwrap()
        .write(|w| {
            for k in kinds {
                w.put(STATE, &k_state(c, &crate::ixset::trimmed_name(*k)), b"1")?;
            }
            Ok(())
        })
        .unwrap();
}

fn generation(f: &Fixture, k: Kind) -> u32 {
    let c = f.p.chain_no;
    f.db.store(c).read(|r| crate::ixset::marker(r, c, k)).unwrap().map(|m| be(&m, 4)).unwrap_or(0)
}

fn be(b: &[u8], at: usize) -> u32 {
    u32::from_be_bytes(b[at..at + 4].try_into().unwrap())
}

#[test]
fn files_answer_exactly_as_the_tables() {
    files_against_tables(false);
}

/// The same with blocks of 1/64 the rows: every file has many blocks, so lookups run across
/// block edges and long address histories span several blocks.
#[test]
fn files_of_many_blocks_answer_exactly_as_the_tables() {
    let _small = crate::ixfile::small_blocks();
    files_against_tables(true);
}

/// Blocks per file of each kind's files.
fn blocks_per_file(f: &Fixture) -> Vec<(String, usize)> {
    names(f).into_iter().map(|n| (n.clone(), crate::ixfile::Reader::open(&f.db.index_dir(f.p.chain_no).join(&n)).unwrap().blocks())).collect()
}

fn files_against_tables(small: bool) {
    let c = fixture().p.chain_no;
    let (f, classic) = (fixture(), fixture());
    let blocks = ix_chain(&f.g, 1560);
    for x in [&f, &classic] {
        x.db.connect_blocks(x.p, &blocks[..1360]).unwrap();
    }
    let want = answers(&classic, &blocks);
    same(&answers(&f, &blocks), &want, "before any files");
    assert!(names(&f).is_empty());
    assert!(!f.db.has_index_files(c).unwrap());

    // Tip 1360: heights up to 299 are whole ranges 1000 deep. One era (two ranges), one range.
    convert(&f, &opts());
    // Read as once the tables are trimmed (they still hold every row: the same answers).
    flag_trimmed(&f, &Kind::ALL);
    for k in Kind::ALL {
        assert_eq!(f.db.index_files_upto(c, k).unwrap(), Some(299), "{k:?}");
    }
    let expect = |spans: &[(u32, u32)]| -> Vec<String> {
        let mut v: Vec<String> = Kind::ALL.iter().flat_map(|k| spans.iter().map(|(a, b)| crate::ixfile::file_name(*k, *a, *b))).collect();
        v.sort();
        v
    };
    assert_eq!(names(&f), expect(&[(0, 199), (200, 299)]));
    let info = f.db.index_files_info(c).unwrap();
    assert!(info.iter().all(|(_, upto, n, rows, _)| (*upto, *n) == (299, 2) && *rows > 300), "{info:?}");
    same(&answers(&f, &blocks), &want, "files up to 299, tables above");
    if small {
        let b = blocks_per_file(&f);
        assert!(b.iter().all(|(_, n)| *n >= 3), "{b:?}");
    }
    // Nothing more is settled: running it again changes nothing.
    let before = files(&f);
    assert!(convert(&f, &opts()).is_empty());
    assert_eq!(files(&f), before);

    // A reorg near the tip is the tables' business alone.
    for x in [&f, &classic] {
        for _ in 0..3 {
            x.db.disconnect_tip(c).unwrap();
        }
    }
    same(&answers(&f, &blocks), &answers(&classic, &blocks), "three blocks disconnected");

    // The chain grows: two more ranges settle, and 200..=399 becomes an era.
    for x in [&f, &classic] {
        x.db.connect_blocks(x.p, &blocks[1357..]).unwrap();
    }
    let want = answers(&classic, &blocks);
    same(&answers(&f, &blocks), &want, "grown, files still up to 299");
    let notes = convert(&f, &opts());
    assert!(notes.iter().any(|n| n.contains("era 200..=399 merged")), "{notes:?}");
    assert_eq!(names(&f), expect(&[(0, 199), (200, 399), (400, 499)]));
    for k in Kind::ALL {
        assert_eq!(f.db.index_files_upto(c, k).unwrap(), Some(499));
    }
    same(&answers(&f, &blocks), &want, "files up to 499");

    // Another arcade converting the same chain in one go writes the same bytes
    // (its 200..=399 era is written directly, never merged).
    let other = fixture();
    other.db.connect_blocks(other.p, &blocks).unwrap();
    convert(&other, &Options { threads: 5, chunk_rows: 64, ..opts() });
    assert_eq!(files(&other), files(&f));

    // With the settled rows gone from the tables, only the files can answer: the same.
    let gone = remove_settled_rows(&f, 499);
    assert!(gone.iter().all(|n| *n > 400), "{gone:?}");
    same(&answers(&f, &blocks), &want, "settled rows only in files");

    // Turned off, the tables answer alone (here: with their settled rows removed).
    f.db.set_index_files_off(c).unwrap();
    assert!(f.db.history_for_script(c, &script(1)).unwrap().iter().all(|r| r.0 > 499));
    assert!(f.db.tx_raw(f.p, &blocks[10].txs[0].txid).unwrap().is_none());
}

#[test]
fn a_stopped_conversion_carries_on_to_the_same_files() {
    let (f, whole) = (fixture(), fixture());
    let blocks = ix_chain(&f.g, 1360);
    for x in [&f, &whole] {
        x.db.connect_blocks(x.p, &blocks).unwrap();
    }
    convert(&whole, &opts());

    // Stopped after every chunk of 64 table rows.
    let o = Options { chunk_rows: 64, checkpoint_rows: 64, ..opts() };
    let stop = AtomicBool::new(true);
    let mut runs = 0;
    let mut carried = 0;
    while !f.db.convert_to_index_files(f.p, &o, &stop, &mut |s| carried += s.contains("carrying on") as u32).unwrap() {
        runs += 1;
        assert!(runs < 2000, "no progress");
        // Until a kind is finished nothing reads its files.
        if runs == 3 {
            assert!(!f.db.has_index_files(f.p.chain_no).unwrap());
        }
    }
    assert!(runs > 100 && carried > 100, "{runs} runs, {carried} carried on");
    assert_eq!(files(&f), files(&whole));
    same(&answers(&f, &blocks), &answers(&whole, &blocks), "after a conversion in pieces");

    // A checkpoint of another job (another layout) is not carried on.
    let g = fixture();
    g.db.connect_blocks(g.p, &blocks).unwrap();
    assert!(!g.db.convert_to_index_files(g.p, &Options { layout: Layout { range: 50, era_ranges: 2, depth: UNDO_DEPTH }, ..o }, &stop, &mut |_| {}).unwrap());
    assert!(names(&g).iter().any(|n| n.ends_with(".tmp")));
    convert(&g, &opts());
    assert_eq!(files(&g), files(&whole));
}

#[test]
fn damaged_or_missing_files_are_errors_not_wrong_answers() {
    let f = fixture();
    let c = f.p.chain_no;
    let blocks = ix_chain(&f.g, 1360);
    f.db.connect_blocks(f.p, &blocks).unwrap();
    let classic = answers(&f, &blocks);
    convert(&f, &opts());
    // History as it will be read once its table is trimmed: from the files.
    flag_trimmed(&f, &[Kind::Hist]);
    let dir = f.db.index_dir(c);
    let s1 = script(1);
    assert!(!f.db.history_for_script(c, &s1).unwrap().is_empty());

    // Until the tx index table is trimmed its files are never read: a lookup that misses the
    // table reads none, so a missing file breaks nothing.
    let tpath = dir.join(crate::ixfile::file_name(Kind::Txs, 0, 199));
    let tgood = std::fs::read(&tpath).unwrap();
    std::fs::remove_file(&tpath).unwrap();
    f.db.ixsets.lock().unwrap().clear();
    assert!(f.db.tx_raw(f.p, &Hash256([0xee; 32])).unwrap().is_none());
    assert!(f.db.coin(f.p, &OutPoint { txid: Hash256([0xee; 32]), vout: 0 }).unwrap().is_none());
    assert!(f.db.spender(f.p, &OutPoint { txid: blocks[5].txs[0].txid, vout: 9 }).unwrap().is_none());
    std::fs::write(&tpath, &tgood).unwrap();

    // Bytes changed inside a history file's block: the read fails.
    let path = dir.join(crate::ixfile::file_name(Kind::Hist, 0, 199));
    let good = std::fs::read(&path).unwrap();
    let mut bad = good.clone();
    for b in &mut bad[100..140] {
        *b ^= 0xff;
    }
    std::fs::write(&path, &bad).unwrap();
    let e = f.db.history_for_script(c, &s1).unwrap_err().to_string();
    assert!(e.contains("damaged"), "{e}");
    assert!(f.db.history_count_for_script(c, &s1).is_err());
    std::fs::write(&path, &good).unwrap();
    assert!(f.db.history_for_script(c, &s1).is_ok());

    // A file of the set missing, cut short, or from another chain: the set does not load.
    let reload = || f.db.ixsets.lock().unwrap().clear();
    std::fs::write(&path, &good[..good.len() - 7]).unwrap();
    reload();
    assert!(f.db.history_for_script(c, &s1).is_err());
    std::fs::remove_file(&path).unwrap();
    reload();
    let e = f.db.history_for_script(c, &s1).unwrap_err().to_string();
    assert!(e.contains("none starts at height 0"), "{e}");
    // A whole, valid file that ends on another block at its last height (another chain or
    // fork): it reads back fine on its own, and the set refuses it when loading.
    std::fs::write(&path, &good).unwrap();
    let r = crate::ixfile::Reader::open(&path).unwrap();
    let mut rows = Vec::new();
    r.for_each_row(|x| {
        rows.push(x.to_vec());
        Ok(())
    })
    .unwrap();
    let alien = crate::ixfile::Meta { last_hash: [0x5a; 32], ..r.meta };
    drop(r);
    let mut w = crate::ixfile::Writer::create(&dir, alien, 3, 1).unwrap();
    for x in &rows {
        w.push(x).unwrap();
    }
    w.finish().unwrap();
    crate::ixfile::Reader::open(&path).unwrap().verify().unwrap();
    reload();
    let e = f.db.history_for_script(c, &s1).unwrap_err().to_string();
    assert!(e.contains("not of this chain"), "{e}");
    // The other kinds are not touched by it.
    assert!(f.db.tx_raw(f.p, &blocks[5].txs[0].txid).unwrap().is_some());

    // Back to the tables alone: every answer as before the conversion.
    f.db.set_index_files_off(c).unwrap();
    same(&answers(&f, &blocks), &classic, "files off");
}

#[test]
fn nothing_is_converted_before_it_is_settled() {
    let f = fixture();
    let c = f.p.chain_no;
    let blocks = ix_chain(&f.g, 1098);
    f.db.connect_blocks(f.p, &blocks).unwrap();
    // Tip 1098: the first range (0..=99) is 999 deep.
    assert!(convert(&f, &opts()).is_empty());
    assert!(!f.db.has_index_files(c).unwrap());
    assert!(names(&f).is_empty());
    // A sealing depth inside the undo window is refused.
    let shallow = Options { layout: Layout { range: 100, era_ranges: 2, depth: UNDO_DEPTH - 1 }, ..opts() };
    assert!(f.db.convert_to_index_files(f.p, &shallow, &AtomicBool::new(false), &mut |_| {}).is_err());
    // Address rows still waiting in the log: not yet.
    let g = fixture();
    g.db.connect_blocks_with(g.p, &blocks, true, true).unwrap();
    assert!(g.db.addresses_pending(c).unwrap());
    assert!(g.db.convert_to_index_files(g.p, &opts(), &AtomicBool::new(false), &mut |_| {}).is_err());
    // Addresses off: the tx index alone is converted.
    let h = fixture();
    h.db.set_addresses_off(c).unwrap();
    h.db.connect_blocks(h.p, &ix_chain(&h.g, 1199)).unwrap();
    convert(&h, &opts());
    assert_eq!(h.db.index_files_upto(c, Kind::Txs).unwrap(), Some(199));
    assert_eq!(h.db.index_files_upto(c, Kind::Hist).unwrap(), None);
    assert_eq!(h.db.index_files_upto(c, Kind::Spends).unwrap(), None);
    assert_eq!(names(&h), [crate::ixfile::file_name(Kind::Txs, 0, 199)]);
}

#[test]
fn turning_addresses_off_stops_history_and_spends_files() {
    let f = fixture();
    let c = f.p.chain_no;
    f.db.connect_blocks(f.p, &ix_chain(&f.g, 1360)).unwrap();
    convert(&f, &opts());
    assert!(!f.db.history_for_script(c, &script(1)).unwrap().is_empty());
    // Off: the address tables are emptied, so history and spends no longer answer from
    // files (a part of the answer would be a wrong one). The tx index keeps its files.
    f.db.set_addresses_off(c).unwrap();
    assert_eq!(f.db.index_files_upto(c, Kind::Hist).unwrap(), None);
    assert_eq!(f.db.index_files_upto(c, Kind::Spends).unwrap(), None);
    assert_eq!(f.db.index_files_upto(c, Kind::Txs).unwrap(), Some(299));
    while f.db.clear_address_rows_batch(c, 10_000).unwrap() > 0 {}
    assert!(f.db.history_for_script(c, &script(1)).unwrap().is_empty());
    // On again: the rows are built again first; a conversion before that is refused.
    f.db.set_addresses_on(c).unwrap();
    assert!(f.db.convert_to_index_files(f.p, &opts(), &AtomicBool::new(false), &mut |_| {}).is_err());
}

/// A reorg as deep as the undo window allows, just above the files: the files stay right,
/// and the next conversion seals heights of the new branch.
#[test]
fn a_deep_reorg_just_above_the_files() {
    let _small = crate::ixfile::small_blocks();
    let c = fixture().p.chain_no;
    let (f, classic) = (fixture(), fixture());
    let blocks = ix_chain(&f.g, 1360);
    for x in [&f, &classic] {
        x.db.connect_blocks(x.p, &blocks).unwrap();
    }
    convert(&f, &opts());
    flag_trimmed(&f, &Kind::ALL);
    assert_eq!(f.db.index_files_upto(c, Kind::Hist).unwrap(), Some(299));
    // 990 blocks go: the tip is 370, only 71 above the files.
    for x in [&f, &classic] {
        for _ in 0..990 {
            x.db.disconnect_tip(c).unwrap();
        }
        assert_eq!(x.db.tip(c).unwrap().unwrap().height, 370);
    }
    same(&answers(&f, &blocks), &answers(&classic, &blocks), "990 blocks disconnected");
    // Nothing more is settled on the shorter chain.
    assert!(convert(&f, &opts()).is_empty());
    // Another branch from 370, longer than the old one: blocks paying other addresses.
    let mut prev = blocks[369].hash();
    let mut alt = Vec::new();
    for h in 371..=1500u32 {
        let cb = tx(&[], &[(7 + h as u64, script(h + 7))], h + 5_000_000);
        let b = block(prev, vec![cb], 20_000 + h);
        prev = b.hash();
        alt.push(b);
    }
    for x in [&f, &classic] {
        x.db.connect_blocks(x.p, &alt).unwrap();
    }
    let all: Vec<Block> = blocks.iter().chain(&alt).cloned().collect();
    let want = answers(&classic, &all);
    same(&answers(&f, &all), &want, "on the other branch, files up to 299");
    // Tip 1500: 300..=499 settle, partly old blocks (to 370), partly the new branch.
    convert(&f, &opts());
    assert_eq!(f.db.index_files_upto(c, Kind::Hist).unwrap(), Some(499));
    same(&answers(&f, &all), &want, "files up to 499 across the fork point");
    remove_settled_rows(&f, 499);
    same(&answers(&f, &all), &want, "settled rows only in files");
}

/// Files name blocks by height and position, never by this arcade's block numbers: an arcade
/// whose main-chain blocks got other numbers (it held another branch first) writes the very
/// same files, ids and bytes (design-v3 §4: files equal whichever builder made them).
#[test]
fn files_do_not_depend_on_local_block_numbers() {
    let c = fixture().p.chain_no;
    let (a, b) = (fixture(), fixture());
    let blocks = ix_chain(&a.g, 1360);
    a.db.connect_blocks(a.p, &blocks).unwrap();
    // b first follows a short branch from genesis: its blocks take numbers 1..=7.
    let mut prev = b.g.hash();
    let mut side = Vec::new();
    for h in 1..=7u32 {
        let b = block(prev, vec![tx(&[], &[(5, script(900 + h))], h + 7_000_000)], 30_000 + h);
        prev = b.hash();
        side.push(b);
    }
    b.db.connect_blocks(b.p, &side).unwrap();
    for _ in 0..7 {
        b.db.disconnect_tip(c).unwrap();
    }
    b.db.connect_blocks(b.p, &blocks).unwrap();
    assert_eq!(b.db.tip(c).unwrap().unwrap().hash, a.db.tip(c).unwrap().unwrap().hash);
    convert(&a, &opts());
    convert(&b, &opts());
    assert!(!files(&a).is_empty());
    assert_eq!(files(&a), files(&b));
}

/// A pass that wrote its files and merged an era, then stopped before switching the
/// readers (as a crash there would leave it): the readers keep the old files, and the next
/// pass checks the merged era before adopting it (merging it again when it is damaged).
#[test]
fn a_pass_stopped_before_the_marker_is_carried_through_by_the_next() {
    let c = fixture().p.chain_no;
    let (f, other) = (fixture(), fixture());
    let blocks = ix_chain(&f.g, 1560);
    f.db.connect_blocks(f.p, &blocks[..1360]).unwrap();
    convert(&f, &opts());
    let gen = generation(&f, Kind::Txs);
    f.db.connect_blocks(f.p, &blocks[1360..]).unwrap();
    let want = answers(&f, &blocks);

    crate::ixconv::FAIL_BEFORE_MARKER.with(|x| x.set(true));
    let e = f.db.convert_to_index_files(f.p, &opts(), &AtomicBool::new(false), &mut |_| {}).unwrap_err().to_string();
    crate::ixconv::FAIL_BEFORE_MARKER.with(|x| x.set(false));
    assert!(e.contains("before the marker"), "{e}");
    for k in Kind::ALL {
        assert_eq!(f.db.index_files_upto(c, k).unwrap(), Some(299), "{k:?}");
    }
    let t = |a: u32, b: u32| crate::ixfile::file_name(Kind::Txs, a, b);
    let have = names(&f);
    for n in [t(0, 199), t(200, 299), t(200, 399), t(300, 399), t(400, 499)] {
        assert!(have.contains(&n), "{n} in {have:?}");
    }
    // The readers still use the files up to 299 (the new era reaches too far for them).
    flag_trimmed(&f, &Kind::ALL);
    same(&answers(&f, &blocks), &want, "stopped before the marker");

    // The merged era is damaged meanwhile: the next pass does not adopt it as it is.
    let era = f.db.index_dir(c).join(t(200, 399));
    let mut v = std::fs::read(&era).unwrap();
    let mid = v.len() / 2;
    for b in &mut v[mid..mid + 16] {
        *b ^= 0xff;
    }
    std::fs::write(&era, &v).unwrap();
    let notes = convert(&f, &opts());
    assert!(notes.iter().any(|n| n.contains("merging it again")), "{notes:?}");
    for k in Kind::ALL {
        assert_eq!(f.db.index_files_upto(c, k).unwrap(), Some(499), "{k:?}");
    }
    assert!(generation(&f, Kind::Txs) > gen);
    same(&answers(&f, &blocks), &want, "carried through");
    other.db.connect_blocks(other.p, &blocks).unwrap();
    convert(&other, &opts());
    assert_eq!(files(&f), files(&other));

    // Off, then converted from scratch: the generation never comes back to an old value,
    // and a stray file of the kind is gone.
    let gen = generation(&f, Kind::Txs);
    f.db.set_index_files_off(c).unwrap();
    std::fs::write(f.db.index_dir(c).join(t(0, 49)), b"left by something else").unwrap();
    convert(&f, &opts());
    assert!(generation(&f, Kind::Txs) > gen);
    assert_eq!(files(&f), files(&other));
    flag_trimmed(&f, &Kind::ALL);
    same(&answers(&f, &blocks), &want, "converted again from scratch");
}

/// The free space the hold test's drive reports.
static FREE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn test_free(_: &std::path::Path) -> Option<u64> {
    Some(FREE.load(std::sync::atomic::Ordering::SeqCst))
}

#[test]
fn a_conversion_holds_while_the_drive_is_short_then_goes_on() {
    use std::sync::atomic::Ordering::SeqCst;
    let (f, whole) = (fixture(), fixture());
    let blocks = ix_chain(&f.g, 1360);
    for x in [&f, &whole] {
        x.db.connect_blocks(x.p, &blocks).unwrap();
    }
    convert(&whole, &opts());
    let o = Options { free_bytes: Some(test_free), floor_bytes: 1 << 20, ..opts() };
    // Under the floor: held (and said so) before anything is written; space freed: on it goes,
    // to the same files.
    FREE.store(1000, SeqCst);
    let mut notes = Vec::new();
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            FREE.store(1 << 40, SeqCst);
        });
        assert!(f.db.convert_to_index_files(f.p, &o, &AtomicBool::new(false), &mut |x| notes.push(x)).unwrap());
    });
    assert!(notes.iter().any(|n| n.contains("held:") && n.contains("ARCADE_IX_FREE_GB")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("room again")), "{notes:?}");
    assert_eq!(files(&f), files(&whole));
    // Stopped while held: nothing written, nothing switched.
    let g = fixture();
    g.db.connect_blocks(g.p, &blocks).unwrap();
    FREE.store(1000, SeqCst);
    assert!(!g.db.convert_to_index_files(g.p, &o, &AtomicBool::new(true), &mut |_| {}).unwrap());
    assert!(!g.db.has_index_files(g.p.chain_no).unwrap());
    assert!(!names(&g).iter().any(|n| n.ends_with(".ixf")), "{:?}", names(&g));
}

#[test]
fn a_file_not_checked_here_is_read_whole_before_it_is_used() {
    use crate::ixset::{checked_mark, is_checked, KindSet};
    let f = fixture();
    let c = f.p.chain_no;
    let blocks = ix_chain(&f.g, 1360);
    f.db.connect_blocks(f.p, &blocks).unwrap();
    convert(&f, &opts());
    let dir = f.db.index_dir(c);
    let upto = f.db.index_files_upto(c, Kind::Hist).unwrap().unwrap();
    let load = || KindSet::load(&dir, Kind::Hist, c, upto, None, |h| Ok(f.db.hash_at(c, h)?.map(|x| x.0)));
    // Every file written here is marked as read back whole.
    let set = load().unwrap();
    for r in &set.files {
        let path = crate::ixset::list(&dir, Kind::Hist).unwrap().into_iter().find(|x| (x.0, x.1) == (r.meta.first, r.meta.last)).unwrap().2;
        assert!(is_checked(&path, &r.id), "{}", path.display());
    }
    // A file whose block bytes are damaged (its footer intact, so it opens), as one copied in
    // from elsewhere (no mark): refused when loaded, not adopted.
    let (_, _, path) = crate::ixset::list(&dir, Kind::Hist).unwrap().into_iter().last().unwrap();
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[crate::ixfile::HEADER_LEN + 40] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();
    std::fs::remove_file(checked_mark(&path)).unwrap();
    assert!(load().is_err());
    // A good file without its mark is read whole once and marked.
    let (_, _, other) = crate::ixset::list(&dir, Kind::Hist).unwrap().into_iter().next().unwrap();
    std::fs::remove_file(checked_mark(&other)).unwrap();
    bytes[crate::ixfile::HEADER_LEN + 40] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();
    std::fs::remove_file(checked_mark(&path)).ok();
    load().unwrap();
    assert!(checked_mark(&other).exists() && checked_mark(&path).exists());
    // Files cut by another layout than the chain's (format 3 pins it in each header): refused.
    let with = |l| KindSet::load(&dir, Kind::Hist, c, upto, Some(l), |h| Ok(f.db.hash_at(c, h)?.map(|x| x.0)));
    assert!(with((100, 2)).is_ok());
    assert!(with((100, 10)).err().is_some_and(|e| e.to_string().contains("layout")));
    assert!(with((10_000, 10)).is_err());
}
