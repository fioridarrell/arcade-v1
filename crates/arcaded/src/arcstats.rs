//! ARCADE storage analytics (Martin 2026-11-18: "all of the sealed data chunks and who holds them, what nodes carry what
//! amount of weight and how the payment allocations are being distributed"): public, read-only views of the ARCADE
//! chain's own state, which every arcade running the chain agrees on (the catalog, the registry, the pot, the coinbases).
//! An arcade that only borrows ARCADE gets them from one that runs it (explorer forwarding, as the ledger reads).
//!
//! - `explorer.arc_storage_summary`: totals.
//! - `explorer.arc_segments {family?, chain?, offset?, limit?}`: each confirmed (sealed) segment and its replicas.
//! - `explorer.arc_holders {offset?, limit?}`: each earner (replica owners and genesis earners): weight, share, bytes,
//!   audits, accrued pay.
//! - `explorer.arc_payouts {blocks?}`: the coinbases of the last blocks, by epoch: what was minted and paid, to whom.
//!
//! Copy floors and short-of-copies lists (FOGSTORE plan) aren't rules yet: `floor` and `short` stay null until they are.

use crate::node::Node;
use arcade_arc::block::Key;
use arcade_arc::chain::Storage;
use arcade_arc::merkle::Hash;
use arcade_arc::params::{self, Net, COIN};
use arcade_arc::registry::{Earner, Registry};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};

type R = Result<Value, String>;

/// Longest page of segments or holders.
const MAX_PAGE: usize = 500;
/// Most blocks `arc_payouts` reads back (one audit period).
const MAX_BLOCKS: u64 = params::AUDIT_PERIOD_BLOCKS;

pub fn family_name(f: u8) -> &'static str {
    match f {
        arcade_arc::segment::FAMILY_BLOCKS => "blocks",
        arcade_arc::segment::FAMILY_ORD => "ord",
        2 => "ledger",
        3 => "profiles",
        4 => "messages",
        5 => "names",
        6 => "tokens",
        7 => "maps",
        8 => "tags",
        arcade_arc::segment::FAMILY_COINS => "coins",
        arcade_arc::segment::FAMILY_BLOBS => "blobs",
        arcade_arc::segment::FAMILY_COLLECTIONS => "collections",
        arcade_arc::segment::FAMILY_DOGEFEED => "dogefeed",
        _ => "other",
    }
}

fn family_of(s: &str) -> Option<u8> {
    (0..=32u8).find(|f| family_name(*f) == s).or_else(|| s.parse().ok())
}

fn hx(h: &[u8]) -> String {
    hex::encode(h)
}

fn not_here() -> String {
    "the ARCADE chain isn't running here (asked of arcades that run it)".into()
}

fn page(req: &Value, default: usize) -> (usize, usize) {
    let offset = req["offset"].as_u64().unwrap_or(0) as usize;
    let limit = (req["limit"].as_u64().unwrap_or(default as u64) as usize).clamp(1, MAX_PAGE);
    (offset, limit)
}

fn address_of(pay: &[u8; 21]) -> String {
    arcade_ledger::address::base58check(pay[0], &pay[1..])
}

/// The node id an owner key is (an arcade's mesh identity) and its @tag on the mesh, when known.
fn who(owner: &Key) -> Value {
    let id = hx(owner);
    json!({"owner": id, "node": &id[..16], "tag": crate::meshview::tag_of(&id)})
}

/// Per segment: its registered replicas (replica id, owner, missed audits in a row, draw-from height, proof height).
fn replicas_by_segment(reg: &Registry) -> HashMap<Hash, Vec<(Hash, Key, u32, Option<u64>, u64)>> {
    let mut out: HashMap<Hash, Vec<_>> = HashMap::new();
    for (id, seg, owner, _w) in reg.all_replicas() {
        let missed = reg.replica_audit(&id).map_or(0, |a| a.1);
        let height = reg.replica(&id).map_or(0, |r| r.2);
        out.entry(seg).or_default().push((id, owner, missed, reg.draw_from(&id), height));
    }
    out
}

