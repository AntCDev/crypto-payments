use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub mod evm;
pub mod sol;
pub mod esplora;

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
    fn derive_address(&self, mnemonic: &str, index: u32) -> Result<String, String>;
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