use axum::{extract::State, Json};
use crate::AppState;
use crate::tokens::TokenMetadata;

/// GET /api/tokens
/// Returns an aggregated list of all dynamically registered token parameters across networks
pub async fn list_tokens_handler(
    State(state): State<AppState>,
) -> Json<Vec<TokenMetadata>> {
    Json(state.registry.get_metadata())
}