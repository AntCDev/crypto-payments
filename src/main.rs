use axum::{routing::get, Router};
use sqlx::postgres::PgPoolOptions;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::compression::CompressionLayer;
use std::env;
use std::sync::Arc;

// Register our modules globally
mod api;
mod models;
mod networks;
mod tokens;
mod orchestrator;

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::PgPool,
    pub registry: Arc<tokens::TokenRegistry>,
}

#[tokio::main]
async fn main() {
    dotenvy::dotenv().ok();

    let database_url = env::var("DATABASE_URL")
        .expect("DATABASE_URL environment variable must be set in .env");

    let pool = PgPoolOptions::new()
        .min_connections(10)
        .max_connections(100)
        .connect(&database_url)
        .await
        .expect("Failed to connect to PostgreSQL");

    let registry = Arc::new(tokens::TokenRegistry::new());

    let state = AppState { pool, registry };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/api/tokens", get(api::watcher::list_tokens_handler))

        // Middleware
        .fallback_service(ServeDir::new("wwwroot"))
        .layer(cors)
        .layer(CompressionLayer::new())
        .with_state(state);

    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .expect("PORT must be a valid number");

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("Server booted up cleanly on http://{}", addr);

    axum::serve(listener, app).await.unwrap();
}