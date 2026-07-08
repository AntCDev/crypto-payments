use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct CreateInvoiceRequest {
    pub amount: i32,
    pub currency: String,
    pub network: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct InvoiceResponse {
    pub invoice_id: String,
    pub status: String,
}