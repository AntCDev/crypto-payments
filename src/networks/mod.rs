use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;
use std::collections::HashMap;
use std::sync::Arc;
use serde_json::{json, Map, Value};

use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce, Key
};
use argon2::{
    password_hash::{PasswordHasher, PasswordVerifier},
};
use sha2::{Digest};



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
    pub(crate) evm: HashMap<u64, Arc<evm::EVMNetwork>>,
    pub(crate) sol: HashMap<SolanaCluster, Arc<sol::SolanaNetwork>>,
    pub(crate) esplora: HashMap<BitcoinNetwork, Arc<esplora::EsploraNetwork>>,
}

impl NetworkRegistry {
    pub fn from_env(pool: PgPool) -> Self {
        println!("\n🌐 Initializing Network Registry...");

        // Safely fetch multi-URL strings (RPCs)
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

        // Helper to fetch single optional strings (like contract addresses)
        fn fetch_optional_env(key: &str) -> Option<String> {
            std::env::var(key)
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        }

        // ---- EVM ----
        let mut evm = HashMap::new();
        let evm_configs = [
            (1, "Ethereum", "ETH_MAINNET_RPC_URLS", "ETH_MAINNET_CONTRACT_ADDRESS"),
            (8453, "Base", "BASE_MAINNET_RPC_URLS", "BASE_MAINNET_CONTRACT_ADDRESS"),
            (137, "Polygon", "POLYGON_MAINNET_RPC_URLS", "POLYGON_MAINNET_CONTRACT_ADDRESS"),
            (84532, "Base Sepolia", "BASE_SEPOLIA_RPC_URLS", "BASE_SEPOLIA_CONTRACT_ADDRESS"),
            (11155111, "Sepolia", "SEPOLIA_RPC_URLS", "SEPOLIA_CONTRACT_ADDRESS"),
        ];

        for (chain_id, name, rpc_key, contract_key) in evm_configs {
            if let Some(urls) = fetch_and_log_urls(name, rpc_key) {
                let contract_address = fetch_optional_env(contract_key);

                if let Some(ref addr) = contract_address {
                    println!("    └─ Contract Address: {}", addr);
                } else {
                    println!("    └─ Contract Address: ⚠️ None configured");
                }

                let network = Arc::new(evm::EVMNetwork::new(chain_id, urls, contract_address));
                evm.insert(chain_id, network.clone());

                // Spawn background payment watcher task
                let pool_clone = pool.clone();
                tokio::spawn(async move {
                    if let Err(err) = network.watch_payments(&pool_clone).await {
                        eprintln!("❌ Error in EVM network (Chain ID: {}) watch_payments: {}", chain_id, err);
                    }
                });
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
                let network = Arc::new(sol::SolanaNetwork::new(cluster, urls));
                sol.insert(cluster, network.clone());

                // Spawn background payment watcher task
                let pool_clone = pool.clone();
                tokio::spawn(async move {
                    if let Err(err) = network.watch_payments(&pool_clone).await {
                        eprintln!("❌ Error in Solana network ({:?}) watch_payments: {}", cluster, err);
                    }
                });
            }
        }

        // ---- Esplora (Bitcoin) ----
        let mut esplora = HashMap::new();
        let bitcoin_configs = [
            (BitcoinNetwork::Mainnet, "Bitcoin Mainnet"),
            (BitcoinNetwork::Testnet4, "Bitcoin Testnet4"),
            (BitcoinNetwork::Signet, "Bitcoin Signet"),
        ];

        for (network_type, name) in bitcoin_configs {
            if let Some(urls) = fetch_and_log_urls(name, network_type.env_prefix()) {
                let network = Arc::new(esplora::EsploraNetwork::new(network_type, urls));
                esplora.insert(network_type, network.clone());

                // Spawn background payment watcher task
                let pool_clone = pool.clone();
                tokio::spawn(async move {
                    if let Err(err) = network.watch_payments(&pool_clone).await {
                        eprintln!("❌ Error in Bitcoin network ({:?}) watch_payments: {}", network_type, err);
                    }
                });
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
    async fn get_derive_address(&self, pool: &PgPool, merchant_id: Uuid, invoice_id: Uuid, mnemonic: &str, token_address: Option<&str>) -> Result<(String, u32, Option<String>), String>;
    fn validate_address(&self, address: &str) -> bool;
    async fn get_native_balance(&self, address: &str) -> Result<Amount, String>;
    async fn get_token_balance(&self, token_address: &str, address: &str, decimals: u8) -> Result<Amount, String>;
    async fn get_current_block(&self) -> Result<u64, String>;
    fn register_payment(&self, watch: PaymentWatch);
    fn unregister_payment(&self, invoice_id: Uuid);
    async fn watch_payments(&self, pool: &PgPool) -> Result<(), String>;
}


/// Enqueues a webhook event for the merchant that owns `invoice_id`.
///
/// - Looks up the merchant's `webhook_url` and the invoice's opaque `data`
///   field in one query, joined off invoice_id (no need for callers to carry
///   merchant_id around separately).
/// - If the merchant hasn't configured a webhook_url, this is a silent no-op —
///   there's nowhere to deliver to yet, and no point enqueueing a row that'll
///   never be dispatched.
/// - `dedupe_suffix` should uniquely identify the underlying occurrence
///   (payment_id, tx_hash, etc.) — it gets combined with event_type to form
///   the dedupe_key, so calling this twice for the same real-world event is
///   always safe (ON CONFLICT DO NOTHING against webhook_events_dedupe_uniq).
/// - `fields` are the event-specific payload fields (TxHash, BlockNumber, ...);
///   this function adds `Data` (the merchant's opaque invoice data) and
///   `InvoiceId` on top.
/// - Takes a `&mut Transaction` deliberately: call sites should insert/update
///   whatever mutated the domain state and enqueue the webhook in the same
///   transaction, so a rollback can never leave a webhook enqueued for a
///   change that didn't happen (or vice versa).
async fn enqueue_webhook(
    tx: &mut Transaction<'_, Postgres>,
    invoice_id: Uuid,
    event_type: &str,
    dedupe_suffix: &str,
    mut fields: Map<String, Value>,
) -> Result<(), String> {
    let row: (Uuid, Option<String>, Option<String>) = sqlx::query_as(
        r#"
        SELECT m.id, m.webhook_url, i.data
          FROM invoices i
          JOIN merchants m ON m.id = i.merchant_id
         WHERE i.id = $1
        "#,
    )
        .bind(invoice_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| format!("enqueue_webhook merchant lookup: {e}"))?;

    let (merchant_id, webhook_url, invoice_data) = row;

    let Some(url) = webhook_url else {
        // Merchant has no webhook configured — nothing to enqueue.
        return Ok(());
    };

    // The merchant-supplied opaque payload from invoice creation. Left as a
    // plain string on purpose: could be "25", could be `{"Amount":50}`, we
    // don't parse it, the merchant does.
    fields.insert(
        "Data".to_string(),
        invoice_data.map(Value::String).unwrap_or(Value::Null),
    );
    fields.insert("InvoiceId".to_string(), json!(invoice_id));

    let event_data = Value::Object(fields);
    let dedupe_key = format!("{event_type}:{dedupe_suffix}");

    sqlx::query(
        r#"
        INSERT INTO webhook_events (merchant_id, url, event_type, event_data, dedupe_key)
        VALUES ($1, $2, $3, $4, $5)
        ON CONFLICT (merchant_id, dedupe_key) DO NOTHING
        "#,
    )
        .bind(merchant_id)
        .bind(url)
        .bind(event_type)
        .bind(event_data)
        .bind(dedupe_key)
        .execute(&mut **tx)
        .await
        .map_err(|e| format!("enqueue_webhook insert: {e}"))?;

    Ok(())
}

pub fn decrypt_data(master_key: &[u8; 32], ciphertext: &[u8], nonce_bytes: &[u8]) -> Result<Vec<u8>, String> {
    if nonce_bytes.len() != 12 {
        return Err("Invalid nonce length".to_string());
    }
    let cipher = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(master_key));
    let nonce = Nonce::from_slice(nonce_bytes);

    cipher
        .decrypt(nonce, ciphertext)
        .map_err(|_| "Decryption failed (tampered data or wrong key)".to_string())
}
