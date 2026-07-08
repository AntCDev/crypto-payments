use axum::{routing::post, Router};
use sqlx::postgres::PgPoolOptions;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::compression::CompressionLayer;
use std::env;

// Register our modules globally
mod api;
mod models;
mod services;
mod networks;

// Thread-safe Global Constants (equivalent to C# static readonly HashSets)
#[tokio::main]
async fn main() {
    // 1. Load the .env file from the Cargo.toml directory
    dotenvy::dotenv().ok();

    // 2. Fetch the DATABASE_URL environment variable
    let database_url = env::var("DATABASE_URL")
        .expect("DATABASE_URL environment variable must be set in .env");

    let pool = PgPoolOptions::new()
        .min_connections(10)
        .max_connections(100)
        .connect(&database_url)
        .await
        .expect("Failed to connect to PostgreSQL");

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        // .route("/Api/CreateInvoice", post(api::payments::create_invoice))
        .route("/api/balance", post(api::watcher::get_balance_handler))
        
        // Middleware
        .fallback_service(ServeDir::new("wwwroot"))
        .layer(cors)
        .layer(CompressionLayer::new())
        .with_state(pool);

    // 3. Fetch and parse the PORT environment variable (fallback to 3000 if not found)
    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .expect("PORT must be a valid number");

    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    println!("Server booted up cleanly on http://{}", addr);

    axum::serve(listener, app).await.unwrap();
}