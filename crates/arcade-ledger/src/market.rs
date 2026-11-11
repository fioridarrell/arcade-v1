//! Market reads over the ledger's indexed tables (docs/MARKET.md, build step 1): the coin book of one
//! token, a two-token pair's book, the trades a pair has made, and the markets that exist. The same
//! answers as the original's ledger.py `book`, `pair_book`, `pair_trades`, `book_pairs` and
//! `token_pairs`: prices are compared as whole-number cross products, never as floats, so two arcades
//! sort a book the same way; at one price the order placed first comes first, and an order still in
//! the mempool comes after every mined order at its price.

use crate::engine::{BOOK, ROWS, TAKE};
use arcade_store::KeyBuf;
use serde_json::{json, Value};
use std::cmp::Ordering;

/// One order on the book, mined (from `ledger.v1.book_order`) or read from the mempool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Order {
    pub txid: String,
    pub block_height: u32,
    pub position: u32,
    pub address: String,
    pub sale_property: u32,
    /// What is left of it.
    pub sale_amount: u64,
    pub want_property: u32,
    /// For what is left, at the same price.
    pub want_amount: u64,
    pub reserved: u64,
    pub pending: bool,
}

impl Order {
    pub fn from_row(v: &Value) -> Option<Order> {
        Some(Order {
            txid: v["txid"].as_str()?.to_string(),
            block_height: v["block_height"].as_u64()? as u32,
            position: v["position"].as_u64().unwrap_or(0) as u32,
            address: v["address"].as_str()?.to_string(),
            sale_property: v["sale_property"].as_u64()? as u32,
            sale_amount: v["sale_amount"].as_u64()?,
            want_property: v["want_property"].as_u64()? as u32,
            want_amount: v["want_amount"].as_u64()?,
            reserved: v["reserved"].as_u64().unwrap_or(0),
            pending: false,
        })
    }

    /// (mined first, then height, then place in the block): the time half of price-time priority.
    fn when(&self) -> (u8, u32, u32) {
        if self.pending {
            (1, 0, 0)
        } else {
            (0, self.block_height, self.position)
        }
    }
}

/// A price as a fraction `num / den` of raw units, compared by cross-multiplying.
#[derive(Clone, Copy, Debug)]
pub struct Price {
    pub num: u64,
    pub den: u64,
}

impl Price {
    pub fn cmp(&self, o: &Price) -> Ordering {
        (self.num as u128 * o.den as u128).cmp(&(o.num as u128 * self.den as u128))
    }
    /// For a reader: the price as a decimal of raw units (display only, never sorted on).
    pub fn f64(&self) -> f64 {
        if self.den == 0 {
            0.0
        } else {
            self.num as f64 / self.den as f64
        }
    }
}

/// One row of a book: the order and its amounts in the book's orientation.
#[derive(Clone, Debug)]
pub struct Row {
    pub order: Order,
    /// Coin book: tokens; pair book: BASE units.
    pub base: u64,
    /// Coin book: coins; pair book: QUOTE units.
    pub quote: u64,
    /// QUOTE (or coin) per BASE (or token), raw units.
    pub price: Price,
    /// A pending pair order that crosses the mined book: the engine will trade it when its block
    /// lands rather than let it rest (pair books only).
    pub crosses: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Book {
    /// Cheapest first.
    pub asks: Vec<Row>,
    /// Dearest first.
    pub bids: Vec<Row>,
}

fn sort_book(b: &mut Book) {
    b.asks.sort_by(|x, y| x.price.cmp(&y.price).then(x.order.when().cmp(&y.order.when())));
    b.bids.sort_by(|x, y| y.price.cmp(&x.price).then(x.order.when().cmp(&y.order.when())));
}

/// Every order on the book, mined.
pub fn book_orders(db: &arcade_chaindb::ChainDb, c: u16) -> Vec<Order> {
    db.store(c)
        .read(|r| r.scan_prefix(BOOK, &KeyBuf::new().u16(c).done()))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_slice::<Value>(&v).ok())
        .filter_map(|v| Order::from_row(&v))
        .collect()
}

/// The mempool as the book reads it: orders not mined yet, and cancels.
#[derive(Clone, Debug, Default)]
pub struct Pool {
    pub orders: Vec<Order>,
    pub cancels: Vec<PoolCancel>,
    /// Order txid -> token units a take in the mempool is buying of it.
    pub taking: std::collections::HashMap<String, u64>,
}

