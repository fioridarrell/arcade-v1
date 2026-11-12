//! One-transaction trades: a seller's pre-signed leg and the buyer's completion (the original
//! arcade's `funding.build_leg`, `listings.leg_terms` and `build_partial`, byte for byte).
//!
//! **The leg.** Two coins of the seller's in, two outputs out:
//! - input 0 (any coin of theirs) is signed SINGLE|ANYONECANPAY over output 0, the OP_RETURN
//!   naming the swap ("arcd", AnyData, "INSC" 1 5, the leg given, the leg taken);
//! - input 1 is signed SINGLE|ANYONECANPAY over output 1, the seller's payment: both coins back,
//!   plus the price in coins, less a reservation for the network fee (`swap_fee`).
//!
//! Nobody can change what the seller gives or gets, and anybody may add inputs and outputs
//! after these. The seller is the first input (a Class C swap), so the ledger moves the leg
//! given from the seller and the leg taken to the seller, both or neither.
//!
//! **The completion.** The buyer adds: output 2, the fee reservation back to the seller (so the
//! seller nets exactly the price); its own coins, signed ALL; and change back to itself. The buyer
//! pays the price and the real fee.

use crate::legacy::{self, In, Shape};
use arcade_chain::encode::write_varint;
use arcade_chain::{Hash256, OutPoint};

pub const MAGIC: &[u8; 4] = b"INSC";
pub const KIND_SWAP: u8 = 5;
/// A size in bytes per input as signed, and the fixed part, as the original prices them.
pub const BYTES_PER_INPUT: usize = 148;
pub const BYTES_OVERHEAD: usize = 10;
pub const BYTES_PER_CHANGE: usize = 34;
pub const BYTES_PER_SIGOP: usize = 20;
/// A P2PKH script by shape (an owner not known yet is priced exactly).
pub const P2PKH_SHAPE: [u8; 25] = [0x76, 0xa9, 0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x88, 0xac];

/// One side of a swap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Side {
    /// An inscription, its txid as displayed (the order the leg carries it in).
    Inscription([u8; 32]),
    Token { pid: u32, units: u64 },
    Coins(u64),
}

impl Side {
    pub fn encode(&self) -> Vec<u8> {
        match self {
            Side::Inscription(t) => [&[1u8][..], t].concat(),
            Side::Token { pid, units } => [&[2u8][..], &pid.to_be_bytes(), &units.to_be_bytes()].concat(),
            Side::Coins(s) => [&[3u8][..], &s.to_be_bytes()].concat(),
        }
    }

    pub fn decode(raw: &[u8], at: usize) -> Option<(Side, usize)> {
        match *raw.get(at)? {
            1 => Some((Side::Inscription(raw.get(at + 1..at + 33)?.try_into().ok()?), at + 33)),
            2 => {
                let b = raw.get(at + 1..at + 13)?;
                Some((Side::Token { pid: u32::from_be_bytes(b[..4].try_into().ok()?), units: u64::from_be_bytes(b[4..].try_into().ok()?) }, at + 13))
            }
            3 => Some((Side::Coins(u64::from_be_bytes(raw.get(at + 1..at + 9)?.try_into().ok()?)), at + 9)),
            _ => None,
        }
    }

    pub fn coins(&self) -> u64 {
        if let Side::Coins(s) = self { *s } else { 0 }
    }
}

/// The OP_RETURN data of a swap: "arcd", AnyData (version 0, type 200), "INSC" 1 5, give, take.
pub fn swap_data(give: &Side, take: &Side) -> Vec<u8> {
    let mut d = arcade_ledger::extract::MARKER.to_vec();
    d.extend_from_slice(&[0, 0, 0, 200]);
    d.extend_from_slice(MAGIC);
    d.extend_from_slice(&[1, KIND_SWAP]);
    d.extend_from_slice(&give.encode());
    d.extend_from_slice(&take.encode());
    d
}

/// The two sides an OP_RETURN script names, if it is exactly a swap with nothing after.
pub fn swap_of(script: &[u8]) -> Option<(Side, Side)> {
    let pushes = arcade_ledger::script::pushes(script.get(1..)?);
    if script.first() != Some(&0x6a) || pushes.len() != 1 {
        return None;
    }
    let d = pushes[0];
    let head = [&arcade_ledger::extract::MARKER[..], &[0, 0, 0, 200], MAGIC, &[1, KIND_SWAP]].concat();
    if !d.starts_with(&head) {
        return None;
    }
    let (give, at) = Side::decode(d, head.len())?;
    let (take, at) = Side::decode(d, at)?;
    (at == d.len()).then_some((give, take))
}