/// Every confirmed segment of every chain and family the catalog may list (the chains V1 knows and ARCADE itself).
fn all_listed(reg: &Registry) -> Vec<&arcade_arc::catalog::Listed> {
    let cat = reg.catalog();
    let mut keys: Vec<&str> = crate::chains::all().map(|p| p.key).chain(cat.net().listed.iter().copied()).chain(cat.net().listed_v4.iter().copied()).collect();
    if let Some(p) = crate::arcsvc::defined() {
        keys.push(p.key);
    }
    keys.sort();
    keys.dedup();
    let mut out = Vec::new();
    for k in keys {
        for f in arcade_arc::segment::FAMILIES {
            out.extend(cat.spans(k, *f).filter_map(|s| s.2));
        }
    }
    out
}

/// Genesis bytes count as weight (paid to the genesis earners) without replicas.
fn genesis_bytes(reg: &Registry) -> u128 {
    reg.genesis_earners().iter().map(|g| g.1 as u128).sum()
}

/// `explorer.arc_storage_summary`.
pub fn summary(_node: &Node, _req: &Value) -> R {
    crate::arcsvc::with_chain(|net, chain| {
        let reg = chain.storage();
        let tip = chain.tip();
        let cat = reg.catalog();
        let reps = replicas_by_segment(reg);
        let mut fams: BTreeMap<String, (u64, u64, u64, u64)> = BTreeMap::new(); // segments, bytes, replicas, unheld
        for l in all_listed(reg) {
            let n = reps.get(&l.id).map_or(0, |v| v.len() as u64);
            let f = fams.entry(format!("{}:{}", l.entry.chain, family_name(l.entry.family))).or_default();
            f.0 += 1;
            f.1 += l.entry.bytes;
            f.2 += n;
            f.3 += (n == 0) as u64;
        }
        let (segments, bytes) = cat.totals();
        let replicas: u64 = reps.values().map(|v| v.len() as u64).sum();
        let block_secs = observed_block_secs(chain, net);
        let blocks_month = (30.0 * 86400.0 / block_secs).round() as u64;
        let mut by_class: BTreeMap<&str, (u64, u64, u64)> = BTreeMap::new(); // blobs, bytes known here, plain here
        for (id, m) in crate::arcblob::known() {
            let name = match m.class { 0 => "permanent", 1 => "release", 2 => "temporary", 3 => "boost", _ => "other" };
            let size = std::fs::metadata(crate::arcblob::dir().join(hex::encode(id))).map(|x| x.len()).unwrap_or(0);
            let e = by_class.entry(name).or_default();
            e.0 += 1;
            e.1 += size;
            e.2 += crate::arcblob::plain_here(&id) as u64;
        }
        let period = net.audit_period;
        let families: Vec<Value> = fams.iter().map(|(k, f)| {
            let (c, fam) = k.split_once(':').unwrap_or(("", k));
            json!({"chain": c, "family": fam, "segments": f.0, "bytes": f.1, "replicas": f.2, "unheld": f.3})
        }).collect();
        let blobs_here: Vec<Value> = by_class.iter().map(|(k, v)| json!({"class": k, "blobs": v.0, "bytes": v.1, "plain": v.2})).collect();
        let (cache_files, cache_bytes) = crate::arcblob::cache_use();
        let unheld: u64 = fams.values().map(|f| f.3).sum();
        let average = if segments > 0 { replicas as f64 / segments as f64 } else { 0.0 };
        json!({
            "chain": chain_key(),
            "height": tip.height,
            "time": tip.time,
            "segments": segments,
            "segment_bytes": bytes,
            "replicas": replicas,
            "average_copies": average,
            "unheld_segments": unheld,
            "families": families,
            "owners": reg.owners().len(),
            "genesis_earners": reg.genesis_earners().len(),
            "total_weight": reg.total_weight().to_string(),
            "drawing_weight": reg.drawing_weight().to_string(),
            "genesis_weight": genesis_bytes(reg).to_string(),
            "pot": reg.pot(),
            "waiting_segments": cat.all_waiting().count(),
            "audit_period": period,
            "next_period_at": tip.height - tip.height % period + period,
            "epoch_blocks": params::EPOCH_BLOCKS,
            "block_seconds_target": net.block_seconds,
            "block_seconds_observed": block_secs,
            "block_reward": params::BLOCK_REWARD,
            "writer_reward": params::writer_reward(net, tip.height + 1),
            "issuance_per_month": blocks_month * params::BLOCK_REWARD,
            "min_payout": params::MIN_PAYOUT,
            "blob_fee_per_kb": arcade_arc::blob::BLOB_FEE_PER_KB,
            "blobs_here": blobs_here,
            "viewer_cache": {"files": cache_files, "bytes": cache_bytes, "limit": crate::arcblob::cache_limit()},
            // FOGSTORE: per-class copy floors and the short-of-copies count, once they are rules.
            "floors": Value::Null,
            "short": Value::Null,
        })
    })
    .ok_or_else(not_here)
}

