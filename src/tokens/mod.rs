use std::collections::HashMap;
use std::sync::Arc;
use serde::Serialize;
use async_trait::async_trait;
use uuid::Uuid;
use sqlx::PgPool;
use chrono::{DateTime, Utc};

pub mod eth;
pub mod base;

#[derive(Clone, Debug, Serialize)]
pub struct PaymentDetails {
    pub invoice_id: Uuid,
    pub network: String,
    pub deposit_address: String,
    pub token_address: Option<String>,
    pub decimals: u8,
    pub required_confirmations: u32,
    pub wallet_index: i32,
    pub expires_at: DateTime<Utc>,
}

#[async_trait]
pub trait TokenHandler: Send + Sync {
    fn token_id(&self) -> &str;

    /// Called by the orchestrator after it has already logged the initial invoice record.
    /// Derives a deposit address, registers a watch on the underlying network, 
    /// executes database updates for the invoice, and returns the final payment details.
    async fn create_invoice_payment(
        &self,
        pool: &PgPool,
        invoice_id: Uuid,
        amount: rust_decimal::Decimal,
    ) -> Result<PaymentDetails, String>;

    /// Cancels payment watching and cleans up allocations for the given invoice.
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
    pub fn new() -> Self {
        let mut registry = Self {
            handlers: HashMap::new(),
            metadata: Vec::new(),
        };

        eth::register(&mut registry);
        base::register(&mut registry);

        registry
    }

    pub fn register_token<H>(&mut self, id: &str, name: &str, detail: &str, info: &str, handler: H)
    where
        H: TokenHandler + 'static,
    {
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