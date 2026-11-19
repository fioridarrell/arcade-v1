//! Inscribe one small ord body from a test wallet through a test arcade's socket (coins and broadcast).
//!   ord_inscribe <arcaded.sock> <chain key> <content type> <body> [to address]
//! The recovery phrase on stdin, never printed. Prints the commit and reveal txids (the inscription is <reveal>i0).
use arcade_chain::{Hash256, OutPoint};
use arcade_wallet::tx::Utxo;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
fn call(sock: &str, req: Value) -> Value {
    let mut s = arcade_local::UnixStream::connect(sock).unwrap();
    writeln!(s, "{req}").unwrap();
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).unwrap();
    let v: Value = serde_json::from_str(&line).unwrap();
    if v["ok"] != true { panic!("{}: {}", req["cmd"], v["error"]); }
    v["result"].clone()
}
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let p = arcade_chain::params::by_key(&a[2]).unwrap();
    let mut phrase = String::new();
    std::io::stdin().read_to_string(&mut phrase).unwrap();
    let seed = arcade_wallet::keys::seed_from_phrase(phrase.trim()).unwrap();
    let key = arcade_wallet::keys::tag_key(&seed[..], p.bip44_coin_type, 0).unwrap().key;
    let pk = secp256k1::PublicKey::from_secret_key(secp256k1::SECP256K1, &key);
    let h = arcade_ledger::address::hash160(&pk.serialize());
    let own = arcade_ledger::address::p2pkh_script(&h);
    let me = arcade_ledger::address::base58check(p.p2pkh_version, &h);
    let cs = call(&a[1], json!({"cmd": "tx.coins", "chain": p.key, "addresses": [me]}));
    let coins: Vec<Utxo> = cs["addresses"][&me]["confirmed"].as_array().unwrap().iter().map(|c| Utxo {
        outpoint: OutPoint { txid: Hash256::from_hex(c["txid"].as_str().unwrap()).unwrap(), vout: c["vout"].as_u64().unwrap() as u32 }, value: c["value"].as_u64().unwrap(), script: own.clone() }).collect();
    let to = match a.get(5) { Some(t) => arcade_ledger::address::p2pkh_script_for_address(p, t).unwrap(), None => own.clone() };
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as u32;
    let i = arcade_wallet::ordwrite::inscribe(p, &key, &coins, &a[3], a[4].as_bytes(), &to, 1_000_000, 1_000_000, 1_000_000, 1_000_000, now).unwrap();
    call(&a[1], json!({"cmd": "tx.broadcast", "chain": p.key, "hex": hex::encode(&i.commit.raw)}));
    call(&a[1], json!({"cmd": "tx.broadcast", "chain": p.key, "hex": hex::encode(&i.reveal)}));
    println!("commit {} reveal {}", i.commit.txid.to_hex(), i.reveal_txid);
}
