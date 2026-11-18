//! The dogefeed (D://) index journals each block it reads as family DOGEFEED (docs/ARCADECHAIN.md §2.2; listed by the
//! ARCADE catalog from rules version 4), from the index's start, the same live and backfilled.
use arcade_chain::encode::write_varint;
use arcade_chain::hash::merkle_root;
use arcade_chain::params::{ChainParams, DIGIBYTE_MAIN};
use arcade_chain::{Block, Hash256, Header, Tx};
use arcade_chaindb::journal::{self, DOGEFEED};
use arcade_chaindb::{BlockCtx, ChainDb, Indexer};
use arcade_ledger::dproto::{self, DprotoIndexer};
use arcade_store::TableDef;
use std::sync::Arc;

fn push(v: &[u8]) -> Vec<u8> {
    let mut s = if v.len() <= 75 { vec![v.len() as u8] } else { vec![0x4c, v.len() as u8] };
    s.extend_from_slice(v);
    s
}

/// One input (a coinbase's when `input` is None), one output paying the address of `owner`.
fn tx(input: Option<(Hash256, Vec<u8>)>, value: u64, owner: u8, nonce: u32) -> Tx {
    let mut o = 1i32.to_le_bytes().to_vec();
    write_varint(&mut o, 1);
    match input {
        None => {
            o.extend_from_slice(&[0; 32]);
            o.extend_from_slice(&u32::MAX.to_le_bytes());
            write_varint(&mut o, 4);
            o.extend_from_slice(&nonce.to_le_bytes());
        }
        Some((prev, sig)) => {
            o.extend_from_slice(&prev.0);
            o.extend_from_slice(&0u32.to_le_bytes());
            write_varint(&mut o, sig.len() as u64);
            o.extend_from_slice(&sig);
        }
    }
    o.extend_from_slice(&u32::MAX.to_le_bytes());
    write_varint(&mut o, 1);
    o.extend_from_slice(&value.to_le_bytes());
    let script = [vec![0x76, 0xa9, 0x14], vec![owner; 20], vec![0x88, 0xac]].concat();
    write_varint(&mut o, script.len() as u64);
    o.extend_from_slice(&script);
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

/// The dogefeed index as it is, starting at height 1 of the test chain (Dogecoin's starts at block 6,300,000).
struct FromOne;
impl Indexer for FromOne {
    fn name(&self) -> &'static str {
        DprotoIndexer.name()
    }
    fn tables(&self) -> Vec<TableDef> {
        DprotoIndexer.tables()
    }
    fn start_height(&self, _: &ChainParams) -> u32 {
        1
    }
    fn connect(&self, ctx: &BlockCtx) -> arcade_store::Result<()> {
        DprotoIndexer.connect(ctx)
    }
    fn journals(&self) -> Vec<(u8, Vec<&'static str>)> {
        DprotoIndexer.journals()
    }
}

#[test]
fn the_dogefeed_index_journals_from_its_start() {
    assert_eq!(DprotoIndexer.journals(), vec![(DOGEFEED, dproto::tables())]);
    let dir = tempfile::tempdir().unwrap();
    let db = ChainDb::open(dir.path().join("chain.mdbx")).unwrap();
    let g = block(Hash256::ZERO, vec![tx(None, 50, 200, 0)], 1);
    let mut p: ChainParams = DIGIBYTE_MAIN.clone();
    // A test chain the D:// index reads (it reads only a chain whose net is "doge").
    p.net = "doge";
    p.genesis = Box::leak(g.hash().to_hex().into_boxed_str());
    p.genesis_header = "";
    p.format = arcade_chain::params::PEPECOIN_TEST.format.clone();
    let p: &'static ChainParams = Box::leak(Box::new(p));
    let c = p.chain_no;
    db.add_indexer(c, Arc::new(FromOne)).unwrap();
    // Block 1: nothing for the index. Block 2: an inscribed D:// post. Block 3: nothing again.
    let cb1 = tx(None, 5000, 200, 1);
    let b1 = block(g.hash(), vec![cb1.clone()], 2);
    let body = br#"{"p":"D://","v":1,"b":{"text":"hello"},"map":{"app":"dogefeed","type":"post","ts":1},"aip":{"alg":"DOGE_ECDSA","addr":"D","sig":"s"}}"#;
    let script = [push(b"ord"), vec![0x51], push(b"application/json"), vec![0], push(body), push(&[0x30; 71])].concat();
    let post = tx(Some((cb1.txid, script)), 4000, 7, 0);
    let b2 = block(b1.hash(), vec![tx(None, 5000, 200, 2), post], 3);
    let b3 = block(b2.hash(), vec![tx(None, 5000, 200, 3)], 4);
    db.connect_blocks(p, &[g.clone(), b1.clone(), b2.clone(), b3.clone()]).unwrap();

    assert_eq!(db.journal_from(c, DOGEFEED).unwrap(), Some(1), "journals run from the index's start");
    assert_eq!(db.journal(c, DOGEFEED, 0).unwrap(), None, "before the index starts");
    assert_eq!(db.journal(c, DOGEFEED, 1).unwrap(), Some(journal::EMPTY.to_vec()));
    assert_eq!(db.journal(c, DOGEFEED, 3).unwrap(), Some(journal::EMPTY.to_vec()));
    let rows = journal::decode(&db.journal(c, DOGEFEED, 2).unwrap().unwrap()).unwrap();
    let tables: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
    assert!(tables.contains(&dproto::REC) && tables.contains(&dproto::BY_TX) && tables.contains(&dproto::BY_AUTHOR), "{tables:?}");
    assert!(rows.iter().all(|r| r.2.is_some()), "every row written");
    let rec = rows.iter().find(|r| r.0 == dproto::REC).unwrap().2.clone().unwrap();
    assert!(String::from_utf8_lossy(&rec).contains("hello"));

    // The same index built later from scratch (backfilled) journals the same bytes from the same start.
    let dir2 = tempfile::tempdir().unwrap();
    let db2 = ChainDb::open(dir2.path().join("chain.mdbx")).unwrap();
    db2.connect_blocks(p, &[g, b1, b2, b3]).unwrap();
    db2.add_indexer(c, Arc::new(FromOne)).unwrap();
    while db2.backfill(p, DprotoIndexer.name(), 1).unwrap() > 0 {}
    assert_eq!(db2.journal_from(c, DOGEFEED).unwrap(), Some(1));
    for h in 1..=3 {
        assert_eq!(db2.journal(c, DOGEFEED, h).unwrap(), db.journal(c, DOGEFEED, h).unwrap(), "height {h}");
    }
    // A reorg takes the block's journal with it.
    db.disconnect_tip(c).unwrap();
    assert_eq!(db.journal(c, DOGEFEED, 3).unwrap(), None);
}
