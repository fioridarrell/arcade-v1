use crate::db::*;
use crate::schema::*;
use arcade_chain::encode::write_varint;
use arcade_chain::hash::merkle_root;
use arcade_chain::params::{ChainParams, PEPECOIN_TEST};
use arcade_chain::{Block, Hash256, Header, OutPoint, Tx};

fn p2pkh(tag: u8) -> Vec<u8> {
    let mut s = vec![0x76, 0xa9, 0x14];
    s.extend_from_slice(&[tag; 20]);
    s.extend_from_slice(&[0x88, 0xac]);
    s
}

fn tx(inputs: &[(Hash256, u32)], outputs: &[(u64, Vec<u8>)], nonce: u32) -> Tx {
    let mut o = Vec::new();
    o.extend_from_slice(&1i32.to_le_bytes());
    if inputs.is_empty() {
        write_varint(&mut o, 1);
        o.extend_from_slice(&[0; 32]);
        o.extend_from_slice(&u32::MAX.to_le_bytes());
        write_varint(&mut o, 4);
        o.extend_from_slice(&nonce.to_le_bytes()); // unique coinbase
        o.extend_from_slice(&u32::MAX.to_le_bytes());
    } else {
        write_varint(&mut o, inputs.len() as u64);
        for (t, v) in inputs {
            o.extend_from_slice(&t.0);
            o.extend_from_slice(&v.to_le_bytes());
            write_varint(&mut o, 0);
            o.extend_from_slice(&u32::MAX.to_le_bytes());
        }
    }
    write_varint(&mut o, outputs.len() as u64);
    for (v, s) in outputs {
        o.extend_from_slice(&v.to_le_bytes());
        write_varint(&mut o, s.len() as u64);
        o.extend_from_slice(s);
    }
    o.extend_from_slice(&0u32.to_le_bytes());
    Tx::parse(&o).unwrap()
}

fn block(prev: Hash256, txs: Vec<Tx>, time: u32) -> Block {
    let ids: Vec<_> = txs.iter().map(|t| t.txid).collect();
    let header = Header { version: 1, prev, merkle_root: merkle_root(&ids), time, bits: 0x207fffff, nonce: 0 };
    let mut raw = header.encode().to_vec();
    write_varint(&mut raw, txs.len() as u64);
    for t in &txs {
        raw.extend_from_slice(&t.raw);
    }
    Block::parse(&raw).unwrap()
}

/// Test chain params whose genesis is our synthetic genesis.
fn params_for(genesis: &Block) -> &'static ChainParams {
    let mut p = PEPECOIN_TEST.clone();
    p.genesis = Box::leak(genesis.hash().to_hex().into_boxed_str());
    p.genesis_header = "";
    Box::leak(Box::new(p))
}

fn dump(db: &ChainDb) -> Vec<(String, Vec<(Vec<u8>, Vec<u8>)>)> {
    LOGGED
        .iter()
        .map(|t| (t.to_string(), db.store.read(|r| r.scan_prefix(t, b"")).unwrap()))
        .collect()
}

struct Fixture {
    _dir: tempfile::TempDir,
    db: ChainDb,
    p: &'static ChainParams,
    g: Block,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db = ChainDb::open(dir.path().join("chain.mdbx")).unwrap();
    let g = block(Hash256::ZERO, vec![tx(&[], &[(50, p2pkh(0))], 0)], 1);
    let p = params_for(&g);
    db.connect_blocks(p, &[g.clone()]).unwrap();
    Fixture { _dir: dir, db, p, g }
}

