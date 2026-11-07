//! Keep the @tag index current: fold in every stored block from the chain's index start,
//! and start over if the chain reorganised under us.

use crate::node::{Node, TagIndex};
use arcade_chain::{Block, Tx};
use arcade_ledger::extract::{class_b_marker, detect_class, extract, Prevout};
use arcade_ledger::tags;

/// Fold blocks up to the chain tip into the tag index. Returns blocks processed.
pub fn catch_up(node: &Node) -> Result<u32, String> {
    let p = node.params;
    let c = p.chain_no;
    let Some(tip) = node.chain.tip(c).map_err(|e| e.to_string())? else { return Ok(0) };
    let marker = class_b_marker(p);
    let mut idx = node.tags.lock().unwrap();

    // Reorg check: is the block we last folded still on the main chain?
    if let Some(h) = idx.hash {
        if node.chain.hash_at(c, idx.height).map_err(|e| e.to_string())? != Some(h) {
            *idx = TagIndex::default();
        }
    }
    let start = if idx.hash.is_some() { idx.height + 1 } else { p.index_start };
    if tip.height < start {
        return Ok(0);
    }
    let mut n = 0;
    for h in start..=tip.height {
        let hash = node.chain.hash_at(c, h).map_err(|e| e.to_string())?.ok_or("gap in chain")?;
        let raw = node.chain.block_raw(c, &hash).map_err(|e| e.to_string())?.ok_or("missing block")?;
        let b = Block::parse_with(&raw, &p.format).map_err(|e| e.to_string())?;
        for (pos, tx) in b.txs.iter().enumerate() {
            if detect_class(tx, &marker, p).is_none() {
                continue;
            }
            let mut prev = Vec::with_capacity(tx.inputs.len());
            for i in &tx.inputs {
                let (raw, _) = node
                    .chain
                    .tx_raw(p, &i.prevout.txid)
                    .map_err(|e| e.to_string())?
                    .ok_or("prevout tx not stored")?;
                let ptx = Tx::parse_with(&raw, &p.format).map_err(|e| e.to_string())?;
                let o = ptx.outputs.get(i.prevout.vout as usize).ok_or("bad prevout index")?;
                prev.push(Prevout { value: o.value, script: o.script_pubkey.clone() });
            }
            let Some(Ok(atx)) = extract(tx, &prev, p) else { continue };
            if let Some(Ok(op)) = tags::parse(&atx.payload) {
                let _ = idx.tags.apply(&op, &atx.sender, atx.reference.as_deref(), &tx.txid.to_hex(), h, pos as u32);
            }
        }
        idx.height = h;
        idx.hash = Some(hash);
        n += 1;
    }
    Ok(n)
}
