use serde::{Deserialize, Serialize};
use uuid::Uuid;
use rust_decimal::Decimal;
use crate::AppState;

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Redirect,
    Json,
};
use chrono::{DateTime, Utc};


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



#[derive(Deserialize)]
pub struct InvoiceRedirectQuery {
    pub id: Uuid,
}

/// GET /invoice?id=<invoice_id>
pub async fn invoice_redirect_handler(
    State(_state): State<AppState>,
    Query(query): Query<InvoiceRedirectQuery>,
) -> Redirect {
    // TODO: once per-merchant / per-chain checkout pages exist, branch here.
    // e.g. match invoice.network_type.as_str() {
    //     "evm" => Redirect::to(&format!("/EVM.html?id={}", query.id)),
    //     "solana" => Redirect::to(&format!("/SOL.html?id={}", query.id)),
    //     _ => Redirect::to(&format!("/invoices.html?id={}", query.id)),
    // }
    Redirect::to(&format!("/invoices.html?id={}", query.id))
}

// =====================================================================
// GET /api/invoices/:id
// =====================================================================
// Returns the checkout-relevant subset of an invoice plus its associated
// payment attempts, and a (currently dummy) wallet-connect command the
// frontend can use to prompt the wallet.
// =====================================================================

#[derive(sqlx::FromRow)]
struct InvoiceRow {
    merchant_id: Uuid,
    token_id: String,
    token_address: Option<String>,
    amount_requested: Decimal,
    amount_received: Decimal,
    wallet_address: String,
    payment_reference: String,
    status: String,
    created_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    required_confirmations: i16,
}

#[derive(Serialize)]
pub struct InvoiceDetailsResponse {
    pub merchant_id: Uuid,
    pub token_id: String,
    pub token_address: Option<String>, // Changed from String to Option<String>
    pub amount_requested: Decimal,
    pub amount_received: Decimal,
    pub wallet_address: String,
    pub payment_reference: String,
    pub status: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub required_confirmations: i16,
    pub payments: Vec<PaymentSummary>,
    pub wallet_connect_command: String,
}

#[derive(sqlx::FromRow, Serialize)]
struct PaymentSummary {
    amount: Decimal,
    confirmations: i32,
    status: String,
}

/// GET /api/invoices/:id
pub async fn get_invoice_handler(
    State(state): State<AppState>,
    Path(invoice_id): Path<Uuid>,
) -> Result<Json<InvoiceDetailsResponse>, (StatusCode, String)> {
    let invoice = sqlx::query_as::<_, InvoiceRow>(
        r#"
        SELECT
            merchant_id,
            token_id,
            token_address,
            amount_requested,
            amount_received,
            wallet_address,
            payment_reference,
            status,
            created_at,
            expires_at,
            required_confirmations
        FROM invoices
        WHERE id = $1
        "#,
    )
        .bind(invoice_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .ok_or((StatusCode::NOT_FOUND, "invoice not found".to_string()))?;

    let payments = sqlx::query_as::<_, PaymentSummary>(
        r#"
        SELECT amount, confirmations, status
        FROM payments
        WHERE invoice_id = $1
        ORDER BY created_at ASC
        "#,
    )
        .bind(invoice_id)
        .fetch_all(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    Ok(Json(InvoiceDetailsResponse {
        merchant_id: invoice.merchant_id,
        token_id: invoice.token_id,
        token_address: invoice.token_address, // Maps Option<String> directly
        amount_requested: invoice.amount_requested,
        amount_received: invoice.amount_received,
        wallet_address: invoice.wallet_address,
        payment_reference: invoice.payment_reference,
        status: invoice.status,
        created_at: invoice.created_at,
        expires_at: invoice.expires_at,
        required_confirmations: invoice.required_confirmations,
        payments,
        // TODO: replace with real wallet-connect payload once that flow is built.
        wallet_connect_command: "DUMMY_COMMAND".to_string(),
    }))
}