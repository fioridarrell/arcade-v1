//! arcade-split: move chains out of the shared chain.mdbx, each into its own file
//! (docs/SPLITDB.md). Run it with the arcade stopped.
//!
//!   arcade-split --home DIR                  list the chains and where each one is
//!   arcade-split --home DIR --chain KEY      move one chain
//!   arcade-split --home DIR --all            move every chain still in the shared file,
//!                                            the one with the lowest tip first
//!
//! The shared file is only read: a moved chain's rows stay in it until the shared file is
//! deleted by hand. Going back = deleting chains/<key>/ (the arcade then uses the shared file).
use arcade_chaindb::db::chain_dir_name;
use arcade_chaindb::schema::*;
use arcade_chaindb::split;
use arcade_store::Store;
use std::path::{Path, PathBuf};

const USAGE: &str = "usage: arcade-split --home DIR [--chain KEY | --all]\n";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let opt = |n: &str| args.iter().position(|a| a == n).and_then(|i| args.get(i + 1)).cloned();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        print!("{USAGE}");
        return;
    }
    let Some(home) = opt("--home").map(PathBuf::from) else {
        eprint!("{USAGE}");
        std::process::exit(2);
    };
    if let Err(e) = run(&home, opt("--chain"), args.iter().any(|a| a == "--all")) {
        eprintln!("arcade-split: {e}");
        std::process::exit(1);
    }
}

fn own_path(home: &Path, c: u16) -> PathBuf {
    home.join("chains").join(chain_dir_name(c)).join("chain.mdbx")
}

fn tip_of(s: &Store, c: u16) -> u32 {
    s.read(|r| r.get(STATE, &k_state(c, "tip"))).ok().flatten().map(|v| u32::from_be_bytes(v[32..36].try_into().unwrap_or([0; 4]))).unwrap_or(0)
}

fn run(home: &Path, chain: Option<String>, all: bool) -> Result<(), String> {
    // The arcade's own lock: refuse while it runs.
    let lock_path = home.join("arcaded.lock");
    let lock = std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&lock_path).map_err(|e| format!("{}: {e}", lock_path.display()))?;
    lock.try_lock().map_err(|_| format!("the arcade is running in {}: stop it first", home.display()))?;
    let shared_path = home.join("chain.mdbx");
    if !shared_path.exists() {
        return Err(format!("no {}", shared_path.display()));
    }
    let shared = Store::open_readonly(&shared_path, &CHAIN_SCHEMA).map_err(|e| e.to_string())?;
    let mut chains = split::chains_in(&shared).map_err(|e| e.to_string())?;
    chains.sort_by_key(|&c| tip_of(&shared, c));
    let todo: Vec<u16> = match (chain, all) {
        (Some(k), false) => {
            let p = arcade_chain::params::by_key(&k).ok_or(format!("unknown chain {k}"))?;
            if !chains.contains(&p.chain_no) {
                return Err(format!("{} has no rows in the shared file", p.key));
            }
            vec![p.chain_no]
        }
        (None, true) => chains.iter().copied().filter(|&c| !own_path(home, c).exists()).collect(),
        (None, false) => {
            for &c in &chains {
                let own = own_path(home, c);
                let at = if own.exists() { format!("its own file {} (its rows are still in the shared file too)", own.display()) } else { "the shared file".into() };
                println!("{:<9} tip {:>9}  in {at}", chain_dir_name(c), tip_of(&shared, c));
            }
            return Ok(());
        }
        _ => return Err(USAGE.trim().into()),
    };
    for c in todo {
        let own = own_path(home, c);
        println!("{}: copying into {}", chain_dir_name(c), own.display());
        let say = |s: &str| println!("{s}");
        let r = split::split_chain(&shared, &own, c, &say).map_err(|e| format!("{}: {e}", chain_dir_name(c)))?;
        println!(
            "{}: done, checked: {} rows, {:.2} GB in {:.0} s ({:.0} MB/s); new file {:.2} GB",
            chain_dir_name(c),
            r.rows(),
            r.bytes() as f64 / 1e9,
            r.secs,
            r.bytes() as f64 / 1e6 / r.secs.max(0.001),
            r.file_bytes as f64 / 1e9
        );
    }
    drop(lock);
    Ok(())
}