fn chain_key() -> Option<&'static str> {
    crate::arcsvc::defined().map(|p| p.key)
}

fn observed_block_secs(chain: &crate::arcsvc::LocalChain, net: &Net) -> f64 {
    let tip = chain.tip();
    let back = tip.height.min(100);
    match chain.id_at(tip.height - back).and_then(|id| chain.entry(&id).map(|e| e.time)) {
        Some(t0) if back > 0 && tip.time > t0 => (tip.time - t0) as f64 / back as f64,
        _ => net.block_seconds as f64,
    }
}

/// `explorer.arc_segments {family?, chain?, offset?, limit?}`: newest first.
pub fn segments(_node: &Node, req: &Value) -> R {
    let fam = match req["family"].as_str() {
        Some(s) => Some(family_of(s).ok_or_else(|| format!("unknown family {s}"))?),
        None => req["family"].as_u64().map(|f| f as u8),
    };
    let src = req["source"].as_str().map(String::from);
    let (offset, limit) = page(req, 100);
    crate::arcsvc::with_chain(|_net, chain| segments_of(chain.storage(), chain.tip().height, fam, src.as_deref(), offset, limit)).ok_or_else(not_here)
}

fn segments_of(reg: &Registry, tip: u64, fam: Option<u8>, src: Option<&str>, offset: usize, limit: usize) -> Value {
    {
        let reps = replicas_by_segment(reg);
        let mut all: Vec<_> = all_listed(reg).into_iter().filter(|l| fam.is_none_or(|f| l.entry.family == f) && src.is_none_or(|c| l.entry.chain == c)).collect();
        all.sort_by(|a, b| b.height.cmp(&a.height).then(a.entry.first.cmp(&b.entry.first)));
        let total = all.len();
        let rows: Vec<Value> = all.into_iter().skip(offset).take(limit).map(|l| {
            let held = reps.get(&l.id).cloned().unwrap_or_default();
            let n = held.len() as u64;
            json!({
                "id": hx(&l.id),
                "chain": l.entry.chain,
                "family": family_name(l.entry.family),
                "first": l.entry.first,
                "last": l.entry.last,
                "bytes": l.entry.bytes,
                "root": hx(&l.entry.root),
                "poster": who(&l.entry.poster),
                "confirmed_at": l.height,
                "settled": l.settled(tip),
                "replicas": n,
                "weight_each": if n > 0 { l.entry.bytes / n } else { 0 },
                "holders": held.iter().map(|(id, owner, missed, from, h)| { let mut w = who(owner);
                    w["replica"] = json!(hx(id)); w["missed"] = json!(missed); w["counts_from"] = json!(from); w["registered_at"] = json!(h); w }).collect::<Vec<_>>(),
                "floor": Value::Null,
                "short": Value::Null,
            })
        }).collect();
        json!({"segments": rows, "total": total, "offset": offset})
    }
}

/// `explorer.arc_holders {offset?, limit?}`: every earner, by pay weight.
pub fn holders(_node: &Node, req: &Value) -> R {
    let (offset, limit) = page(req, 100);
    crate::arcsvc::with_chain(|net, chain| holders_of(net, chain.storage(), chain.tip().height, offset, limit)).ok_or_else(not_here)
}

