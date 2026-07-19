// tokens/eth.rs
use crate::networks::evm::EVMNetwork;
use crate::networks::{NetworkClient, NetworkRegistry};
use crate::tokens::{PaymentDetails, TokenHandler, TokenRegistry};
use async_trait::async_trait;
use std::sync::Arc;
use sqlx::PgPool;
use uuid::Uuid;
use chrono::{Utc, Duration};

pub fn register(registry: &mut TokenRegistry, networks: Arc<NetworkRegistry>) {
    let handler = EthHandler {
        network: networks.evm_chain(1), // Ethereum mainnet
    };

    registry.register_token(
        "USDC_ETH",
        "USD Coin (Ethereum)",
        "USDC stablecoin hosted natively on the Ethereum Mainnet.",
        "Requires 12 network confirmations.",
        handler,
    );
}

pub struct EthHandler {
    network: Arc<EVMNetwork>,
}

#[async_trait]
impl TokenHandler for EthHandler {
    fn token_id(&self) -> &str {
        "USDC_ETH"
    }

    async fn create_invoice_payment(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        amount: rust_decimal::Decimal,
        _token_id: &str,
    ) -> Result<PaymentDetails, String> {
        // TODO: Replace with real merchant mnemonic fetching mechanism when ready
        let merchant_mnemonic = "test test test test test test test test test test test junk";

        // Dynamic key derivation using the shared EVM network helper
        let (deposit_address, derived_wallet_index, payment_reference) = self.network
            .get_derive_address(pool, merchant_id, invoice_id, merchant_mnemonic)
            .await
            .map_err(|e| format!("Address derivation failed: {e}"))?;

        // Mainnet transactions take longer; keeping your 60-minute payment window
        let expires_at = Utc::now() + Duration::minutes(60);

        // Update the database record using the matching SQLX structure
        sqlx::query!(
            r#"UPDATE invoices SET wallet_address = $1, wallet_index = $2, expires_at = $3, payment_reference = $4 WHERE id = $5"#,
            deposit_address, derived_wallet_index as i32, expires_at, payment_reference, invoice_id
        )
            .execute(pool)
            .await
            .map_err(|e| format!("DB update failed: {e}"))?;

        Ok(PaymentDetails {
            invoice_id,
            network: "ethereum".to_string(),
            deposit_address,
            token_address: Some("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string()), // Real Ethereum USDC contract
            decimals: 6,
            required_confirmations: 12, // Higher threshold for Mainnet safety
            wallet_index: derived_wallet_index,
            expires_at,
        })
    }

    async fn cancel_payment(&self, _pool: &PgPool, invoice_id: Uuid) -> Result<(), String> {
        println!("EthHandler::cancel_payment({invoice_id})");
        Ok(())
    }
}