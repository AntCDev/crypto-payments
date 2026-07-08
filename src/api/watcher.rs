use axum::{
    extract::State,
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::env;

use crate::networks::watch_factory;
use crate::services::watcher::CryptoWatcher;

#[derive(Deserialize)]
pub struct BalanceRequest {
    pub network: String,
    pub currency: String,
    pub address: String,
}

#[derive(Serialize)]
pub struct BalanceResponse {
    pub balance: f64,
    pub currency: String,
}

pub async fn get_balance_handler(
    State(_pool): State<PgPool>, // Axum requires this to match your router's state type
    Json(payload): Json<BalanceRequest>,
) -> Result<Json<BalanceResponse>, (StatusCode, String)> {
    let network_lower = payload.network.to_lowercase();
    let currency_lower = payload.currency.to_lowercase();

    // 1. Resolve RPC URL, Token Address, and Decimals based on User input
    let (rpc_url, token_address, decimals) = match network_lower.as_str() {
        "ethereum" | "eth" => {
            let url = env::var("ETHEREUM_RPC_URL")
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "ETHEREUM_RPC_URL is not configured".to_string()))?;

            match currency_lower.as_str() {
                "eth" => (url, None, 18),
                "usdc" => (
                    url,
                    Some("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string()), // Hardcoded Mainnet USDC Address
                    6,
                ),
                _ => return Err((StatusCode::BAD_REQUEST, format!("Unsupported currency '{}' for Ethereum in this test", payload.currency))),
            }
        }
        "solana" | "sol" => {
            let _url = env::var("SOLANA_RPC_URL")
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "SOLANA_RPC_URL is not configured".to_string()))?;
            return Err((StatusCode::NOT_IMPLEMENTED, "Solana balance checks are not implemented yet".to_string()));
        }
        "bitcoin" | "btc" => {
            let _url = env::var("BITCOIN_RPC_URL")
                .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "BITCOIN_RPC_URL is not configured".to_string()))?;
            return Err((StatusCode::NOT_IMPLEMENTED, "Bitcoin balance checks are not implemented yet".to_string()));
        }
        _ => return Err((StatusCode::BAD_REQUEST, format!("Unsupported network '{}'", payload.network))),
    };

    // 2. Generate the appropriate watcher using your existing factory pattern
    let watcher = watch_factory(
        &network_lower,
        &rpc_url,
        token_address.as_deref(),
        decimals,
    )
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Factory generation failed: {}", e)))?;

    // 3. Query the balance via the trait method
    let balance = watcher
        .get_balance(&payload.address)
        .await
        .map_err(|e| (StatusCode::BAD_REQUEST, format!("Failed to fetch balance: {}", e)))?;

    Ok(Json(BalanceResponse {
        balance,
        currency: currency_lower,
    }))
}