#[derive(Clone, Debug)]
pub enum CancelKind {
    Price { sale_amount: u64, want_amount: u64 },
    Pair,
    Ecosystem,
}

#[derive(Clone, Debug)]
pub struct PoolCancel {
    pub address: String,
    pub sale_property: u32,
    pub want_property: u32,
    pub kind: CancelKind,
}

/// The txids a pool cancel takes off the book, mined or pending (ledger.py pending_orders): a price
/// somebody has withdrawn is not a price.
pub fn cancelled(mined: &[Order], pool: &Pool) -> std::collections::HashSet<String> {
    let mut gone = std::collections::HashSet::new();
    for c in &pool.cancels {
        for o in mined.iter().chain(pool.orders.iter()) {
            if o.address != c.address {
                continue;
            }
            match c.kind {
                CancelKind::Ecosystem => {
                    gone.insert(o.txid.clone());
                }
                _ if o.sale_property != c.sale_property || o.want_property != c.want_property => {}
                CancelKind::Price { sale_amount, want_amount } => {
                    if want_amount as u128 * o.sale_amount as u128 == sale_amount as u128 * o.want_amount as u128 {
                        gone.insert(o.txid.clone());
                    }
                }
                CancelKind::Pair => {
                    gone.insert(o.txid.clone());
                }
            }
        }
    }
    gone
}

/// The orders a book is drawn from: mined ones not cancelled in the pool, then the pool's own.
pub fn standing(mined: Vec<Order>, pool: Option<&Pool>) -> Vec<Order> {
    let Some(pool) = pool else { return mined };
    let gone = cancelled(&mined, pool);
    mined
        .into_iter()
        .chain(pool.orders.iter().cloned())
        .filter(|o| !gone.contains(&o.txid))
        .collect()
}

/// One token's book against the chain's coin (ledger.py `book` / `_coin_book`): an ask sells the token
/// for the coin, a bid sells the coin for the token. Each row's `base` is tokens and `quote` coins.
pub fn coin_book(orders: &[Order], pid: u32, limit: usize) -> Book {
    let mut b = Book::default();
    for o in orders {
        let ask = o.sale_property == pid && o.want_property == 0;
        let bid = o.want_property == pid && o.sale_property == 0;
        if !ask && !bid {
            continue;
        }
        let (tokens, coins) = if ask { (o.sale_amount, o.want_amount) } else { (o.want_amount, o.sale_amount) };
        if tokens == 0 {
            continue;
        }
        let row = Row { order: o.clone(), base: tokens, quote: coins, price: Price { num: coins, den: tokens }, crosses: false };
        if ask {
            b.asks.push(row)
        } else {
            b.bids.push(row)
        }
    }
    sort_book(&mut b);
    b.asks.truncate(limit);
    b.bids.truncate(limit);
    b
}

/// BASE/QUOTE's book (ledger.py `pair_book`): an ask sells BASE for QUOTE, a bid sells QUOTE for BASE.
/// A pending order that crosses the best mined price on the other side is marked `crosses`.
pub fn pair_book(orders: &[Order], base: u32, quote: u32, limit: usize) -> Book {
    let mut b = Book::default();
    for o in orders {
        let ask = o.sale_property == base && o.want_property == quote;
        let bid = o.sale_property == quote && o.want_property == base;
        if !ask && !bid {
            continue;
        }
        let (bu, qu) = if ask { (o.sale_amount, o.want_amount) } else { (o.want_amount, o.sale_amount) };
        if bu == 0 {
            continue;
        }
        let row = Row { order: o.clone(), base: bu, quote: qu, price: Price { num: qu, den: bu }, crosses: false };
        if ask {
            b.asks.push(row)
        } else {
            b.bids.push(row)
        }
    }
    sort_book(&mut b);
    let best_ask = b.asks.iter().find(|r| !r.order.pending).map(|r| r.price);
    let best_bid = b.bids.iter().find(|r| !r.order.pending).map(|r| r.price);
    for r in &mut b.asks {
        r.crosses = r.order.pending && best_bid.is_some_and(|bb| bb.cmp(&r.price) != Ordering::Less);
    }
    for r in &mut b.bids {
        r.crosses = r.order.pending && best_ask.is_some_and(|ba| ba.cmp(&r.price) != Ordering::Greater);
    }
    b.asks.truncate(limit);
    b.bids.truncate(limit);
    b
}

