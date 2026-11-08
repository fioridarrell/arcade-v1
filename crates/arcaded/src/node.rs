//! Shared state of one running arcade.

use arcade_app::AppDb;
use arcade_chain::params::ChainParams;
use arcade_chaindb::ChainDb;
use ed25519_dalek::SigningKey;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// What the mesh told us about a chain's index (an `ix` tip).
#[derive(Debug, Clone, Default)]
pub struct MeshTip {
    pub height: u64,
    pub names_digest: Option<String>,
    /// Index checkpoint and its range digest, and the consensus hash, as the peer reports them.
    pub at: u64,
    pub d: Option<String>,
    pub s: Option<String>,
    #[allow(dead_code)]
    pub from: String,
}

#[derive(Debug, Clone)]
pub struct LinkInfo {
    pub addr: String,
    pub node_id: String,
    pub since: u64,
}

pub struct Node {
    #[allow(dead_code)]
    pub home: std::path::PathBuf,
    pub app: AppDb,
    pub chain: ChainDb,
    pub params: &'static ChainParams,
    pub identity: SigningKey,
    /// Raw transactions waiting to be sent to coin peers.
    pub tx_outbox: Mutex<Vec<Vec<u8>>>,
    /// Frames waiting to be sent to every mesh link.
    pub mesh_outbox: Mutex<Vec<Value>>,
    pub links: Mutex<BTreeMap<String, LinkInfo>>,
    pub mesh_tips: Mutex<BTreeMap<String, MeshTip>>,
    pub sync_state: Mutex<String>,
    /// V1.0 arcades that serve blocks: dial address -> their tip height.
    pub bk_peers: Mutex<BTreeMap<String, u64>>,
    /// Port we accept mesh links on (None = dial out only).
    pub mesh_port: Option<u16>,
    /// Set false to skip mesh bootstrap (P2P only).
    pub mesh_bootstrap: bool,
    /// Welcome asks seen on the mesh: asker arcade id -> latest ask record.
    pub asks: Mutex<BTreeMap<String, Value>>,
    /// Blocks we mined, waiting to be sent to coin peers.
    pub block_outbox: Mutex<Vec<Vec<u8>>>,
    pub miner: crate::miner::MinerState,
    /// Chains installed by hand (Chains & Coins), by key.
    pub extra: Mutex<BTreeMap<String, std::sync::Arc<crate::chains::ChainRun>>>,
    /// Weak self-reference so API handlers can start threads that own the node.
    pub me: std::sync::OnceLock<std::sync::Weak<Node>>,
    /// Coins spent by our own transactions that are not in a block yet.
    pub pending_spent: Mutex<std::collections::HashSet<arcade_chain::OutPoint>>,
    /// Asker ids someone has already paid (a `sent` or `closed` record was seen).
    pub asks_done: Mutex<std::collections::BTreeSet<String>>,
}


impl Node {
    pub fn self_arc(&self) -> std::sync::Arc<Node> {
        self.me.get().and_then(|w| w.upgrade()).expect("node is running")
    }
}
