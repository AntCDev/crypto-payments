use crate::networks::evm::EVMNetwork;
use crate::networks::NetworkClient;
use crate::tokens::{PaymentDetails, TokenHandler, TokenRegistry};
use async_trait::async_trait;
use std::sync::Arc;

pub struct EthHandler {
    network: Arc<EVMNetwork>, // shared mainnet instance, not dyn — this handler knows its network
}

#[async_trait]
impl TokenHandler for EthHandler {
    fn token_id(&self) -> &str {
        "USDC_ETH"
    }

    async fn create_invoice_payment(
        &self,
        invoice_id: &str,
        amount: rust_decimal::Decimal,
    ) -> Result<PaymentDetails, String> {
        println!("EthHandler::create_invoice_payment({invoice_id}, {amount})");
        // next_index = self.next_derivation_index();
        // address = self.network.derive_address(mnemonic, next_index)?;
        // self.network.register_payment(PaymentWatch { .. });
        Ok(PaymentDetails {
            invoice_id: invoice_id.to_string(),
            network: "ethereum".to_string(),
            deposit_address: "stub_eth_address".to_string(),
            token_address: Some("0xUSDC".to_string()),
            decimals: 6,
            required_confirmations: 12,
        })
    }

    async fn cancel_payment(&self, invoice_id: &str) -> Result<(), String> {
        println!("EthHandler::cancel_payment({invoice_id})");
        Ok(())
    }
}

pub fn register(registry: &mut TokenRegistry) {
    let network = Arc::new(EVMNetwork::new("https://eth-rpc.example.com"));

    registry.register_token(
        "USDC_ETH",
        "USDC",
        "Eth",
        "USDC on the mainnet built using crates.",
        EthHandler { network },
    );
}