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
    // 1. Create Merchants Table (Must be created first as invoices and key material reference it)
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS merchants (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            name VARCHAR(255) NOT NULL,
            slug VARCHAR(100) NOT NULL UNIQUE,

            -- dashboard login
            password_hash TEXT NOT NULL,              -- argon2id

            -- API auth (Stripe-style pk_/sk_ pair)
            api_key_id VARCHAR(64) NOT NULL UNIQUE,   -- public identifier, sent on every request
            api_key_secret_hash TEXT NOT NULL,        -- argon2id of the secret; shown once at creation, never again

            -- outbound webhooks: needs to be reversible, you sign the payload yourself
            webhook_url TEXT,
            webhook_secret_encrypted BYTEA,           -- AES-GCM(MASTER_KEY-derived, secret)
            webhook_secret_nonce BYTEA,

            status VARCHAR(20) NOT NULL DEFAULT 'active'
                CHECK (status IN ('active', 'suspended', 'disabled')),

            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        "#
    )
        .execute(pool)
        .await?;

    // 2. Create Invoices Table
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS invoices (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            merchant_id UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
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

    // 3. Create Payments Table
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

    // 4. Create Merchant Key Material Table
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS merchant_key_material (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            merchant_id UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,

            -- 'bip39' covers secp256k1 (BIP32) + ed25519 (SLIP-0010) derivation from one seed.
            -- 'raw_ed25519' / 'raw_secp256k1' etc. reserved for a future network that can't
            -- derive from the standard tree at all.
            key_family VARCHAR(50) NOT NULL,

            encrypted_secret BYTEA NOT NULL,   -- AES-256-GCM ciphertext of the mnemonic/seed
            encryption_nonce BYTEA NOT NULL,
            encryption_version SMALLINT NOT NULL DEFAULT 1,  -- lets you rotate schemes later

            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            UNIQUE (merchant_id, key_family)
        );
        "#
    )
        .execute(pool)
        .await?;

    // 5. Create Merchant Network Indices Table
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS merchant_network_indices (
            merchant_id UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
            network VARCHAR(50) NOT NULL,       -- 'EVM', 'SOL', 'ESPLORA', future 'TRON'...
            account_index INT NOT NULL DEFAULT 0,
            next_index INT NOT NULL DEFAULT 0,
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            PRIMARY KEY (merchant_id, network, account_index)
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
    println!("==================================================");
    println!("🚀 Booting Payment Gateway...");
    println!("==================================================");

    dotenvy::dotenv().ok();

    let database_url = env::var("DATABASE_URL")
        .expect("DATABASE_URL environment variable must be set in .env");

    print!("🐘 Connecting to Database...");
    let pool = PgPoolOptions::new()
        .min_connections(10)
        .max_connections(100)
        .connect(&database_url)
        .await
        .expect("Failed to connect to PostgreSQL");
    println!(" Done.");

    print!("⚙️ Initializing Database Schema...");
    initialize_database(&pool)
        .await
        .expect("Failed to initialize database tables");
    println!(" Done.");

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
        .route("/api/merchants", post(api::merchants::signup_merchant_handler))

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

    println!("\n==================================================");
    println!("⚡ Server booted up cleanly on http://{}", addr);
    println!("==================================================\n");

    axum::serve(listener, app).await.unwrap();
}