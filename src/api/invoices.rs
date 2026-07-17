use axum::{extract::State, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use rust_decimal::Decimal;
use crate::AppState;

#[derive(Deserialize)]
pub struct CreateInvoiceRequest {
    pub merchant_id: Uuid,
    pub token_id: String,
    pub amount_requested: Decimal,
    pub data: Option<String>,
}

#[derive(Serialize)]
pub struct CreateInvoiceResponse {
    pub url: String,
    pub invoice_id: Uuid,
}

/// POST /api/invoices
/// Accepts payload and delegates execution to the orchestrator layer
pub async fn create_invoice_handler(
    State(state): State<AppState>,
    Json(payload): Json<CreateInvoiceRequest>,
) -> Result<Json<CreateInvoiceResponse>, (StatusCode, String)> {

    // Pass implementation over to the orchestrator
    let invoice_id = state
        .orchestrator
        .create_invoice(
            payload.merchant_id,
            &payload.token_id,
            payload.amount_requested,
            payload.data,
        )
        .await
        .map_err(|err_msg| (StatusCode::INTERNAL_SERVER_ERROR, err_msg))?;

    // Assemble dynamic checkout URL
    let base_url = std::env::var("BASE_URL").unwrap_or_else(|_| "http://localhost:3000".to_string());
    let invoice_url = format!("{}/invoice?id={}", base_url, invoice_id);

    Ok(Json(CreateInvoiceResponse {
        url: invoice_url,
        invoice_id,
    }))
}