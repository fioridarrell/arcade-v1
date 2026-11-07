//! arcaded: the Arcade V1.0 service. Syncs Pepecoin testnet over P2P into chain.mdbx,
//! keeps mesh links to other arcades, maintains the @tag index, runs tag claims, and
//! serves the local API for the `arcade` CLI and the desktop app.
//!
//!   arcaded [--home DIR] [--no-mesh]

mod api;
mod chainsvc;
mod claims;
mod faucet;
mod ledger;
mod meshsvc;
mod node;

use node::Node;
use std::sync::{Arc, Mutex};

pub fn default_home() -> std::path::PathBuf {
    if let Some(h) = std::env::var_os("ARCADE_HOME") {
        return h.into();
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
    base.join("arcade")
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let home = args
        .iter()
        .position(|a| a == "--home")
        .and_then(|i| args.get(i + 1))
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_home);
    let mesh = !args.iter().any(|a| a == "--no-mesh");
    std::fs::create_dir_all(&home).expect("create home");

    let app = arcade_app::AppDb::open(&home).expect("open arcade.mdbx");
    let chain = arcade_chaindb::ChainDb::open(home.join("chain.mdbx")).expect("open chain.mdbx");
    let identity = app.node_identity().expect("node identity");
    let node = Arc::new(Node {
        home: home.clone(),
        app,
        chain,
        params: &arcade_chain::params::PEPECOIN_TEST,
        identity,
        tags: Mutex::new(Default::default()),
        tx_outbox: Mutex::new(Vec::new()),
        mesh_outbox: Mutex::new(Vec::new()),
        links: Mutex::new(Default::default()),
        mesh_tips: Mutex::new(Default::default()),
        sync_state: Mutex::new("starting".into()),
    });
    let _ = node.app.event("service", "arcaded started");
    eprintln!(
        "arcaded: home {}, node {}, chain {}",
        home.display(),
        hex::encode(node.identity.verifying_key().to_bytes()),
        node.params.name
    );

    {
        let n = node.clone();
        std::thread::Builder::new().name("chain".into()).spawn(move || chainsvc::run(n)).unwrap();
    }
    {
        // Claims and the tag index also advance on their own clock.
        let n = node.clone();
        std::thread::Builder::new()
            .name("claims".into())
            .spawn(move || loop {
                let _ = ledger::catch_up(&n);
                claims::advance_all(&n);
                std::thread::sleep(std::time::Duration::from_secs(15));
            })
            .unwrap();
    }
    if mesh {
        for seed in meshsvc::SEEDS {
            let n = node.clone();
            let a = seed.to_string();
            std::thread::Builder::new().name(format!("mesh {a}")).spawn(move || meshsvc::run(n, a)).unwrap();
        }
    }
    api::serve(node, home.join("arcaded.sock"));
}