/// OP_RETURN with one push of `data`; a payload must fit the 83-byte standard script.
pub fn op_return(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut s = vec![0x6a];
    crate::tx::push(&mut s, data);
    if s.len() > 83 {
        return Err(format!("a trade is written in one OP_RETURN of at most 83 bytes, and this one is {}", s.len()));
    }
    Ok(s)
}

/// Sigops as a block counts them (GetSigOpCount, not accurate).
pub fn legacy_sigops(script: &[u8]) -> usize {
    let (mut n, mut i) = (0, 0);
    while i < script.len() {
        let op = script[i] as usize;
        i += 1;
        match op {
            0..=0x4b => i += op,
            0x4c => i += 1 + *script.get(i).unwrap_or(&0) as usize,
            0x4d => i += 2 + script.get(i..i + 2).map(|b| u16::from_le_bytes([b[0], b[1]]) as usize).unwrap_or(0),
            0x4e => i += 4 + script.get(i..i + 4).map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize).unwrap_or(0),
            0xac | 0xad => n += 1,
            0xae | 0xaf => n += 20,
            _ => {}
        }
    }
    n
}

/// Fee rules of a chain: the rate per 1000 virtual bytes, and the soft-dust line (Pepecoin and
/// the Dogecoin family add the dust line again for every spendable output under it).
#[derive(Debug, Clone, Copy)]
pub struct Fees {
    pub per_kb: u64,
    pub dust: u64,
    pub soft_dust: bool,
}

/// What a transaction with `inputs` signed P2PKH inputs and these outputs costs (funding.price).
pub fn price(f: &Fees, inputs: usize, outputs: &[(u64, Vec<u8>)], change: bool) -> u64 {
    let mut raw = BYTES_OVERHEAD + inputs * BYTES_PER_INPUT;
    let mut sigops = 0;
    for (_, s) in outputs {
        let mut v = Vec::new();
        write_varint(&mut v, s.len() as u64);
        raw += 8 + v.len() + s.len();
        sigops += legacy_sigops(s);
    }
    if change {
        raw += BYTES_PER_CHANGE;
        sigops += 1;
    }
    let vsize = raw.max(sigops * BYTES_PER_SIGOP) as u64;
    let soft = if f.soft_dust { outputs.iter().filter(|(v, s)| s.first() != Some(&0x6a) && *v > 0 && *v < f.dust).count() as u64 * f.dust } else { 0 };
    (vsize * f.per_kb).div_ceil(1000) + soft
}

/// What a seller reserves for the finished swap (funding.swap_fee).
pub fn swap_fee(f: &Fees, opret: &[u8]) -> u64 {
    let outs = vec![(0, P2PKH_SHAPE.to_vec()), (0, P2PKH_SHAPE.to_vec()), (0, opret.to_vec())];
    price(f, 3, &outs, false)
}

/// A coin, by where it is and what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Coin {
    pub outpoint: OutPoint,
    pub value: u64,
}

/// A built (unsigned) leg.
#[derive(Debug, Clone)]
pub struct Leg {
    pub shape: Shape,
    pub sighashes: [[u8; 32]; 2],
    /// The fee reserved out of the seller's payment.
    pub fee: u64,
    /// What output 1 pays the seller.
    pub paid: u64,
}

/// Build a seller's leg: `piece` and `coin` (two different coins of the seller's), `price` in
/// coins taken (0 for a token or inscription), the swap's OP_RETURN, the seller's script.
pub fn build(f: &Fees, piece: &Coin, coin: &Coin, price: u64, opret: &[u8], seller: &[u8]) -> Result<Leg, String> {
    if piece.outpoint == coin.outpoint {
        return Err("a leg's two coins must be different coins".into());
    }
    let fee = swap_fee(f, opret);
    let paid = (piece.value + coin.value + price).checked_sub(fee).filter(|p| *p > 0).ok_or("the price and the two coins do not cover the network fee")?;
    let shape = Shape {
        version: 1,
        time: None,
        inputs: vec![In { prevout: piece.outpoint, sequence: u32::MAX }, In { prevout: coin.outpoint, sequence: u32::MAX }],
        outputs: vec![(0, opret.to_vec()), (paid, seller.to_vec())],
        lock_time: 0,
    };
    let sighashes = [legacy::sighash(&shape, 0, seller, legacy::SINGLE_ANYONECANPAY)?, legacy::sighash(&shape, 1, seller, legacy::SINGLE_ANYONECANPAY)?];
    Ok(Leg { shape, sighashes, fee, paid })
}

