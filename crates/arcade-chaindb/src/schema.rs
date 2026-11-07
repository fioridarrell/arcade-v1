//! `chain.mdbx`: tables and key layouts. Everything here is derived from the chains
//! and can be rebuilt; nothing precious lives in this file.
//!
//! Key parts: C = chain number (u16 BE), B = local block number (u32 BE, arrival order,
//! forks included, from 1), P = tx position (u32 BE), T12 = first 12 bytes of a txid
//! (wire order), vout = u32 BE, SH = sha256(scriptPubKey).

use arcade_store::{KeyBuf, Schema, TableDef};

pub const HEADERS: &str = "core.v1.headers"; // C|hash -> HeaderRec
pub const HEIGHT: &str = "core.v1.height"; // C|height -> hash(32) | B(4)
pub const BLOCK_META: &str = "core.v1.block_meta"; // C|B -> BlockMeta
pub const BLOCK_TX: &str = "core.v1.block_tx"; // C|B|P -> raw tx
pub const TXS: &str = "core.v1.txs"; // C|T12 -> {B|P} (dup)
pub const OUTPUTS: &str = "core.v1.outputs"; // C|T12|vout -> value u64 | height u32 | coinbase u8 | script
pub const SPENDS: &str = "core.v1.spends"; // C|T12|vout -> B|P|vin
pub const BY_SCRIPT: &str = "core.v1.by_script"; // C|SH|T12|vout -> value u64 | height u32
pub const SCRIPT_HIST: &str = "core.v1.script_hist"; // C|SH|height|T12 -> received u64 | spent u64
pub const UNDO: &str = "core.v1.undo"; // C|B -> undo record
pub const STATE: &str = "core.v1.state"; // C|name -> value
pub const MEMPOOL: &str = "core.v1.mempool"; // C|T12 -> first_seen u64 LE | raw tx
pub const MEMPOOL_SPENDS: &str = "core.v1.mempool_spends"; // C|T12|vout -> spending txid (32)

/// Undo log refers to tables by index into this list. Never reorder: append only.
pub static LOGGED: &[&str] =
    &[HEADERS, HEIGHT, TXS, OUTPUTS, SPENDS, BY_SCRIPT, SCRIPT_HIST, STATE];

pub fn logged_index(table: &str) -> u8 {
    LOGGED.iter().position(|t| *t == table).expect("table is undo-logged") as u8
}

static TABLES: &[TableDef] = &[
    TableDef::plain(HEADERS),
    TableDef::plain(HEIGHT),
    TableDef::plain(BLOCK_META),
    TableDef::plain(BLOCK_TX),
    TableDef::dup(TXS),
    TableDef::plain(OUTPUTS),
    TableDef::plain(SPENDS),
    TableDef::plain(BY_SCRIPT),
    TableDef::plain(SCRIPT_HIST),
    TableDef::plain(UNDO),
    TableDef::plain(STATE),
    TableDef::plain(MEMPOOL),
    TableDef::plain(MEMPOOL_SPENDS),
];

/// v1 -> v2: mempool tables (created automatically before the migration runs).
fn add_mempool(_w: &arcade_store::WriteTxn) -> arcade_store::Result<()> {
    Ok(())
}

static MIGRATIONS: &[arcade_store::Migration] = &[arcade_store::Migration { to: 2, run: add_mempool }];

pub static CHAIN_SCHEMA: Schema = Schema {
    name: "chain",
    version: 2,
    tables: TABLES,
    migrations: MIGRATIONS,
    max_size: 4 << 40, // 4 TiB ceiling; the file grows as needed
};

pub fn k_header(c: u16, hash: &[u8; 32]) -> Vec<u8> {
    KeyBuf::new().u16(c).bytes(hash).done()
}
pub fn k_height(c: u16, h: u32) -> Vec<u8> {
    KeyBuf::new().u16(c).u32(h).done()
}
pub fn k_block(c: u16, b: u32) -> Vec<u8> {
    KeyBuf::new().u16(c).u32(b).done()
}
pub fn k_block_tx(c: u16, b: u32, p: u32) -> Vec<u8> {
    KeyBuf::new().u16(c).u32(b).u32(p).done()
}
pub fn k_txs(c: u16, t12: &[u8; 12]) -> Vec<u8> {
    KeyBuf::new().u16(c).bytes(t12).done()
}
pub fn k_out(c: u16, t12: &[u8; 12], vout: u32) -> Vec<u8> {
    KeyBuf::new().u16(c).bytes(t12).u32(vout).done()
}
pub fn k_by_script(c: u16, sh: &[u8; 32], t12: &[u8; 12], vout: u32) -> Vec<u8> {
    KeyBuf::new().u16(c).bytes(sh).bytes(t12).u32(vout).done()
}
pub fn k_script(c: u16, sh: &[u8; 32]) -> Vec<u8> {
    KeyBuf::new().u16(c).bytes(sh).done()
}
pub fn k_hist(c: u16, sh: &[u8; 32], height: u32, t12: &[u8; 12]) -> Vec<u8> {
    KeyBuf::new().u16(c).bytes(sh).u32(height).bytes(t12).done()
}
pub fn k_state(c: u16, name: &str) -> Vec<u8> {
    KeyBuf::new().u16(c).bytes(name.as_bytes()).done()
}
pub fn k_chain(c: u16) -> Vec<u8> {
    KeyBuf::new().u16(c).done()
}
