use crate::networks::evm::EVMNetwork;
use crate::networks::NetworkRegistry;
use crate::tokens::{PaymentDetails, TokenHandler, TokenRegistry};
use async_trait::async_trait;
use std::sync::Arc;
use sqlx::PgPool;
use uuid::Uuid;
use chrono::{Utc, Duration};
use crate::networks::NetworkClient;

pub fn register(registry: &mut TokenRegistry, networks: Arc<NetworkRegistry>) {
    // Clone the underlying EVM network instance pointer (zero allocation cost)
    let handler = BaseUsdcHandler {
        network: networks.evm.clone(),
    };

    registry.register_token(
        "USDC_BASE",
        "USD Coin (Base)",
        "USDC stablecoin hosted natively on the Base Layer-2 network.",
        "Requires 5 network confirmations.",
        handler,
    );
}

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
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        amount: rust_decimal::Decimal,
    ) -> Result<PaymentDetails, String> {
        println!("BaseUsdcHandler::create_invoice_payment(invoice: {invoice_id}, amount: {amount})");

        // 1 & 2. ACQUIRE INDEX & DERIVE DEPOSIT ADDRESS
        let merchant_mnemonic = "test test test test test test test test test test test junk";

        // Uses the globally shared singleton network client instance directly
        let (deposit_address, derived_wallet_index) = self
            .network
            .get_derive_address(pool, merchant_id, merchant_mnemonic)
            .await
            .map_err(|e| format!("Address derivation failed: {}", e))?;

        // 2.5 Get payment referece (Memo, Smart Contract Id, ETC...)
        
        
        // 3. DEFINE EXPIRATION
        let expires_at = Utc::now() + Duration::minutes(30);
        

        // 4. UPDATE THE INVOICE RECORD
        sqlx::query!(
            r#"
            UPDATE invoices
            SET wallet_address = $1, wallet_index = $2, expires_at = $3
            WHERE id = $4
            "#,
            deposit_address,
            derived_wallet_index as i32,
            expires_at,
            invoice_id
        )
            .execute(pool)
            .await
            .map_err(|e| format!("Failed to update database invoice record: {}", e))?;

        // 5. REGISTER CHAIN WATCHER
        // (You can invoke self.network.register_payment(...) here if needed)

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
        println!("BaseUsdcHandler::cancel_payment({invoice_id})");
        // -------------------------------------------------------------------------
        // WHAT HAPPENS HERE:
        // 1. Drop the block scanner / watcher loop looking at this address.
        // 2. Update DB status for the invoice to 'expired' or 'cancelled' if appropriate.
        // -------------------------------------------------------------------------
        Ok(())
    }
}
