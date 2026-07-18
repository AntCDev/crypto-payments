use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;
use std::sync::Arc;

pub mod evm;
pub mod sol;
pub mod esplora;

#[derive(Clone)]
pub struct NetworkRegistry {
    pub evm: Arc<evm::EVMNetwork>,
    pub sol: Arc<sol::SolanaNetwork>,
    pub esplora: Arc<esplora::EsploraNetwork>,
}

impl NetworkRegistry {
    pub fn from_env() -> Self {
        let ethereum_rpc = std::env::var("ETHEREUM_RPC_URL")
            .expect("ETHEREUM_RPC_URL environment variable must be set");

        let solana_rpc = std::env::var("SOLANA_RPC_URL")
            .expect("SOLANA_RPC_URL environment variable must be set");

        let esplora_api = std::env::var("BITCOIN_ESPLORA_URL")
            .expect("BITCOIN_ESPLORA_URL environment variable must be set");

        Self {
            evm: Arc::new(evm::EVMNetwork::new(&ethereum_rpc)),
            sol: Arc::new(sol::SolanaNetwork::new(&solana_rpc)),
            esplora: Arc::new(esplora::EsploraNetwork::new(&esplora_api)),
        }
    }
}

#[derive(Clone, Debug)]
pub struct PaymentWatch {
    pub invoice_id: Uuid,
    pub address: String,
    pub token_address: Option<String>, // None = native asset
    pub decimals: u8,
    pub target_amount: u128,           // Changed from f64 to u128 (Atomic units)
    pub required_confirmations: u32,
    pub from_block: Option<u64>,       // None where not applicable (e.g. Esplora)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Amount(pub u128);          // Changed from f64 to u128 (Atomic units)

// Trait exists for consistency/testing across impls, not for dynamic dispatch —
// each TokenHandler holds its concrete network type directly.
#[async_trait]
pub trait NetworkClient: Send + Sync {
    fn new(rpc_url: &str) -> Self
    where
        Self: Sized;

    // wallet
    async fn get_derive_address(&self, pool: &PgPool, merchant_id: Uuid, mnemonic: &str) -> Result<(String, u32), String>;
    fn get_derivation_path(&self, index: u32) -> String;
    fn validate_address(&self, address: &str) -> bool;

    // chain state
    async fn get_native_balance(&self, address: &str) -> Result<Amount, String>;
    async fn get_token_balance(
        &self,
        token_address: &str,
        address: &str,
        decimals: u8,
    ) -> Result<Amount, String>;
    async fn get_current_block(&self) -> Result<u64, String>;

    // batched watching
    fn register_payment(&self, watch: PaymentWatch);
    fn unregister_payment(&self, invoice_id: Uuid);
    async fn watch_payments(&self) -> Result<(), String>;
}