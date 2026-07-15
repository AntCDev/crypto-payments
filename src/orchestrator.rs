// orchestrator.rs
use crate::tokens::TokenRegistry;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

pub struct PaymentOrchestrator {
    pool: PgPool,
    registry: Arc<TokenRegistry>,
}

impl PaymentOrchestrator {
    pub fn new(pool: PgPool, registry: Arc<TokenRegistry>) -> Self {
        Self { pool, registry }
    }

    pub async fn create_invoice(
        &self,
        token_id: &str,
        amount: rust_decimal::Decimal,
        // metadata: InvoiceMetadata,
    ) -> Result<String, String> {
        let invoice_id = Uuid::new_v4().to_string();
        println!("db.insert_invoice({invoice_id}, {token_id}, {amount}, Pending) using self.pool");

        let handler = self
            .registry
            .get_handler(token_id)
            .ok_or_else(|| format!("no handler registered for token {token_id}"))?;

        let details = handler.create_invoice_payment(&invoice_id, amount).await?;
        println!("db.insert_payment_details({:?}) using self.pool", details);

        Ok(invoice_id)
    }
}