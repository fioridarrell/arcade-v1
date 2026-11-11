//! Market reads (docs/MARKET.md, build step 1): the ledger's order book, its trades and its markets,
//! for app pages and the socket API. Read only; nothing here builds or signs.
//!
//!   GET /r/book/<property id>[?address=]   the original's shape (asks, each with `taking`), plus bids
//!   GET /r/market/markets                  every market with an order on it or a trade in it
//!   GET /r/market/book?base=&quote=        one market's book; a side is a property id, 0 or "coin"
//!   GET /r/market/trades?base=&quote=      what traded, newest first
//!   GET /r/market/candles?base=&quote=&span=&count=
//!
//! The socket API answers `ledger.book`, `ledger.trades` and `ledger.pairs` with the same bodies.
//! The home chain only for now; another chain's ledger comes with the cross-chain steps.

use crate::node::Node;
use arcade_ledger::engine as led;
use arcade_ledger::market::{self as mk, Book, CancelKind, Order, Pool, PoolCancel, Row};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

const DEPTH: usize = 50;
const MAX_DEPTH: usize = 500;

fn c(node: &Node) -> u16 {
    node.params.chain_no
}

fn prop(node: &Node, pid: u32) -> Option<Value> {
    led::property(&node.chain, c(node), pid).ok().flatten()
}

/// The mempool as the book reads it (ledger.py pending_orders, pending_takes): orders not mined yet
/// (checked as the indexer would, minus what only a block can answer), cancels, and takes.
pub fn pool(node: &Node) -> Pool {
    use arcade_ledger::payload::{decode, Msg};
    let token_pairs = led::gates(node.params).token_pairs_from.is_some();
    let mut out = Pool::default();
    for (tx, atx) in crate::claims::pending_arcade(node) {
        let Ok(d) = decode(&atx.payload) else { continue };
        let base = |sale: u32, sa: u64, want: u32, wa: u64| Order {
            txid: tx.txid.to_hex(),
            block_height: 0,
            position: 0,
            address: atx.sender.clone(),
            sale_property: sale,
            sale_amount: sa,
            want_property: want,
            want_amount: wa,
            reserved: 0,
            pending: true,
        };
        match d.msg {
            Msg::Take { amount, order, .. } => {
                *out.taking.entry(hex::encode(order)).or_insert(0) += amount;
            }
            Msg::CancelEcosystem { .. } => out.cancels.push(PoolCancel { address: atx.sender.clone(), sale_property: 0, want_property: 0, kind: CancelKind::Ecosystem }),
            Msg::CancelPair { sale, want } => out.cancels.push(PoolCancel { address: atx.sender.clone(), sale_property: sale, want_property: want, kind: CancelKind::Pair }),
            Msg::CancelPrice { sale, sale_amount, want, want_amount } => out.cancels.push(PoolCancel {
                address: atx.sender.clone(),
                sale_property: sale,
                want_property: want,
                kind: CancelKind::Price { sale_amount, want_amount },
            }),
            Msg::Trade { sale, sale_amount, want, want_amount } => {
                if sale == want {
                    continue;
                }
                if sale != 0 && want != 0 && (!token_pairs || prop(node, sale).is_none() || prop(node, want).is_none()) {
                    continue;
                }
                let range = |n: u64| n > 0 && n < (1u64 << 63);
                if !range(sale_amount) || !range(want_amount) {
                    continue;
                }
                if (sale == 0 || want == 0) && prop(node, sale.max(want)).is_none() {
                    continue;
                }
                out.orders.push(base(sale, sale_amount, want, want_amount));
            }
            _ => {}
        }
    }
    out
}

/// The orders a book is drawn from right now: mined, less what the pool cancels, plus the pool's own.
fn standing(node: &Node, with_pool: bool) -> (Vec<Order>, Pool) {
    let mined = mk::book_orders(&node.chain, c(node));
    let pool = if with_pool { pool(node) } else { Pool::default() };
    (mk::standing(mined, with_pool.then_some(&pool)), pool)
}

/// A market side named in a request: a property id, or the coin (0, "coin", or the chain's ticker).
fn side(node: &Node, v: Option<&Value>) -> Result<u32, String> {
    let v = v.ok_or("give base and quote: a property id, or coin")?;
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).map_err(|_| "no such property".to_string());
    }
    let s = v.as_str().unwrap_or("").trim();
    if s.eq_ignore_ascii_case("coin") || s == "0" || s.eq_ignore_ascii_case(crate::explorer::unit(node.params)) {
        return Ok(0);
    }
    s.parse::<u32>().map_err(|_| format!("{s} is not a property id or coin"))
}