#[test]
fn coins_spends_history_and_raw_reads() {
    let f = fixture();
    let c = f.p.chain_no;
    let cb1 = tx(&[], &[(5000, p2pkh(1)), (0, vec![0x6a, 1, 2])], 1);
    let b1 = block(f.g.hash(), vec![cb1.clone()], 2);
    let spend = tx(&[(cb1.txid, 0)], &[(3000, p2pkh(2)), (1900, p2pkh(1))], 0);
    let cb2 = tx(&[], &[(5000, p2pkh(3))], 2);
    let b2 = block(b1.hash(), vec![cb2, spend.clone()], 3);
    let tip = f.db.connect_blocks(f.p, &[b1.clone(), b2.clone()]).unwrap();
    assert_eq!(tip.height, 2);

    // Genesis outputs are not coins; OP_RETURN is never a coin.
    assert!(f.db.coin(c, &OutPoint { txid: f.g.txs[0].txid, vout: 0 }).unwrap().is_none());
    assert!(f.db.coin(c, &OutPoint { txid: cb1.txid, vout: 1 }).unwrap().is_none());
    // cb1:0 was spent by `spend` (block 2's position 1, input 0).
    assert!(f.db.coin(c, &OutPoint { txid: cb1.txid, vout: 0 }).unwrap().is_none());
    let (_, p, vin) = f.db.spender(c, &OutPoint { txid: cb1.txid, vout: 0 }).unwrap().unwrap();
    assert_eq!((p, vin), (1, 0));
    // Balances by address script.
    assert_eq!(f.db.balance_for_script(c, &p2pkh(1)).unwrap(), 1900);
    assert_eq!(f.db.balance_for_script(c, &p2pkh(2)).unwrap(), 3000);
    assert_eq!(f.db.coins_for_script(c, &p2pkh(3)).unwrap().len(), 1);
    // History for address 1: received 5000 at h1; at h2 received 1900 and spent 5000.
    let sh = script_hash(&p2pkh(1));
    let hist = f.db.store.read(|r| r.scan_prefix(SCRIPT_HIST, &k_script(c, &sh))).unwrap();
    assert_eq!(hist.len(), 2);
    // Raw reads rebuild exactly.
    assert_eq!(f.db.block_raw(c, &b2.hash()).unwrap().unwrap(), b2.encode());
    assert_eq!(f.db.tx_raw(f.p, &spend.txid).unwrap().unwrap().0, spend.raw);
    assert_eq!(f.db.hash_at(c, 1).unwrap(), Some(b1.hash()));
}

#[test]
fn disconnect_restores_every_index_exactly() {
    let f = fixture();
    let c = f.p.chain_no;
    let cb1 = tx(&[], &[(5000, p2pkh(1))], 1);
    let b1 = block(f.g.hash(), vec![cb1.clone()], 2);
    f.db.connect_blocks(f.p, &[b1.clone()]).unwrap();
    let before = dump(&f.db);

    let spend = tx(&[(cb1.txid, 0)], &[(4000, p2pkh(2))], 0);
    let b2 = block(b1.hash(), vec![tx(&[], &[(5000, p2pkh(9))], 2), spend], 3);
    f.db.connect_blocks(f.p, &[b2.clone()]).unwrap();
    assert_ne!(dump(&f.db), before);

    let t = f.db.disconnect_tip(c).unwrap();
    assert_eq!(t.hash, b1.hash());
    // Indexes are byte-for-byte what they were, except that b2's header record keeps its
    // block number (its bytes stay stored, forks included) — compare everything else.
    let after = dump(&f.db);
    for ((name, a), (_, b)) in before.iter().zip(after.iter()) {
        if name == STATE {
            // The block-number counter only moves forward (numbers are never reused).
            let nb = k_state(c, "next_b");
            let strip = |v: &Vec<(Vec<u8>, Vec<u8>)>| v.iter().filter(|(k, _)| *k != nb).cloned().collect::<Vec<_>>();
            assert_eq!(strip(a), strip(b), "{name}");
        } else if name == HEADERS {
            let b2k = k_header(c, &b2.hash().0);
            let b: Vec<_> = b.iter().filter(|(k, _)| *k != b2k).cloned().collect();
            assert_eq!(a, &b, "{name}");
        } else {
            assert_eq!(a, b, "{name}");
        }
    }
    assert_eq!(f.db.balance_for_script(c, &p2pkh(1)).unwrap(), 5000);
    // Reconnecting reuses the stored block.
    f.db.connect_blocks(f.p, &[b2.clone()]).unwrap();
    assert_eq!(f.db.balance_for_script(c, &p2pkh(2)).unwrap(), 4000);
    assert_eq!(f.db.block_raw(c, &b2.hash()).unwrap().unwrap(), b2.encode());
}

#[test]
fn reorg_to_other_branch() {
    let f = fixture();
    let c = f.p.chain_no;
    let a1 = block(f.g.hash(), vec![tx(&[], &[(10, p2pkh(1))], 1)], 2);
    let a2 = block(a1.hash(), vec![tx(&[], &[(10, p2pkh(1))], 2)], 3);
    f.db.connect_blocks(f.p, &[a1.clone(), a2]).unwrap();
    assert_eq!(f.db.balance_for_script(c, &p2pkh(1)).unwrap(), 20);
    f.db.disconnect_tip(c).unwrap();
    f.db.disconnect_tip(c).unwrap();
    let b1 = block(f.g.hash(), vec![tx(&[], &[(7, p2pkh(2))], 11)], 2);
    let tip = f.db.connect_blocks(f.p, &[b1.clone()]).unwrap();
    assert_eq!((tip.height, tip.hash), (1, b1.hash()));
    assert_eq!(f.db.balance_for_script(c, &p2pkh(1)).unwrap(), 0);
    assert_eq!(f.db.balance_for_script(c, &p2pkh(2)).unwrap(), 7);
    assert!(f.db.disconnect_tip(c).is_ok());
    assert!(f.db.disconnect_tip(c).is_err(), "genesis stays");
}

