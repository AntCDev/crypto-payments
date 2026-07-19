// tokens/base.rs
use crate::networks::evm::EVMNetwork;
use crate::networks::{NetworkClient, NetworkRegistry};
use crate::tokens::{PaymentDetails, TokenHandler, TokenRegistry};
use async_trait::async_trait;
use std::sync::Arc;
use sqlx::PgPool;
use uuid::Uuid;
use chrono::{Utc, Duration};

pub fn register(registry: &mut TokenRegistry, networks: Arc<NetworkRegistry>) {
    let handler = BaseHandler {
        network: networks.evm_chain(8453), // Base mainnet
    };

    registry.register_token(
        "USDC_BASE",
        "USD Coin (Base)",
        "USDC stablecoin hosted natively on the Base Layer-2 network.",
        "Requires 5 network confirmations.",
        handler,
    );
}

pub struct BaseHandler {
    network: Arc<EVMNetwork>,
}

#[async_trait]
impl TokenHandler for BaseHandler {
    fn token_id(&self) -> &str { "USDC_BASE" }

    async fn create_invoice_payment(
        &self, pool: &PgPool, merchant_id: Uuid, invoice_id: Uuid,
        amount: rust_decimal::Decimal, _token_id: &str,
    ) -> Result<PaymentDetails, String> {
        let merchant_mnemonic = "test test test test test test test test test test test junk";

        let (deposit_address, derived_wallet_index, payment_reference) = self.network
            .get_derive_address(pool, merchant_id, invoice_id, merchant_mnemonic)
            .await
            .map_err(|e| format!("Address derivation failed: {e}"))?;

        let expires_at = Utc::now() + Duration::minutes(30);

        sqlx::query!(
            r#"UPDATE invoices SET wallet_address = $1, wallet_index = $2, expires_at = $3, payment_reference = $4 WHERE id = $5"#,
            deposit_address, derived_wallet_index as i32, expires_at, payment_reference, invoice_id
        ).execute(pool).await.map_err(|e| format!("DB update failed: {e}"))?;

        Ok(PaymentDetails {
            invoice_id,
            network: "base".to_string(),
            deposit_address,
            token_address: Some("0x833589fCD6eDb3E08f4c7C32D4f71b54bda02913".to_string()),
            decimals: 6,
            required_confirmations: 5,
            wallet_index: derived_wallet_index,
            expires_at,
        })
    }

    async fn cancel_payment(&self, _pool: &PgPool, invoice_id: Uuid) -> Result<(), String> {
        println!("BaseHandler::cancel_payment({invoice_id})");
        Ok(())
    }
}