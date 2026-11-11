//! The DigiDollar and DigiByte addresses of a wallet's first tag key on DigiByte testnet (or `dgb`).
//! Reads the recovery phrase from stdin, prints only addresses.
use std::io::Read;
fn main() {
    let key = std::env::args().nth(1).unwrap_or_else(|| "dgbtest".into());
    let p = arcade_chain::params::by_key(&key).expect("chain");
    let mut phrase = String::new();
    std::io::stdin().read_to_string(&mut phrase).unwrap();
    let seed = arcade_wallet::keys::seed_from_phrase(phrase.trim()).expect("phrase");
    let x = arcade_wallet::keys::tag_key(&seed[..], p.bip44_coin_type, 0).expect("key");
    let pk = x.public();
    let q = arcade_wallet::taproot::output_key(&pk).expect("taproot");
    println!("dd {}", arcade_ledger::digidollar::address(&q.serialize(), key == "dgbtest"));
}
