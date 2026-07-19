use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;

pub mod evm;
pub mod sol;
pub mod esplora;


#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SolanaCluster {
    MainnetBeta,
    Testnet,
    Devnet,
    // Localnet omitted from prod registry; add if you spin up test validators in CI
}

impl SolanaCluster {
    fn env_prefix(&self) -> &'static str {
        match self {
            SolanaCluster::MainnetBeta => "SOLANA_MAINNET_RPC_URLS",
            SolanaCluster::Testnet => "SOLANA_TESTNET_RPC_URLS",
            SolanaCluster::Devnet => "SOLANA_DEVNET_RPC_URLS",
        }
    }

    /// Mainnet is required in every deployment; testnet/devnet are optional
    /// (only needed if you're actually accepting payments there, e.g. staging).
    fn required(&self) -> bool {
        matches!(self, SolanaCluster::MainnetBeta)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitcoinNetwork {
    Mainnet,
    Testnet4, // testnet3 is being deprecated by most Esplora operators; prefer testnet4
    Signet,
    // Regtest omitted from prod registry; useful for local/CI only
}

impl BitcoinNetwork {
    fn env_prefix(&self) -> &'static str {
        match self {
            BitcoinNetwork::Mainnet => "ESPLORA_MAINNET_URLS",
            BitcoinNetwork::Testnet4 => "ESPLORA_TESTNET4_URLS",
            BitcoinNetwork::Signet => "ESPLORA_SIGNET_URLS",
        }
    }

    fn required(&self) -> bool {
        matches!(self, BitcoinNetwork::Mainnet)
    }
}



#[derive(Clone)]
pub struct NetworkRegistry {
    evm: HashMap<u64, Arc<evm::EVMNetwork>>,
    sol: HashMap<SolanaCluster, Arc<sol::SolanaNetwork>>,
    esplora: HashMap<BitcoinNetwork, Arc<esplora::EsploraNetwork>>,
}

impl NetworkRegistry {
    pub fn from_env() -> Self {
        // ETH_MAINNET_RPC_URLS="https://a,https://b,https://c" -> Vec<String>
        fn urls_from_env(key: &str) -> Vec<String> {
            let raw = std::env::var(key)
                .unwrap_or_else(|_| panic!("{key} environment variable must be set"));
            let urls: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            assert!(!urls.is_empty(), "{key} must contain at least one URL");
            urls
        }

        // Same as above, but returns None if the var is simply unset —
        // lets optional networks (testnet/devnet/signet) be skipped entirely.
        fn urls_from_env_opt(key: &str) -> Option<Vec<String>> {
            match std::env::var(key) {
                Ok(raw) => {
                    let urls: Vec<String> = raw
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    assert!(!urls.is_empty(), "{key} was set but contained no URLs");
                    Some(urls)
                }
                Err(_) => None,
            }
        }

        // ---- EVM ----
        let mut evm = HashMap::new();
        evm.insert(1,     Arc::new(evm::EVMNetwork::new(1,     urls_from_env("ETH_MAINNET_RPC_URLS"))));
        evm.insert(8453,  Arc::new(evm::EVMNetwork::new(8453,  urls_from_env("BASE_MAINNET_RPC_URLS"))));
        evm.insert(137,   Arc::new(evm::EVMNetwork::new(137,   urls_from_env("POLYGON_MAINNET_RPC_URLS"))));
        evm.insert(84532, Arc::new(evm::EVMNetwork::new(84532, urls_from_env("BASE_SEPOLIA_RPC_URLS"))));

        // ---- Solana ----
        let mut sol = HashMap::new();
        for cluster in [
            SolanaCluster::MainnetBeta,
            SolanaCluster::Testnet,
            SolanaCluster::Devnet,
        ] {
            let urls = if cluster.required() {
                Some(urls_from_env(cluster.env_prefix()))
            } else {
                urls_from_env_opt(cluster.env_prefix())
            };
            if let Some(urls) = urls {
                sol.insert(cluster, Arc::new(sol::SolanaNetwork::new(cluster, urls)));
            }
        }

        // ---- Esplora (Bitcoin) ----
        let mut esplora = HashMap::new();
        for network in [
            BitcoinNetwork::Mainnet,
            BitcoinNetwork::Testnet4,
            BitcoinNetwork::Signet,
        ] {
            let urls = if network.required() {
                Some(urls_from_env(network.env_prefix()))
            } else {
                urls_from_env_opt(network.env_prefix())
            };
            if let Some(urls) = urls {
                esplora.insert(network, Arc::new(esplora::EsploraNetwork::new(network, urls)));
            }
        }

        Self { evm, sol, esplora }
    }

    /// Central lookup so token handlers never touch the HashMap directly.
    pub fn evm_chain(&self, chain_id: u64) -> Arc<evm::EVMNetwork> {
        self.evm
            .get(&chain_id)
            .cloned()
            .unwrap_or_else(|| panic!("chain_id {chain_id} not configured in NetworkRegistry"))
    }

    pub fn sol_cluster(&self, cluster: SolanaCluster) -> Arc<sol::SolanaNetwork> {
        self.sol
            .get(&cluster)
            .cloned()
            .unwrap_or_else(|| panic!("Solana cluster {cluster:?} not configured in NetworkRegistry"))
    }

    pub fn esplora_network(&self, network: BitcoinNetwork) -> Arc<esplora::EsploraNetwork> {
        self.esplora
            .get(&network)
            .cloned()
            .unwrap_or_else(|| panic!("Bitcoin network {network:?} not configured in NetworkRegistry"))
    }
}

#[derive(Clone, Debug)]
pub struct PaymentWatch {
    pub invoice_id: Uuid,
    pub address: String,
    pub token_address: Option<String>,
    pub decimals: u8,
    pub target_amount: u128,
    pub required_confirmations: u32,
    pub from_block: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Amount(pub u128);

#[async_trait]
pub trait NetworkClient: Send + Sync {
    async fn get_derive_address(&self, pool: &PgPool, merchant_id: Uuid, invoice_id: Uuid, mnemonic: &str) -> Result<(String, u32, Option<String>), String>;
    fn get_derivation_path(&self, index: u32) -> String;
    fn validate_address(&self, address: &str) -> bool;
    async fn get_native_balance(&self, address: &str) -> Result<Amount, String>;
    async fn get_token_balance(&self, token_address: &str, address: &str, decimals: u8) -> Result<Amount, String>;
    async fn get_current_block(&self) -> Result<u64, String>;
    fn register_payment(&self, watch: PaymentWatch);
    fn unregister_payment(&self, invoice_id: Uuid);
    async fn watch_payments(&self) -> Result<(), String>;
}