#[test]
fn bad_block_rolls_back_whole_batch() {
    let f = fixture();
    let c = f.p.chain_no;
    let b1 = block(f.g.hash(), vec![tx(&[], &[(10, p2pkh(1))], 1)], 2);
    let ghost = tx(&[(Hash256([7; 32]), 0)], &[(1, p2pkh(2))], 0);
    let b2 = block(b1.hash(), vec![tx(&[], &[(10, p2pkh(1))], 2), ghost], 3);
    let before = dump(&f.db);
    assert!(f.db.connect_blocks(f.p, &[b1, b2]).is_err());
    assert_eq!(dump(&f.db), before, "nothing from the failed batch was kept");
    assert_eq!(f.db.tip(c).unwrap().unwrap().height, 0);
}

#[test]
fn block_must_extend_tip() {
    let f = fixture();
    let stray = block(Hash256([1; 32]), vec![tx(&[], &[(1, p2pkh(1))], 5)], 2);
    assert!(f.db.connect_blocks(f.p, &[stray]).is_err());
}

#[test]
fn serves_headers_like_p2p() {
    let f = fixture();
    let b1 = block(f.g.hash(), vec![tx(&[], &[(10, p2pkh(1))], 1)], 2);
    let b2 = block(b1.hash(), vec![tx(&[], &[(10, p2pkh(1))], 2)], 3);
    f.db.connect_blocks(f.p, &[b1.clone(), b2.clone()]).unwrap();
    let pl = f.db.headers_payload(f.p, &[f.g.hash()], 2000).unwrap();
    let hs = arcade_chain::p2p::parse_headers(&pl, &f.p.format).unwrap();
    assert_eq!(hs.iter().map(|e| e.header.hash()).collect::<Vec<_>>(), vec![b1.hash(), b2.hash()]);
    let pl = f.db.headers_payload(f.p, &[b2.hash(), f.g.hash()], 2000).unwrap();
    assert_eq!(arcade_chain::p2p::parse_headers(&pl, &f.p.format).unwrap().len(), 0);
    let pl = f.db.headers_payload(f.p, &[Hash256([9; 32])], 1).unwrap();
    assert_eq!(arcade_chain::p2p::parse_headers(&pl, &f.p.format).unwrap()[0].header.hash(), f.g.hash());
}

#[test]
fn mempool_add_conflict_and_settle() {
    let f = fixture();
    let c = f.p.chain_no;
    let cb1 = tx(&[], &[(5000, p2pkh(1))], 1);
    let b1 = block(f.g.hash(), vec![cb1.clone()], 2);
    f.db.connect_blocks(f.p, &[b1.clone()]).unwrap();
    let spend = tx(&[(cb1.txid, 0)], &[(4000, p2pkh(2))], 0);
    let rival = tx(&[(cb1.txid, 0)], &[(4500, p2pkh(3))], 0);
    assert!(f.db.mempool_add(f.p, &spend, 100).unwrap());
    assert!(!f.db.mempool_add(f.p, &spend, 101).unwrap(), "already pooled");
    assert!(!f.db.mempool_add(f.p, &rival, 102).unwrap(), "double spend of a pooled coin");
    assert_eq!(f.db.mempool(c).unwrap().len(), 1);
    assert_eq!(f.db.mempool_spender(c, &OutPoint { txid: cb1.txid, vout: 0 }).unwrap(), Some(spend.txid));
    // The rival confirms instead: our pooled spend is dropped.
    let b2 = block(b1.hash(), vec![tx(&[], &[(1, p2pkh(9))], 2), rival], 3);
    f.db.connect_blocks(f.p, &[b2]).unwrap();
    assert!(f.db.mempool(c).unwrap().is_empty());
    assert_eq!(f.db.mempool_spender(c, &OutPoint { txid: cb1.txid, vout: 0 }).unwrap(), None);
    // Expiry.
    let cb3 = tx(&[], &[(1, p2pkh(4))], 3);
    assert!(f.db.mempool_add(f.p, &cb3, 10).unwrap());
    assert_eq!(f.db.mempool_expire(f.p, 10 + 3601, 3600).unwrap(), 1);
}