fn holders_of(net: &Net, reg: &Registry, tip: u64, offset: usize, limit: usize) -> Value {
    {
        let total = reg.total_weight().max(1) as f64;
        let window = params::CHALLENGE_DELAY + 2 * params::ANSWER_WINDOW;
        let mut rows: Vec<Value> = Vec::new();
        for o in reg.owners() {
            let reps = reg.replicas_of(&o);
            let picks = reg.audits_of(&o);
            let passed = picks.iter().filter(|p| p.passed).count();
            let missed = picks.iter().filter(|p| !p.passed && tip > p.at + window).count();
            let pay_weight = reg.owner_pay_weight(&o);
            let mut w = who(&o);
            let turn = reg.turn_of(&o);
            let (served, held) = (reg.verdicts_of(&o), reg.held_of(&o));
            let serving = json!({"served": served.0, "not_served": served.1, "held": held.0, "held_times": held.1});
            let next_turn = turn.map(|t| {
                let r = tip % net.audit_period;
                tip - r + if t > r { t } else { net.audit_period + t }
            });
            let audits = json!({"passed": passed, "missed": missed, "running": picks.len() - passed - missed});
            let more = json!({
                "kind": "owner",
                "address": reg.payout_of(&o).map(|p| address_of(&p)),
                "replicas": reps.len(),
                "bytes": reps.iter().map(|r| r.0).sum::<u64>(),
                "missing_audits": reps.iter().filter(|r| r.1 > 0).count(),
                "pay_weight": pay_weight,
                "draw_weight": reg.weight(&o),
                "share": pay_weight as f64 / total,
                "audits": audits,
                "accrued": reg.accrued_of(&Earner::Owner(o)),
                "serving": serving,
                "turn": turn,
                "next_turn": next_turn,
                "punished": reg.punished(&o),
                "silenced_until": reg.silenced_until(&o),
            });
            if let (Some(w), Some(m)) = (w.as_object_mut(), more.as_object()) {
                w.extend(m.clone());
            }
            rows.push(w);
        }
        for (pay, weight, keys, accrued) in reg.genesis_earners() {
            rows.push(json!({
                "kind": "genesis",
                "address": address_of(&pay),
                "keys": keys.iter().map(|k| who(k)).collect::<Vec<_>>(),
                "pay_weight": weight,
                "draw_weight": keys.iter().map(|k| reg.weight(k)).sum::<u64>(),
                "share": weight as f64 / total,
                "accrued": accrued,
            }));
        }
        rows.sort_by(|a, b| b["pay_weight"].as_u64().cmp(&a["pay_weight"].as_u64()));
        let n = rows.len();
        json!({"holders": rows.into_iter().skip(offset).take(limit).collect::<Vec<_>>(), "total": n, "offset": offset,
               "total_weight": reg.total_weight().to_string()})
    }
}

