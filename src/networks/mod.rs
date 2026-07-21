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
}

impl SolanaCluster {
    fn env_prefix(&self) -> &'static str {
        match self {
            SolanaCluster::MainnetBeta => "SOLANA_MAINNET_RPC_URLS",
            SolanaCluster::Testnet => "SOLANA_TESTNET_RPC_URLS",
            SolanaCluster::Devnet => "SOLANA_DEVNET_RPC_URLS",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BitcoinNetwork {
    Mainnet,
    Testnet4,
    Signet,
}

impl BitcoinNetwork {
    fn env_prefix(&self) -> &'static str {
        match self {
            BitcoinNetwork::Mainnet => "ESPLORA_MAINNET_URLS",
            BitcoinNetwork::Testnet4 => "ESPLORA_TESTNET4_URLS",
            BitcoinNetwork::Signet => "ESPLORA_SIGNET_URLS",
        }
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
        println!("\n🌐 Initializing Network Registry...");

        // Safely fetch URLs from env variables; treats empty strings ("") or missing variables as None
        fn fetch_and_log_urls(name: &str, key: &str) -> Option<Vec<String>> {
            let urls: Vec<String> = match std::env::var(key) {
                Ok(raw) => raw
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
                Err(_) => Vec::new(),
            };

            if urls.is_empty() {
                println!("  {} Network ❌ No valid RPC_URL found", name);
                None
            } else {
                let count = urls.len();
                let redundancy = if count > 1 { ", enabling redundancy" } else { "" };
                println!("  {} Network ✅ {} RPC_URL Found{}", name, count, redundancy);
                Some(urls)
            }
        }

        // ---- EVM ----
        let mut evm = HashMap::new();
        let evm_configs = [
            (1, "Ethereum", "ETH_MAINNET_RPC_URLS"),
            (8453, "Base", "BASE_MAINNET_RPC_URLS"),
            (137, "Polygon", "POLYGON_MAINNET_RPC_URLS"),
            (84532, "Base Sepolia", "BASE_SEPOLIA_RPC_URLS"),
        ];

        for (chain_id, name, key) in evm_configs {
            if let Some(urls) = fetch_and_log_urls(name, key) {
                evm.insert(chain_id, Arc::new(evm::EVMNetwork::new(chain_id, urls)));
            }
        }

        // ---- Solana ----
        let mut sol = HashMap::new();
        let sol_configs = [
            (SolanaCluster::MainnetBeta, "Solana Mainnet"),
            (SolanaCluster::Testnet, "Solana Testnet"),
            (SolanaCluster::Devnet, "Solana Devnet"),
        ];

        for (cluster, name) in sol_configs {
            if let Some(urls) = fetch_and_log_urls(name, cluster.env_prefix()) {
                sol.insert(cluster, Arc::new(sol::SolanaNetwork::new(cluster, urls)));
            }
        }

        // ---- Esplora (Bitcoin) ----
        let mut esplora = HashMap::new();
        let bitcoin_configs = [
            (BitcoinNetwork::Mainnet, "Bitcoin Mainnet"),
            (BitcoinNetwork::Testnet4, "Bitcoin Testnet4"),
            (BitcoinNetwork::Signet, "Bitcoin Signet"),
        ];

        for (network, name) in bitcoin_configs {
            if let Some(urls) = fetch_and_log_urls(name, network.env_prefix()) {
                esplora.insert(network, Arc::new(esplora::EsploraNetwork::new(network, urls)));
            }
        }

        Self { evm, sol, esplora }
    }

    pub fn evm_chain(&self, chain_id: u64) -> Option<Arc<evm::EVMNetwork>> {
        self.evm.get(&chain_id).cloned()
    }

    pub fn sol_cluster(&self, cluster: SolanaCluster) -> Option<Arc<sol::SolanaNetwork>> {
        self.sol.get(&cluster).cloned()
    }

    pub fn esplora_network(&self, network: BitcoinNetwork) -> Option<Arc<esplora::EsploraNetwork>> {
        self.esplora.get(&network).cloned()
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