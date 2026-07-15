use crate::tokens::{TokenHandler, TokenRegistry};
use async_trait::async_trait;

pub struct BaseHandler;
#[async_trait]
impl TokenHandler for BaseHandler {
    async fn create_invoice(&self, id: &str, amount: f64) -> Result<String, String> {
        Ok(format!( "Invoice created via BaseHandler for {} with amount {}", id, amount ))
    }
}


pub struct BaseHandlerRPC;
#[async_trait]
impl TokenHandler for BaseHandlerRPC {
    async fn create_invoice(&self, id: &str, amount: f64) -> Result<String, String> {
        Ok(format!(  "Invoice created via BaseHandlerRPC for {} with amount {}", id, amount ))
    }
}


pub fn register(registry: &mut TokenRegistry) {
    registry.register_token(
        "USDC_BASE",
        "USDC",
        "BASE",
        "USDC on base built using crates.",
        BaseHandler,
    );

    registry.register_token(
        "USDC_BASE_RPC",
        "USDC",
        "BASE",
        "USDC on base built using raw RPC-JSON calls.",
        BaseHandlerRPC,
    );
}