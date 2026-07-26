use axum::{extract::State, http::StatusCode, Json};
use serde::Serialize;
use uuid::Uuid;

use crate::{tokens::TokenMetadata, AppState};

// ==========================================
// 1. TOKENS ENDPOINT
// ==========================================

/// GET /api/test/tokens
/// Returns all metadata for currently registered token handlers in the system.
pub async fn list_tokens_test_handler(
    State(state): State<AppState>,
) -> Json<Vec<TokenMetadata>> {
    let tokens = state.registry.get_metadata();
    Json(tokens)
}

// ==========================================
// 2. NETWORKS ENDPOINT
// ==========================================

#[derive(Serialize)]
pub struct NetworkSummaryResponse {
    pub evm_chain_ids: Vec<u64>,
    pub solana_clusters: Vec<String>,
    pub bitcoin_networks: Vec<String>,
}

/// GET /api/test/networks
/// Returns active blockchain network instances registered from environment configuration.
pub async fn list_networks_test_handler(
    State(state): State<AppState>,
) -> Json<NetworkSummaryResponse> {
    let evm_chain_ids = state.networks.evm.keys().copied().collect();
    let solana_clusters = state
        .networks
        .sol
        .keys()
        .map(|cluster| format!("{:?}", cluster))
        .collect();
    let bitcoin_networks = state
        .networks
        .esplora
        .keys()
        .map(|net| format!("{:?}", net))
        .collect();

    Json(NetworkSummaryResponse {
        evm_chain_ids,
        solana_clusters,
        bitcoin_networks,
    })
}

// ==========================================
// 3. MERCHANTS ENDPOINT
// ==========================================

#[derive(Serialize)]
pub struct MerchantSummaryResponse {
    pub id: Uuid,
    pub name: String,
    pub slug: String,
    pub api_key_id: String,
    pub webhook_url: Option<String>,
}

/// GET /api/test/merchants
/// Fetches registered merchant accounts from the database for quick inspection.
pub async fn list_merchants_test_handler(
    State(state): State<AppState>,
) -> Result<Json<Vec<MerchantSummaryResponse>>, (StatusCode, String)> {
    let merchants = sqlx::query_as!(
        MerchantSummaryResponse,
        r#"
        SELECT id, name, slug, api_key_id, webhook_url
        FROM merchants
        ORDER BY id DESC
        "#
    )
        .fetch_all(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("Failed to list merchants: {e}")))?;

    Ok(Json(merchants))
}

// ==========================================
// 4. COMBINED SYSTEM OVERVIEW (BONUS)
// ==========================================

#[derive(Serialize)]
pub struct SystemTestOverview {
    pub tokens: Vec<TokenMetadata>,
    pub networks: NetworkSummaryResponse,
    pub total_merchants: i64,
}

/// GET /api/test/overview
/// Single aggregator payload for inspecting overall application state.
pub async fn test_overview_handler(
    State(state): State<AppState>,
) -> Result<Json<SystemTestOverview>, (StatusCode, String)> {
    let tokens = state.registry.get_metadata();

    let networks = NetworkSummaryResponse {
        evm_chain_ids: state.networks.evm.keys().copied().collect(),
        solana_clusters: state
            .networks
            .sol
            .keys()
            .map(|c| format!("{:?}", c))
            .collect(),
        bitcoin_networks: state
            .networks
            .esplora
            .keys()
            .map(|n| format!("{:?}", n))
            .collect(),
    };

    let total_merchants = sqlx::query_scalar!(
        r#"SELECT COUNT(*) FROM merchants"#
    )
        .fetch_one(&state.pool)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("DB query failed: {e}")))?
        .unwrap_or(0);

    Ok(Json(SystemTestOverview {
        tokens,
        networks,
        total_merchants,
    }))
}