fn asset(node: &Node, pid: u32) -> Value {
    if pid == 0 {
        return json!({"network": node.params.key, "kind": "coin", "asset": crate::explorer::unit(node.params), "property_id": 0,
                      "name": crate::explorer::unit(node.params), "divisible": true});
    }
    let p = prop(node, pid);
    json!({"network": node.params.key, "kind": "token", "asset": pid.to_string(), "property_id": pid,
           "name": p.as_ref().map(|p| p["name"].clone()).unwrap_or(Value::Null),
           "divisible": p.as_ref().map(led::divisible).unwrap_or(false)})
}

fn row_json(r: &Row, taking: &HashMap<String, u64>) -> Value {
    json!({"order": r.order.txid, "maker": r.order.address, "base": r.base, "quote": r.quote,
           "price": {"num": r.price.num, "den": r.price.den, "each": r.price.f64()},
           "pending": r.order.pending, "crosses": r.crosses, "height": r.order.block_height,
           "taking": taking.get(&r.order.txid).copied().unwrap_or(0), "venue": "ledger", "live": true,
           "partial": true})
}

fn book_json(node: &Node, base: u32, quote: u32, b: &Book, pool: &Pool) -> Value {
    let spread = match (b.asks.iter().find(|r| !r.order.pending), b.bids.iter().find(|r| !r.order.pending)) {
        (Some(a), Some(bi)) => json!({"ask": a.price.f64(), "bid": bi.price.f64()}),
        _ => Value::Null,
    };
    json!({"base": asset(node, base), "quote": asset(node, quote),
           "asks": b.asks.iter().map(|r| row_json(r, &pool.taking)).collect::<Vec<_>>(),
           "bids": b.bids.iter().map(|r| row_json(r, &pool.taking)).collect::<Vec<_>>(),
           "spread": spread})
}

/// A block's time by height, cached for one request.
struct Times<'a> {
    node: &'a Node,
    seen: HashMap<u32, Option<u32>>,
}

impl<'a> Times<'a> {
    fn new(node: &'a Node) -> Self {
        Times { node, seen: HashMap::new() }
    }
    fn at(&mut self, h: u32) -> Option<u32> {
        let node = self.node;
        *self.seen.entry(h).or_insert_with(|| {
            let hash = node.chain.hash_at(c(node), h).ok().flatten()?;
            node.chain.header(c(node), &hash).ok().flatten().map(|r| r.header.time)
        })
    }
}

/// The height a mined transaction is in.
fn height_of(node: &Node, txid: &str) -> Option<u32> {
    let id = arcade_chain::hash::Hash256::from_hex(txid).ok()?;
    let (b, _pos) = node.chain.tx_location(node.params, &id).ok().flatten()?;
    node.chain.block_at(node.params, b).ok().flatten().map(|(_, h, _)| h)
}

/// What traded on BASE/QUOTE, newest first: pair matches between token orders, and takes of resting coin
/// orders (a take buys tokens for the coin). {height, time, txid, base, quote, side, taker, maker}.
pub fn trades(node: &Node, base: u32, quote: u32, limit: usize) -> Vec<Value> {
    trades_in(node, &Traded::read(node), base, quote, limit)
}

/// The ledger's trades, read once for a request that draws many markets (the pair records are a scan of
/// every block's digest rows: 0.3 s on Pepecoin testnet).
struct Traded {
    pairs: Vec<mk::PairTrade>,
    takes: Vec<Value>,
}

impl Traded {
    fn read(node: &Node) -> Traded {
        Traded { pairs: mk::pair_trades_all(&node.chain, c(node)), takes: mk::takes(&node.chain, c(node), None) }
    }
}