/// Every token with an order standing against the coin (ledger.py `book_pairs`), ascending.
pub fn coin_markets(orders: &[Order]) -> Vec<u32> {
    let mut v: Vec<u32> = orders
        .iter()
        .filter(|o| o.sale_property == 0 || o.want_property == 0)
        .map(|o| if o.sale_property == 0 { o.want_property } else { o.sale_property })
        .filter(|p| *p != 0)
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// One match the engine made between two token orders (the `pair_trade` digest record).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairTrade {
    pub height: u32,
    pub txid: String,
    pub seq: u32,
    pub position: u32,
    pub taker: String,
    pub maker: String,
    pub maker_txid: String,
    pub gave_property: u32,
    pub gave: u64,
    pub got_property: u32,
    pub got: u64,
}

/// Parse a `pair_trade|txid|seq|pos|taker|maker|mtx|sale|gave|want|got` record.
pub fn parse_pair_trade(height: u32, line: &str) -> Option<PairTrade> {
    let f: Vec<&str> = line.trim_end().split('|').collect();
    if f.len() != 11 || f[0] != "pair_trade" {
        return None;
    }
    Some(PairTrade {
        height,
        txid: f[1].into(),
        seq: f[2].parse().ok()?,
        position: f[3].parse().ok()?,
        taker: f[4].into(),
        maker: f[5].into(),
        maker_txid: f[6].into(),
        gave_property: f[7].parse().ok()?,
        gave: f[8].parse().ok()?,
        got_property: f[9].parse().ok()?,
        got: f[10].parse().ok()?,
    })
}

/// Every pair trade in the ledger, oldest first (read from the blocks' digest records).
pub fn pair_trades_all(db: &arcade_chaindb::ChainDb, c: u16) -> Vec<PairTrade> {
    let rows = db.store(c).read(|r| r.scan_prefix(ROWS, &KeyBuf::new().u16(c).done())).unwrap_or_default();
    let mut out = Vec::new();
    for (k, v) in rows {
        if k.len() < 6 || !v.windows(11).any(|w| w == b"pair_trade|") {
            continue;
        }
        let h = u32::from_be_bytes(k[k.len() - 4..].try_into().unwrap());
        for line in String::from_utf8_lossy(&v).lines() {
            if let Some(t) = parse_pair_trade(h, line) {
                out.push(t);
            }
        }
    }
    out
}

/// What traded on BASE/QUOTE, newest first: {height, txid, base, quote, side, taker, maker} with `side`
/// "buy" when the taker bought BASE (ledger.py `pair_trades`, book matches).
pub fn pair_trades(all: &[PairTrade], base: u32, quote: u32, limit: usize) -> Vec<Value> {
    let mut v: Vec<&PairTrade> = all
        .iter()
        .filter(|t| (t.gave_property == base && t.got_property == quote) || (t.gave_property == quote && t.got_property == base))
        .collect();
    v.sort_by(|a, b| (b.height, &b.txid).cmp(&(a.height, &a.txid)));
    v.into_iter()
        .take(limit)
        .map(|t| {
            let buy = t.got_property == base;
            json!({"height": t.height, "txid": t.txid, "base": if buy { t.got } else { t.gave },
                   "quote": if buy { t.gave } else { t.got }, "side": if buy { "buy" } else { "sell" },
                   "taker": t.taker, "maker": t.maker, "maker_order": t.maker_txid})
        })
        .collect()
}

