//! Find which key-derivation reading matches the live arcade: try variants on its AUTH frame.
use arcade_mesh::link::{blake2b32, hello_body};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use std::io::{Read, Write};
use std::net::TcpStream;

fn raw(s: &mut TcpStream) -> Vec<u8> { let mut l=[0u8;4]; s.read_exact(&mut l).unwrap(); let mut b=vec![0;u32::from_be_bytes(l) as usize]; s.read_exact(&mut b).unwrap(); b }
fn main() {
    let addr = std::env::args().nth(1).unwrap_or("156.67.28.112:8421".into());
    let mut s = TcpStream::connect(&addr).unwrap();
    let sk = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
    let pk = x25519_dalek::PublicKey::from(&sk).to_bytes();
    let my_hello = hello_body("arcade-main", &pk);
    let mut f = (my_hello.len() as u32).to_be_bytes().to_vec(); f.extend_from_slice(&my_hello); s.write_all(&f).unwrap();
    let their_hello = raw(&mut s);
    let their: [u8;32] = their_hello[their_hello.len()-32..].try_into().unwrap();
    let shared = sk.diffie_hellman(&x25519_dalek::PublicKey::from(their)).to_bytes();
    let ct = raw(&mut s);
    println!("their hello {} bytes, first encrypted frame {} bytes", their_hello.len(), ct.len());
    let nonce = |n: u64, le: bool| { let mut b=[0u8;24]; if le { b[..8].copy_from_slice(&n.to_le_bytes()) } else { b[16..].copy_from_slice(&n.to_be_bytes()) }; XNonce::from(b) };
    let cat = |a:&[u8],b:&[u8]| { let mut v=a.to_vec(); v.extend_from_slice(b); v };
    let mut hits = 0;
    for (tname, first, second) in [("eph D-first", &pk[..], &their[..]), ("eph L-first", &their[..], &pk[..]), ("hello D-first", &my_hello[..], &their_hello[..]), ("hello L-first", &their_hello[..], &my_hello[..])] {
        for (pname, padded) in [("person padded", true), ("no person", false)] {
            let transcript = blake2b32(&cat(first, second), &[]);
            for (kname, ikm) in [("shared+transcript", cat(&shared, &transcript)), ("transcript+shared", cat(&transcript, &shared)), ("shared only", shared.to_vec())] {
                for label in [&b"arcm-dialer"[..], &b"arcm-listener"[..]] {
                    let key = if padded { blake2b32(&ikm, label) } else { blake2b_simd::Params::new().hash_length(32).hash(&cat(&ikm,label)).as_bytes().try_into().unwrap() };
                    for le in [false, true] {
                        let c = XChaCha20Poly1305::new_from_slice(&key).unwrap();
                        if let Ok(pt) = c.decrypt(&nonce(0, le), ct.as_slice()) {
                            hits += 1;
                            println!("MATCH: transcript={tname}, {pname}, ikm={kname}, label={}, nonce {}: {}", String::from_utf8_lossy(label), if le {"LE"} else {"BE"}, String::from_utf8_lossy(&pt[2..pt.len().min(200)]));
                        }
                    }
                }
            }
        }
    }
    if hits == 0 { println!("no variant matched"); }
}
