use sqlx::postgres::PgPoolOptions;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tower_http::compression::CompressionLayer;
use std::env;
use std::sync::Arc;
use axum::{routing::{get, post}, Router};

// Register our modules globally
mod networks;
mod tokens;
mod api;
mod orchestrator;

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::PgPool,
    pub networks: Arc<networks::NetworkRegistry>,
    pub registry: Arc<tokens::TokenRegistry>,
    pub orchestrator: Arc<orchestrator::PaymentOrchestrator>,
}

async fn initialize_database(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    // 1. Create Invoices Table
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS invoices (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            merchant_id UUID NOT NULL,
            token_id VARCHAR(100) NOT NULL,
            amount_requested NUMERIC(78, 0) NOT NULL,
            amount_received NUMERIC(78, 0) NOT NULL DEFAULT 0,
            wallet_address VARCHAR(255) NOT NULL,
            wallet_index INT NOT NULL,
            payment_reference VARCHAR(255),
            tx_hash VARCHAR(255),
            status VARCHAR(50) NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'paid', 'underpaid', 'overpaid', 'expired')),
            data TEXT,
            created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
            expires_at TIMESTAMP WITH TIME ZONE NOT NULL,
            updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        "#
    )
    .execute(pool)
    .await?;

    // 2. Create Payments Table
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS payments (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            invoice_id UUID NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
            tx_hash VARCHAR(255) NOT NULL,
            amount NUMERIC(78, 0) NOT NULL,
            block_number BIGINT NOT NULL,
            block_hash VARCHAR(255) NOT NULL,
            confirmations INT NOT NULL DEFAULT 0,
            status VARCHAR(50) NOT NULL DEFAULT 'detected' CHECK (status IN ('detected', 'merchant_confirmed', 'system_confirmed', 'orphaned')),
            created_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
            updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP
        );
        "#
    )
    .execute(pool)
    .await?;

    // 3. Create index tables
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS merchant_network_indices (
            merchant_id UUID NOT NULL,
            network VARCHAR(100) NOT NULL, -- e.g., 'ethereum', 'arbitrum', 'tron'
            next_index INT NOT NULL DEFAULT 0,
            updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
            PRIMARY KEY (merchant_id, network)
        );
        "#
    )
    .execute(pool)
    .await?;

    println!("Database tables initialized successfully (or already exist).");
    Ok(())
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

    initialize_database(&pool)
        .await
        .expect("Failed to initialize database tables");

    // 1. Instantiate the networks ONCE on load
    let networks = Arc::new(networks::NetworkRegistry::from_env());

    // 2. Pass the singletons down to the token registry so handlers can clone the Arcs
    let registry = Arc::new(tokens::TokenRegistry::new(networks.clone()));

    // 3. Instantiate Orchestrator and pass the required dependencies
    let orchestrator = Arc::new(orchestrator::PaymentOrchestrator::new(
        pool.clone(),
        registry.clone()
    ));

    let state = AppState {
        pool,
        networks,
        registry,
        orchestrator
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/api/tokens", get(api::watcher::list_tokens_handler))
        .route("/api/invoices", post(api::invoices::create_invoice_handler))
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