fn trades_in(node: &Node, all: &Traded, base: u32, quote: u32, limit: usize) -> Vec<Value> {
    let mut t = Times::new(node);
    let mut out: Vec<Value> = if base != 0 && quote != 0 {
        mk::pair_trades(&all.pairs, base, quote, limit)
    } else {
        let pid = base.max(quote);
        let token_is_base = quote == 0;
        all.takes
            .iter()
            .filter(|v| v["property_id"].as_u64() == Some(pid as u64))
            .filter_map(|v| {
                let txid = v["txid"].as_str()?.to_string();
                let (tokens, coins) = (v["tokens"].as_u64()?, v["coins"].as_u64()?);
                Some(json!({"height": height_of(node, &txid), "txid": txid,
                            "base": if token_is_base { tokens } else { coins }, "quote": if token_is_base { coins } else { tokens },
                            // the taker bought the tokens
                            "side": if token_is_base { "buy" } else { "sell" }, "taker": Value::Null, "maker": v["maker"]}))
            })
            .collect()
    };
    out.sort_by(|a, b| (b["height"].as_u64(), b["txid"].as_str()).cmp(&(a["height"].as_u64(), a["txid"].as_str())));
    out.truncate(limit);
    for v in &mut out {
        let time = v["height"].as_u64().and_then(|h| t.at(h as u32));
        v["at"] = json!(time);
        v["id"] = v["txid"].clone();
    }
    out
}

/// Candles of `span` seconds, newest last, `count` of them ending at the newest block: {t, o, h, l, c, v}
/// in QUOTE per BASE (raw units), `v` in BASE units, or null for a span with no trade (gaps kept).
pub fn candles(node: &Node, base: u32, quote: u32, span: u64, count: usize) -> Vec<Value> {
    let span = span.clamp(60, 30 * 86_400);
    let count = count.clamp(1, 1000);
    let now = node.chain.tip(c(node)).ok().flatten().and_then(|t| node.chain.header(c(node), &t.hash).ok().flatten()).map(|h| h.header.time as u64).unwrap_or(0);
    let end = (now / span + 1) * span;
    let start = end.saturating_sub(span * count as u64);
    let mut buckets: Vec<Option<(f64, f64, f64, f64, u64)>> = vec![None; count];
    let mut ts = trades(node, base, quote, 100_000);
    ts.reverse(); // oldest first
    for t in ts {
        let (Some(at), Some(b), Some(q)) = (t["at"].as_u64(), t["base"].as_u64(), t["quote"].as_u64()) else { continue };
        if at < start || at >= end || b == 0 {
            continue;
        }
        let i = ((at - start) / span) as usize;
        let p = q as f64 / b as f64;
        buckets[i] = Some(match buckets[i] {
            None => (p, p, p, p, b),
            Some((o, h, l, _c, v)) => (o, h.max(p), l.min(p), p, v + b),
        });
    }
    buckets
        .into_iter()
        .enumerate()
        .map(|(i, x)| match x {
            Some((o, h, l, cl, v)) => json!({"t": start + i as u64 * span, "o": o, "h": h, "l": l, "c": cl, "v": v}),
            None => Value::Null,
        })
        .collect()
}

/// Every market: each token against the coin with an order on it or a take of it, and each two-token
/// pair with an order or a trade. {base, quote, venue, last, change_24h, volume_24h, asks, bids}.
pub fn markets(node: &Node) -> Vec<Value> {
    let (orders, _pool) = standing(node, true);
    let all = Traded::read(node);
    let mut list: Vec<(u32, u32)> = mk::coin_markets(&orders).into_iter().map(|p| (p, 0)).collect();
    for t in &all.takes {
        if let Some(p) = t["property_id"].as_u64() {
            list.push((p as u32, 0));
        }
    }
    list.extend(mk::token_markets(&orders, &all.pairs));
    list.sort_unstable();
    list.dedup();
    let now = node.chain.tip(c(node)).ok().flatten().and_then(|t| node.chain.header(c(node), &t.hash).ok().flatten()).map(|h| h.header.time as u64).unwrap_or(0);
    list.into_iter()
        .map(|(base, quote)| {
            let b = mk::pair_book(&orders, base, quote, MAX_DEPTH);
            let ts = trades_in(node, &all, base, quote, 1000);
            let price = |t: &Value| -> Option<f64> { Some(t["quote"].as_u64()? as f64 / t["base"].as_u64().filter(|b| *b > 0)? as f64) };
            let last = ts.first().and_then(price);
            let day: Vec<&Value> = ts.iter().filter(|t| t["at"].as_u64().is_some_and(|a| a + 86_400 >= now)).collect();
            let before = ts.iter().find(|t| t["at"].as_u64().is_some_and(|a| a + 86_400 < now)).and_then(price);
            let change = match (last, before) {
                (Some(l), Some(b)) if b > 0.0 => json!((l - b) / b),
                _ => Value::Null,
            };
            json!({"base": asset(node, base), "quote": asset(node, quote), "venue": "ledger", "last": last,
                   "change_24h": change, "volume_24h": day.iter().filter_map(|t| t["base"].as_u64()).sum::<u64>(),
                   "asks": b.asks.len(), "bids": b.bids.len()})
        })
        .collect()
}

