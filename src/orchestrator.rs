use crate::tokens::TokenRegistry;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;
use rust_decimal::Decimal;
use chrono::{Utc, Duration};

pub struct PaymentOrchestrator {
    pool: PgPool,
    registry: Arc<TokenRegistry>,
}

impl PaymentOrchestrator {
    pub fn new(pool: PgPool, registry: Arc<TokenRegistry>) -> Self {
        Self { pool, registry }
    }

    /// Handles complete database persistence and delegates key updates to token handlers
    // Inside /orchestrator.rs -> PaymentOrchestrator::create_invoice
    pub async fn create_invoice(
        &self,
        merchant_id: Uuid,
        token_id: &str,
        amount_requested: rust_decimal::Decimal,
        data: Option<String>,
    ) -> Result<Uuid, String> {

        // 1. Determine temporary expiration window
        let default_expiration = chrono::Utc::now() + chrono::Duration::hours(1);

        // 2. Insert initial database skeleton
        let row = sqlx::query!(
        r#"
        INSERT INTO invoices (merchant_id, token_id, amount_requested, wallet_address, wallet_index, expires_at, status, data)
        VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
        RETURNING id
        "#,
        merchant_id,
        token_id,
        amount_requested,
        "", // placeholder
        0,  // placeholder
        default_expiration,
        "pending",
        data
    )
            .fetch_one(&self.pool)
            .await
            .map_err(|e| e.to_string())?;

        let invoice_id = row.id;

        // 3. Resolve the specific network handler (BaseUsdcHandler or EthHandler)
        let handler = self
            .registry
            .get_handler(token_id)
            .ok_or_else(|| format!("No handler registered for token {}", token_id))?;

        // 4. Delegate database update & watcher creation straight to the handler
        let details = handler
            .create_invoice_payment(&self.pool, invoice_id, amount_requested)
            .await?;

        println!("Invoice provisioning complete: {:?}", details);
        Ok(invoice_id)
    }
}