/// Every two-token market with an order on it or a trade in it, each once, lower id first
/// (ledger.py `token_pairs`).
pub fn token_markets(orders: &[Order], trades: &[PairTrade]) -> Vec<(u32, u32)> {
    let mut v: Vec<(u32, u32)> = orders
        .iter()
        .filter(|o| o.sale_property != 0 && o.want_property != 0)
        .map(|o| (o.sale_property, o.want_property))
        .chain(trades.iter().map(|t| (t.gave_property, t.got_property)))
        .map(|(a, b)| (a.min(b), a.max(b)))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// A take of a resting coin order (`ledger.v1.take_trade`): {txid, property_id, tokens, coins, maker}.
pub fn takes(db: &arcade_chaindb::ChainDb, c: u16, pid: Option<u32>) -> Vec<Value> {
    db.store(c)
        .read(|r| r.scan_prefix(TAKE, &KeyBuf::new().u16(c).done()))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(_, v)| serde_json::from_slice::<Value>(&v).ok())
        .filter(|v| pid.is_none_or(|p| v["property_id"].as_u64() == Some(p as u64)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn o(txid: &str, h: u32, pos: u32, sale: u32, sa: u64, want: u32, wa: u64) -> Order {
        Order { txid: txid.into(), block_height: h, position: pos, address: format!("addr-{txid}"), sale_property: sale,
                sale_amount: sa, want_property: want, want_amount: wa, reserved: sa, pending: false }
    }

    #[test]
    fn coin_book_sorts_by_price_then_time_and_pending_last() {
        let mut p = o("p", 0, 0, 7, 10, 0, 20);
        p.pending = true;
        let orders = vec![o("b", 5, 1, 7, 10, 0, 20), o("a", 4, 9, 7, 10, 0, 20), o("c", 3, 0, 7, 10, 0, 30), p,
                          o("x", 1, 0, 0, 50, 7, 10), o("y", 2, 0, 0, 60, 7, 10), o("z", 9, 0, 8, 1, 0, 1)];
        let b = coin_book(&orders, 7, 50);
        let asks: Vec<&str> = b.asks.iter().map(|r| r.order.txid.as_str()).collect();
        assert_eq!(asks, ["a", "b", "p", "c"]);
        let bids: Vec<&str> = b.bids.iter().map(|r| r.order.txid.as_str()).collect();
        assert_eq!(bids, ["y", "x"]);
        assert_eq!((b.asks[0].base, b.asks[0].quote), (10, 20));
        assert_eq!((b.bids[0].base, b.bids[0].quote), (10, 60));
    }

    #[test]
    fn prices_never_round() {
        // 1/3 and 333333333/1000000000 differ, and a float sort could tie them.
        let orders = vec![o("third", 2, 0, 7, 3, 0, 1), o("close", 1, 0, 7, 1_000_000_000, 0, 333_333_333)];
        let b = coin_book(&orders, 7, 50);
        assert_eq!(b.asks[0].order.txid, "close");
    }

    #[test]
    fn pair_book_orients_and_marks_crossing_pending_orders() {
        let mut cross = o("cross", 0, 0, 2, 10, 1, 4); // bids 1 BASE... sells QUOTE 2 for BASE 1: a bid at 10/4
        cross.pending = true;
        let orders = vec![o("ask", 1, 0, 1, 4, 2, 8), cross];
        let b = pair_book(&orders, 1, 2, 50);
        assert_eq!((b.asks[0].base, b.asks[0].quote), (4, 8));
        assert_eq!((b.bids[0].base, b.bids[0].quote), (4, 10));
        assert!(b.bids[0].crosses);
        assert!(!b.asks[0].crosses);
    }

    #[test]
    fn pool_cancels_take_orders_off() {
        let mined = vec![o("a", 1, 0, 7, 10, 0, 20), o("b", 1, 1, 7, 10, 0, 30)];
        let pool = Pool {
            orders: vec![],
            cancels: vec![PoolCancel { address: "addr-a".into(), sale_property: 7, want_property: 0,
                                       kind: CancelKind::Price { sale_amount: 1, want_amount: 2 } }],
            taking: Default::default(),
        };
        let left: Vec<String> = standing(mined, Some(&pool)).into_iter().map(|o| o.txid).collect();
        assert_eq!(left, ["b"]);
    }

    #[test]
    fn pair_trade_records_parse_and_orient() {
        let t = parse_pair_trade(9, "pair_trade|tx1|0|3|taker|maker|mtx|2|50|1|5\n").unwrap();
        assert_eq!((t.gave_property, t.gave, t.got_property, t.got), (2, 50, 1, 5));
        let v = pair_trades(&[t], 1, 2, 10);
        assert_eq!(v[0]["side"], "buy");
        assert_eq!((v[0]["base"].as_u64(), v[0]["quote"].as_u64()), (Some(5), Some(50)));
    }
}