/// A scriptSig of a signature and a public key.
pub fn script_sig(sig: &[u8], pubkey: &[u8]) -> Vec<u8> {
    let mut s = Vec::new();
    crate::tx::push(&mut s, sig);
    crate::tx::push(&mut s, pubkey);
    s
}

/// The buyer's completion of a leg: the shape to sign (the buyer's inputs from index 2), what
/// it pays the network, and its change (0 when under the dust line, which goes to the fee).
#[derive(Debug, Clone)]
pub struct Completion {
    pub shape: Shape,
    pub fee: u64,
    pub change: u64,
}

/// Complete `leg` with the buyer's coins (smallest that cover it first) paying `price`.
pub fn complete(f: &Fees, leg: &Shape, reserve: u64, price: u64, buyer_coins: &[Coin], buyer: &[u8]) -> Result<Completion, String> {
    if leg.inputs.len() != 2 || leg.outputs.len() != 2 {
        return Err("a leg is two inputs and two outputs".into());
    }
    let mut outputs = leg.outputs.clone();
    if reserve > 0 {
        outputs.push((reserve, leg.outputs[1].1.clone()));
    }
    // The seller's outputs are its two coins plus the price: the coins fund the leg's own
    // payment and the reservation, so the buyer owes the price and the real fee.
    let need_base = price;
    let mut coins: Vec<&Coin> = buyer_coins.iter().collect();
    coins.sort_by_key(|c| c.value);
    // The smallest single coin that covers it, else the smallest ones added up.
    let mut chosen: Vec<&Coin> = Vec::new();
    let one = coins.iter().find(|c| c.value >= need_base + price_of(f, 3, &outputs, true)).copied();
    if let Some(c) = one {
        chosen.push(c);
    } else {
        let mut total = 0;
        for c in &coins {
            chosen.push(c);
            total += c.value;
            if total >= need_base + price_of(f, 2 + chosen.len(), &outputs, true) {
                break;
            }
        }
    }
    let have: u64 = chosen.iter().map(|c| c.value).sum();
    let fee_with_change = price_of(f, 2 + chosen.len(), &outputs, true);
    let fee_without = price_of(f, 2 + chosen.len(), &outputs, false);
    if have < need_base + fee_without {
        return Err(format!("not enough coins: have {have}, need {}", need_base + fee_without));
    }
    let left = have - need_base - fee_with_change.min(have - need_base);
    let (change, fee) = if have >= need_base + fee_with_change && left >= f.dust { (left, fee_with_change) } else { (0, have - need_base) };
    if change > 0 {
        outputs.push((change, buyer.to_vec()));
    }
    let mut inputs = leg.inputs.clone();
    inputs.extend(chosen.iter().map(|c| In { prevout: c.outpoint, sequence: u32::MAX }));
    Ok(Completion { shape: Shape { version: leg.version, time: leg.time, inputs, outputs, lock_time: leg.lock_time }, fee, change })
}

fn price_of(f: &Fees, inputs: usize, outputs: &[(u64, Vec<u8>)], change: bool) -> u64 {
    price(f, inputs, outputs, change)
}

