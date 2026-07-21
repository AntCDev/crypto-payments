use std::collections::HashMap;
use std::sync::Arc;
use serde::Serialize;
use async_trait::async_trait;
use uuid::Uuid;
use sqlx::PgPool;
use chrono::{DateTime, Utc};
use crate::networks::NetworkRegistry; // Import your central network registry

pub mod eth;
pub mod base;

#[derive(Clone, Debug, Serialize)]
pub struct PaymentDetails {
    pub invoice_id: Uuid,
    pub network: String,
    pub deposit_address: String,
    pub token_address: Option<String>,
    pub decimals: u8,
    pub required_confirmations: i32,
    pub wallet_index: u32,
    pub expires_at: DateTime<Utc>,
}

#[async_trait]
pub trait TokenHandler: Send + Sync {
    fn token_id(&self) -> &str;

    async fn create_invoice_payment(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        amount: rust_decimal::Decimal,
        token_id: &str,
    ) -> Result<PaymentDetails, String>;

    async fn cancel_payment(&self, pool: &PgPool, invoice_id: Uuid) -> Result<(), String>;
}

#[derive(Clone, Serialize)]
pub struct TokenMetadata {
    pub id: String,
    pub name: String,
    pub detail: String,
    pub info: String,
}

pub struct TokenRegistry {
    handlers: HashMap<String, Arc<dyn TokenHandler>>,
    metadata: Vec<TokenMetadata>,
}

impl TokenRegistry {
    /// Accepts the shared single-instance networks on initialization
    pub fn new(networks: Arc<NetworkRegistry>) -> Self {
        println!("\n🪙 Registering Token Handlers...");
        let mut registry = Self {
            handlers: HashMap::new(),
            metadata: Vec::new(),
        };

        // Pass the networks registry forward to sub-modules
        eth::register(&mut registry, networks.clone());
        base::register(&mut registry, networks.clone());

        registry
    }

    pub fn register_token<H>(&mut self, id: &str, name: &str, detail: &str, info: &str, handler: H)
    where
        H: TokenHandler + 'static,
    {
        // Extract struct name from full type path (e.g. "my_app::tokens::eth::EthHandler" -> "EthHandler")
        let full_type = std::any::type_name::<H>();
        let handler_name = full_type.split("::").last().unwrap_or(full_type);

        println!("  ✅ {} - {} - {} - {}", id, name, detail, handler_name);

        self.metadata.push(TokenMetadata {
            id: id.to_string(),
            name: name.to_string(),
            detail: detail.to_string(),
            info: info.to_string(),
        });
        self.handlers.insert(id.to_string(), Arc::new(handler));
    }

    pub fn get_metadata(&self) -> Vec<TokenMetadata> {
        self.metadata.clone()
    }

    pub fn get_handler(&self, id: &str) -> Option<Arc<dyn TokenHandler>> {
        self.handlers.get(id).cloned()
    }
}