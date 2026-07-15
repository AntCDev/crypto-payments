use crate::tokens::{TokenHandler, TokenRegistry};
use async_trait::async_trait;

pub struct EthHandler;

#[async_trait]
impl TokenHandler for EthHandler {
    async fn create_invoice(&self, id: &str, amount: f64) -> Result<String, String> {
        Ok(format!("Invoice created via EthHandler for {} with amount {}", id, amount))
    }
}

pub fn register(registry: &mut TokenRegistry) {
    registry.register_token(
        "USDC_ETH",
        "USDC",
        "Eth",
        "USDC on the mainnet built using crates.",
        EthHandler,
    );
}