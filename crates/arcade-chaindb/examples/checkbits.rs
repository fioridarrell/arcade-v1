//! Recompute every block's difficulty with our Pepecoin rule and compare with the chain.
use arcade_chain::params::PEPECOIN_TEST;
use arcade_chain::pepecoin::next_bits;
fn main() {
    let p = &PEPECOIN_TEST;
    let db = arcade_chaindb::ChainDb::open(std::env::args().nth(1).unwrap()).unwrap();
    let tip = db.tip(p.chain_no).unwrap().unwrap().height;
    let hdr = |h: u32| db.header(p.chain_no, &db.hash_at(p.chain_no, h).unwrap().unwrap()).unwrap().unwrap().header;
    let (mut ok, mut bad) = (0u32, 0u32);
    let (mut gp, mut pa) = (hdr(0), hdr(1));
    for h in 2..=tip {
        let cur = hdr(h);
        let want = next_bits(p, h - 1, pa.bits, pa.time, Some(gp.time), cur.time);
        if want == cur.bits { ok += 1 } else { bad += 1; if bad <= 5 { eprintln!("height {h}: chain {:08x}, ours {want:08x}", cur.bits) } }
        gp = pa; pa = cur;
    }
    println!("blocks 2..={tip}: {ok} match, {bad} differ");
}