/// The original's /r/book/<id> (app.py r_book), with bids beside the asks.
pub fn r_book(node: &Node, pid: u32, address: Option<&str>) -> Option<Value> {
    let p = prop(node, pid)?;
    let (orders, pool) = standing(node, true);
    let b = mk::coin_book(&orders, pid, DEPTH);
    let side = |rows: &[Row]| -> Vec<Value> {
        rows.iter()
            .filter(|r| address.is_none_or(|a| a.is_empty() || r.order.address == a))
            .map(|r| json!({"order": r.order.txid, "address": r.order.address, "tokens": r.base, "coins": r.quote,
                            "pending": r.order.pending, "taking": pool.taking.get(&r.order.txid).copied().unwrap_or(0)}))
            .collect()
    };
    Some(json!({"property_id": pid, "name": p["name"], "divisible": led::divisible(&p), "asks": side(&b.asks), "bids": side(&b.bids)}))
}

fn num(q: &Map<String, Value>, k: &str, d: u64) -> u64 {
    q.get(k).and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))).unwrap_or(d)
}

fn pair_of(node: &Node, q: &Map<String, Value>) -> Result<(u32, u32), String> {
    let (base, quote) = (side(node, q.get("base"))?, side(node, q.get("quote"))?);
    if base == quote {
        return Err("a market is two different assets".into());
    }
    for p in [base, quote] {
        if p != 0 && prop(node, p).is_none() {
            return Err(format!("no such property {p}"));
        }
    }
    Ok((base, quote))
}

/// One read by name, with its arguments (the socket API and /r/market/<name>).
pub fn read(node: &Node, name: &str, q: &Map<String, Value>) -> Result<Value, String> {
    match name {
        "markets" | "pairs" => Ok(json!(markets(node))),
        "book" => {
            let (base, quote) = pair_of(node, q)?;
            let (orders, pool) = standing(node, num(q, "pool", 1) != 0);
            let depth = (num(q, "depth", DEPTH as u64) as usize).clamp(1, MAX_DEPTH);
            Ok(book_json(node, base, quote, &mk::pair_book(&orders, base, quote, depth), &pool))
        }
        "trades" => {
            let (base, quote) = pair_of(node, q)?;
            let limit = num(q, "limit", 100).clamp(1, 5000) as usize;
            let before = q.get("before").and_then(|v| v.as_u64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
            let mut v = trades(node, base, quote, if before.is_some() { 100_000 } else { limit });
            if let Some(b) = before {
                v.retain(|t| t["height"].as_u64().is_some_and(|h| h < b));
                v.truncate(limit);
            }
            Ok(json!(v))
        }
        "candles" => {
            let (base, quote) = pair_of(node, q)?;
            Ok(json!(candles(node, base, quote, num(q, "span", 3600), num(q, "count", 48) as usize)))
        }
        _ => Err(format!("{name} is not a market read")),
    }
}

/// A GET under /r/ this module answers: Some((status, JSON text)), or None for another module's path.
pub fn route(node: &Node, path: &str, q: &Map<String, Value>) -> Option<(u16, String)> {
    let seg: Vec<&str> = path.trim_start_matches("/r/").split('/').collect();
    match seg.as_slice() {
        ["book", id] => Some(match id.parse::<u32>().ok().and_then(|pid| r_book(node, pid, q.get("address").and_then(|v| v.as_str()))) {
            Some(v) => (200, v.to_string()),
            None => (404, json!({"error": "no such token"}).to_string()),
        }),
        ["market", name] => Some(match read(node, name, q) {
            Ok(v) => (200, json!({"ok": true, "result": v}).to_string()),
            Err(e) => (400, json!({"ok": false, "error": e}).to_string()),
        }),
        _ => None,
    }
}

/// The socket API's `ledger.book`, `ledger.trades`, `ledger.pairs`.
pub fn handle(node: &Node, cmd: &str, req: &Value) -> Result<Value, String> {
    let q = req.as_object().cloned().unwrap_or_default();
    match cmd {
        "ledger.book" => read(node, "book", &q),
        "ledger.trades" => read(node, "trades", &q),
        "ledger.pairs" => read(node, "markets", &q),
        _ => Err(format!("unknown command {cmd}")),
    }
}
