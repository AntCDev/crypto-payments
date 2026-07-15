use crate::networks::evm::EVMNetwork;
use crate::networks::NetworkClient;
use crate::tokens::{PaymentDetails, TokenHandler, TokenRegistry};
use async_trait::async_trait;
use std::sync::Arc;

pub struct BaseUsdcHandler {
    network: Arc<EVMNetwork>,
}

#[async_trait]
impl TokenHandler for BaseUsdcHandler {
    fn token_id(&self) -> &str {
        "USDC_BASE"
    }

    async fn create_invoice_payment(
        &self,
        invoice_id: &str,
        amount: rust_decimal::Decimal,
    ) -> Result<PaymentDetails, String> {
        println!("BaseUsdcHandler::create_invoice_payment({invoice_id}, {amount})");
        Ok(PaymentDetails {
            invoice_id: invoice_id.to_string(),
            network: "base".to_string(),
            deposit_address: "stub_base_address".to_string(),
            token_address: Some("0xUSDC_BASE".to_string()),
            decimals: 6,
            required_confirmations: 5,
        })
    }

    async fn cancel_payment(&self, invoice_id: &str) -> Result<(), String> {
        println!("BaseUsdcHandler::cancel_payment({invoice_id})");
        Ok(())
    }
}

pub fn register(registry: &mut TokenRegistry) {
    let network = Arc::new(EVMNetwork::new("https://base-rpc.example.com"));

    registry.register_token(
        "USDC_BASE",
        "USDC",
        "Base",
        "USDC on Base built using crates.",
        BaseUsdcHandler { network },
    );
}