/// The txid of a serialized transaction, as displayed.
pub fn txid_of(raw: &[u8]) -> Hash256 {
    arcade_chain::hash::sha256d(raw)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const PEP: Fees = Fees { per_kb: 1_000_000, dust: 1_000_000, soft_dust: true };

    fn coin(v: &Value) -> Coin {
        let mut t: [u8; 32] = hex::decode(v["txid"].as_str().unwrap()).unwrap().try_into().unwrap();
        t.reverse();
        Coin { outpoint: OutPoint { txid: Hash256(t), vout: v["vout"].as_u64().unwrap() as u32 }, value: v["value"].as_u64().unwrap() }
    }

    #[test]
    fn legs_equal_to_the_original_arcade_byte_for_byte() {
        let v: Value = serde_json::from_str(include_str!("../tests/data/leg_vectors.json")).unwrap();
        let seller = hex::decode(v["script"].as_str().unwrap()).unwrap();
        for (n, c) in v["legs"].as_array().unwrap().iter().enumerate() {
            let give = Side::decode(&hex::decode(c["give"].as_str().unwrap()).unwrap(), 0).unwrap().0;
            let take = Side::decode(&hex::decode(c["take"].as_str().unwrap()).unwrap(), 0).unwrap().0;
            let data = swap_data(&give, &take);
            assert_eq!(hex::encode(&data), c["data"].as_str().unwrap(), "leg {n}: data");
            let opret = op_return(&data).unwrap();
            assert_eq!(hex::encode(&opret), c["opret"].as_str().unwrap(), "leg {n}: script");
            assert_eq!(swap_of(&opret), Some((give.clone(), take.clone())));
            assert_eq!(swap_fee(&PEP, &opret), c["swap_fee"].as_u64().unwrap(), "leg {n}: fee");
            let leg = build(&PEP, &coin(&c["piece"]), &coin(&c["coin"]), take.coins(), &opret, &seller).unwrap();
            assert_eq!(leg.paid, c["paid"].as_u64().unwrap(), "leg {n}: paid");
            assert_eq!(hex::encode(legacy::serialize(&leg.shape, &[])), c["raw"].as_str().unwrap(), "leg {n}: raw");
            let want: Vec<String> = c["sighashes"].as_array().unwrap().iter().map(|h| h.as_str().unwrap().to_string()).collect();
            assert_eq!(leg.sighashes.iter().map(hex::encode).collect::<Vec<_>>(), want, "leg {n}: digests");
        }
        for (n, p) in v["prices"].as_array().unwrap().iter().enumerate() {
            let outs: Vec<(u64, Vec<u8>)> = p["outputs"].as_array().unwrap().iter().map(|o| (o[0].as_u64().unwrap(), hex::decode(o[1].as_str().unwrap()).unwrap())).collect();
            assert_eq!(price(&PEP, p["inputs"].as_u64().unwrap() as usize, &outs, p["change"].as_bool().unwrap()), p["fee"].as_u64().unwrap(), "price {n}");
        }
    }

    #[test]
    fn a_completion_pays_the_seller_exactly_and_the_buyer_the_rest() {
        let opret = op_return(&swap_data(&Side::Inscription([7; 32]), &Side::Coins(5_000_000))).unwrap();
        let seller = P2PKH_SHAPE.to_vec();
        let mut buyer = P2PKH_SHAPE.to_vec();
        buyer[3] = 9;
        let c = |b: u8, v: u64| Coin { outpoint: OutPoint { txid: Hash256([b; 32]), vout: 0 }, value: v };
        let leg = build(&PEP, &c(1, 1_000_000), &c(2, 2_000_000), 5_000_000, &opret, &seller).unwrap();
        let done = complete(&PEP, &leg.shape, leg.fee, 5_000_000, &[c(3, 4_000_000), c(4, 20_000_000)], &buyer).unwrap();
        let s = &done.shape;
        assert_eq!(&s.outputs[..2], &leg.shape.outputs[..], "the leg's outputs stand where it signed them");
        let to_seller: u64 = s.outputs.iter().filter(|o| o.1 == seller).map(|o| o.0).sum();
        assert_eq!(to_seller, 1_000_000 + 2_000_000 + 5_000_000, "the seller nets the price exactly");
        let ins: u64 = 3_000_000 + s.inputs[2..].iter().map(|i| if i.prevout.txid.0[0] == 3 { 4_000_000 } else { 20_000_000 }).sum::<u64>();
        let outs: u64 = s.outputs.iter().map(|o| o.0).sum();
        assert_eq!(ins - outs, done.fee);
        assert!(done.fee >= price(&PEP, s.inputs.len(), &s.outputs[..s.outputs.len() - usize::from(done.change > 0)], done.change > 0));
        // The leg's digests still hold over the finished transaction (SINGLE|ANYONECANPAY).
        for i in 0..2 {
            assert_eq!(legacy::sighash(s, i, &seller, legacy::SINGLE_ANYONECANPAY).unwrap(), leg.sighashes[i]);
        }
        assert!(complete(&PEP, &leg.shape, leg.fee, 5_000_000, &[c(3, 4_000_000)], &buyer).is_err(), "too little: refused");
    }
}
