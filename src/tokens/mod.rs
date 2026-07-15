use std::collections::HashMap;
use std::sync::Arc;
use serde::Serialize;
use async_trait::async_trait;

pub mod eth;
pub mod base;

#[async_trait]
pub trait TokenHandler: Send + Sync {
    async fn create_invoice(&self, id: &str, amount: f64) -> Result<String, String>;
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