/// `explorer.arc_payouts {blocks?}`: the last `blocks` (default 300, at most one audit period) coinbases, grouped by
/// epoch: minted, fees, the writer's share and what went to each earner (named by owner or genesis address).
pub fn payouts(_node: &Node, req: &Value) -> R {
    let want = req["blocks"].as_u64().unwrap_or(300).clamp(1, MAX_BLOCKS);
    crate::arcsvc::with_chain(|net, chain| {
        let reg = chain.storage();
        let tip = chain.tip().height;
        // Who a payout script is.
        let mut names: HashMap<Vec<u8>, Value> = HashMap::new();
        for o in reg.owners() {
            if let Some(s) = reg.payout_of(&o).and_then(|p| arcade_arc::script::pay_to(net, &p)) {
                let mut w = who(&o);
                w["address"] = json!(reg.payout_of(&o).map(|p| address_of(&p)));
                w["kind"] = json!("owner");
                names.insert(s, w);
            }
        }
        for (pay, ..) in reg.genesis_earners() {
            if let Some(s) = arcade_arc::script::pay_to(net, &pay) {
                names.entry(s).or_insert_with(|| json!({"kind": "genesis", "address": address_of(&pay)}));
            }
        }
        let mut epochs: BTreeMap<u64, Value> = BTreeMap::new();
        let mut totals: HashMap<String, (Value, u64, u64)> = HashMap::new(); // label -> (who, earned, payouts)
        let from = tip.saturating_sub(want - 1).max(1);
        for h in from..=tip {
            let Some(id) = chain.id_at(h) else { continue };
            let Some(block) = chain.block(&id) else { continue };
            let Some(cb) = block.txs.first() else { continue };
            let writer_pay = reg.writer_payout(&block.header.writer);
            let wshare = params::writer_reward(net, h);
            let e = epochs.entry(h / params::EPOCH_BLOCKS).or_insert_with(|| json!({"epoch": h / params::EPOCH_BLOCKS, "first": h, "last": h,
                "blocks": 0, "minted": 0, "coinbase": 0, "writer": 0, "paid": 0, "payouts": 0, "to": {}}));
            e["last"] = json!(h);
            e["blocks"] = json!(e["blocks"].as_u64().unwrap() + 1);
            e["minted"] = json!(e["minted"].as_u64().unwrap() + params::BLOCK_REWARD);
            let mut writer_taken = false;
            for out in &cb.outputs {
                e["coinbase"] = json!(e["coinbase"].as_u64().unwrap() + out.value);
                // The writer's own share is the last output to its payout (chain.rs payouts_at pushes it last).
                if !writer_taken && wshare > 0 && out.value == wshare && writer_pay.as_ref() == Some(&out.script_pubkey) && out.script_pubkey == cb.outputs.last().map(|o| o.script_pubkey.clone()).unwrap_or_default() {
                    writer_taken = true;
                    e["writer"] = json!(e["writer"].as_u64().unwrap() + out.value);
                    continue;
                }
                e["paid"] = json!(e["paid"].as_u64().unwrap() + out.value);
                e["payouts"] = json!(e["payouts"].as_u64().unwrap() + 1);
                let who = names.get(&out.script_pubkey).cloned().unwrap_or_else(|| json!({"kind": "unknown", "script": hx(&out.script_pubkey)}));
                let label = who["address"].as_str().map(String::from).unwrap_or_else(|| hx(&out.script_pubkey));
                let to = &mut e["to"][&label];
                *to = json!(to.as_u64().unwrap_or(0) + out.value);
                let t = totals.entry(label).or_insert((who, 0, 0));
                t.1 += out.value;
                t.2 += 1;
            }
        }
        let mut earners: Vec<Value> = totals.into_values().map(|(mut w, sats, n)| { w["paid"] = json!(sats); w["payouts"] = json!(n); w }).collect();
        earners.sort_by(|a, b| b["paid"].as_u64().cmp(&a["paid"].as_u64()));
        let list: Vec<Value> = epochs.into_values().rev().collect();
        json!({"from": from, "to": tip, "epochs": list, "earners": earners, "pot": reg.pot(), "min_payout": params::MIN_PAYOUT,
               "coin": COIN, "note": "coinbase = minted + fees; writer = the block writer's own share; paid = released to earners"})
    })
    .ok_or_else(not_here)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn family_names_round_trip() {
        for f in arcade_arc::segment::FAMILIES {
            assert_eq!(family_of(family_name(*f)), Some(*f));
        }
        assert_eq!(family_of("blobs"), Some(arcade_arc::segment::FAMILY_BLOBS));
        assert_eq!(family_of("10"), Some(10));
        assert_eq!(family_of("nope"), None);
    }

    #[test]
    fn pages_are_bounded() {
        assert_eq!(page(&json!({}), 100), (0, 100));
        assert_eq!(page(&json!({"offset": 5, "limit": 100000}), 100), (5, MAX_PAGE));
        assert_eq!(page(&json!({"limit": 0}), 100), (0, 1));
    }

    /// A registry with segments listed at genesis and owners holding replicas of them (Registry::from_list): the
    /// segment and holder views show every segment, its holders and their weights, and the shares add up.
    #[test]
    fn segments_and_holders_from_a_genesis_list() {
        use arcade_arc::block::CatalogEntry;
        use arcade_arc::genesis::{FinalList, ListedOwner};
        use arcade_arc::params::{RANGE, TESTNET};
        use ed25519_dalek::SigningKey;
        let net = TESTNET;
        let full = 1_000;
        let k = |i: u8| SigningKey::from_bytes(&[i; 32]);
        let entry = |who: &SigningKey, chain: &str, i: u64, root: u8| {
            let mut e = CatalogEntry { chain: chain.into(), family: arcade_arc::segment::FAMILY_BLOCKS, first: i * RANGE, last: (i + 1) * RANGE - 1,
                bytes: RANGE * arcade_arc::segment::ROW + full + 500 * root as u64, manifest: [root; 32], root: [root; 32], poster: who.verifying_key().to_bytes(), sig: [0; 64] };
            e.sig = arcade_arc::block::sign(who, &e.message(&net));
            e
        };
        let entries = vec![entry(&k(1), "doge", 0, 1), entry(&k(1), "doge", 1, 2), entry(&k(2), "peptest", 0, 3)];
        let seg = |e: &CatalogEntry| e.segment(&net);
        let pay = |i: u8| { let mut p = [0u8; 21]; p[0] = net.p2pkh_version; p[1] = i; p };
        let owners = vec![
            ListedOwner { owner: k(1).verifying_key().to_bytes(), payout: pay(1), replicas: vec![(seg(&entries[0]), [9; 32]), (seg(&entries[1]), [8; 32])] },
            ListedOwner { owner: k(2).verifying_key().to_bytes(), payout: pay(2), replicas: vec![(seg(&entries[0]), [7; 32])] },
        ];
        let holders = vec![(k(3).verifying_key().to_bytes(), 1u64 << 20, pay(3))];
        let reg = Registry::from_list(&net, &holders, &FinalList { entries: entries.clone(), owners }, full).expect("genesis list");
        let v = segments_of(&reg, 0, None, None, 0, 100);
        assert_eq!(v["total"], 3);
        let by_id = |e: &CatalogEntry| v["segments"].as_array().unwrap().iter().find(|x| x["id"] == hx(&seg(e))).unwrap().clone();
        assert_eq!(by_id(&entries[0])["replicas"], 2, "doge 0 is held twice");
        assert_eq!(by_id(&entries[1])["replicas"], 1);
        assert_eq!(by_id(&entries[2])["replicas"], 0, "peptest 0 is held by nobody");
        assert_eq!(by_id(&entries[0])["weight_each"], entries[0].bytes / 2);
        assert_eq!(by_id(&entries[0])["family"], "blocks");
        assert_eq!(segments_of(&reg, 0, None, Some("peptest"), 0, 100)["total"], 1);
        assert_eq!(segments_of(&reg, 0, Some(arcade_arc::segment::FAMILY_BLOBS), None, 0, 100)["total"], 0);
        assert_eq!(segments_of(&reg, 0, None, None, 1, 1)["segments"].as_array().unwrap().len(), 1, "paged");
        let h = holders_of(&net, &reg, 0, 0, 100);
        let rows = h["holders"].as_array().unwrap();
        assert_eq!(rows.len(), 3, "two owners and one genesis earner");
        let share: f64 = rows.iter().map(|r| r["share"].as_f64().unwrap()).sum();
        assert!((share - 1.0).abs() < 1e-9, "shares add to 1: {share}");
        let one = rows.iter().find(|r| r["owner"] == hx(&k(1).verifying_key().to_bytes())).unwrap();
        assert_eq!(one["replicas"], 2);
        assert_eq!(one["pay_weight"], entries[0].bytes / 2 + entries[1].bytes);
        assert!(rows.iter().any(|r| r["kind"] == "genesis" && r["pay_weight"] == 1u64 << 20));
        assert!(rows.windows(2).all(|w| w[0]["pay_weight"].as_u64() >= w[1]["pay_weight"].as_u64()), "by weight");
    }

    #[test]
    fn reads_say_where_to_ask_without_a_chain() {
        // No ARCADE chain runs in a unit test: each read refuses so the explorer forwards it to an arcade that runs it.
        if crate::arcsvc::svc().is_some() {
            return; // another test runs a chain in this process
        }
        let t = crate::sitesvc::tests::setup();
        for f in [summary, segments, holders, payouts] {
            assert!(f(&t.node, &json!({})).unwrap_err().contains("isn't running here"));
        }
    }
}
