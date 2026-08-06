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
            token_address VARCHAR(255),
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
            updated_at TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
            created_block BIGINT,
            required_confirmations SMALLINT,
            token_decimals SMALLINT,
            network_type VARCHAR(20),
            chain_ref VARCHAR(50)
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

    // 3b. Payments needs a uniqueness guarantee so detection is idempotent across restarts.
    sqlx::query(
        r#"
    CREATE UNIQUE INDEX IF NOT EXISTS payments_invoice_tx_uniq
        ON payments (invoice_id, tx_hash);
    "#
    ).execute(pool).await?;


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
            network VARCHAR(50) NOT NULL,
            account_index INT NOT NULL DEFAULT 0,
            next_index INT NOT NULL DEFAULT 1, -- Set default to 1
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            PRIMARY KEY (merchant_id, network, account_index)
        );
        "#
    )
        .execute(pool)
        .await?;

    // 5a. Create Merchant Main Wallets
    sqlx::query(
        r#"
CREATE TABLE IF NOT EXISTS merchant_wallets (
    merchant_id  UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
    network_type VARCHAR(20) NOT NULL,   -- 'evm' / 'solana' / etc.
    address      VARCHAR(255) NOT NULL,  -- lowercase for evm; keep native case for solana
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (merchant_id, network_type)
);
        "#
    )
        .execute(pool)
        .await?;

    // 6. Per-chain scan cursor. This is what makes watch_addresses restart-safe:
    //    we never rely on `self.pending` for "where was I", only for "who do I care about".
    sqlx::query(
        r#"
    CREATE TABLE IF NOT EXISTS network_scan_state (
        network_type      VARCHAR(20) NOT NULL,
        chain_ref         VARCHAR(50) NOT NULL,
        scope             VARCHAR(20) NOT NULL, -- 'addresses' | 'logs'
        last_block        BIGINT NOT NULL,
        last_block_hash   VARCHAR(255) NOT NULL,
        updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
        PRIMARY KEY (network_type, chain_ref, scope)
    );
    "#
    ).execute(pool).await?;

    // 7. The scan cursor tells us "where was I", this tells us "what did I think the chain looked like".
    sqlx::query(
        r#"
    CREATE TABLE IF NOT EXISTS network_seen_blocks (
        network_type  VARCHAR(20) NOT NULL,
        chain_ref     VARCHAR(50) NOT NULL,
        scope         VARCHAR(20) NOT NULL,
        block_number  BIGINT      NOT NULL,
        block_hash    VARCHAR(255) NOT NULL,
        parent_hash   VARCHAR(255) NOT NULL,
        seen_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
        PRIMARY KEY (network_type, chain_ref, scope, block_number)
    );
    "#
    ).execute(pool).await?;


    // 8. Webhook Events Table (transactional outbox for merchant webhook delivery)
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS webhook_events (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            merchant_id UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,

            -- Snapshot the URL at enqueue time. If the merchant edits webhook_url on
            -- merchants mid-retry, you don't want an in-flight event silently
            -- redirecting to wherever they just pointed it.
            url TEXT NOT NULL,

            event_type VARCHAR(100) NOT NULL,   -- e.g. 'invoice.paid', 'invoice.expired'
            event_data JSONB NOT NULL,

            -- Lets the producer (invoice/payment logic) be called twice for the same
            -- underlying event without double-enqueuing. Something like
            -- 'invoice.paid:<invoice_id>' or 'payment.confirmed:<payment_id>'.
            dedupe_key VARCHAR(255) NOT NULL,

            status VARCHAR(20) NOT NULL DEFAULT 'pending'
                CHECK (status IN ('pending', 'processing', 'completed', 'failed', 'dead', 'cancelled')),

            attempt_count SMALLINT NOT NULL DEFAULT 0,
            max_attempts SMALLINT NOT NULL DEFAULT 10,

            next_attempt_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            last_attempt_at TIMESTAMPTZ,

            last_response_code SMALLINT,
            last_error TEXT,

            -- Claim fields so you can run more than one dispatcher instance safely with
            -- SELECT ... FOR UPDATE SKIP LOCKED, instead of relying on a single worker.
            locked_at TIMESTAMPTZ,
            locked_by VARCHAR(100),

            created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        "#
    )
        .execute(pool)
        .await?;

    // 8b. Idempotency guard: same merchant can't get the same logical event enqueued twice.
    sqlx::query(
        r#"
        CREATE UNIQUE INDEX IF NOT EXISTS webhook_events_dedupe_uniq
            ON webhook_events (merchant_id, dedupe_key);
        "#
    )
        .execute(pool)
        .await?;

    // 8c. The one query your dispatcher loop actually runs: "what's due, oldest first".
    //     Partial index keeps it tiny since 'pending' rows are a small fraction of the table.
    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS webhook_events_ready_idx
            ON webhook_events (next_attempt_at)
            WHERE status = 'pending';
        "#
    )
        .execute(pool)
        .await?;

    // 9. Webhook Delivery Attempts Table (append-only history, one row per HTTP call)
    sqlx::query(
        r#"
        CREATE TABLE IF NOT EXISTS webhook_delivery_attempts (
            id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
            webhook_event_id UUID NOT NULL REFERENCES webhook_events(id) ON DELETE CASCADE,
            attempt_number SMALLINT NOT NULL,
            response_code SMALLINT,
            error TEXT,
            duration_ms INT,
            attempted_at TIMESTAMPTZ NOT NULL DEFAULT now()
        );
        "#
    )
        .execute(pool)
        .await?;

    sqlx::query(
        r#"
        CREATE INDEX IF NOT EXISTS webhook_delivery_attempts_event_idx
            ON webhook_delivery_attempts (webhook_event_id);
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

    // 1. Instantiate the networks ONCE on load & spawn payment watchers
    let networks = Arc::new(networks::NetworkRegistry::from_env(pool.clone()));

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

        // Inspection / Test routes
        .route("/api/test/tokens", get(api::tests::list_tokens_test_handler))
        .route("/api/test/networks", get(api::tests::list_networks_test_handler))
        .route("/api/test/merchants", get(api::tests::list_merchants_test_handler))
        .route("/api/test/overview", get(api::tests::test_overview_handler))

        // Middleware
        .fallback_service(ServeDir::new("wwwroot"))
        .layer(cors)
        .layer(CompressionLayer::new())
        .with_state(state);

    let port: u16 = env::var("PORT")
        .unwrap_or_else(|_| "3000".to_string())
        .parse()
        .expect("PORT must be a valid number");

    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let ip: std::net::IpAddr = host.parse().expect("HOST must be a valid IP address");
    let addr = std::net::SocketAddr::from((ip, port));

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    println!("\n==================================================");
    println!("⚡ Server booted up cleanly on http://{}", addr);
    if ip.is_unspecified() {
        println!("🔗 Local access: http://localhost:{} (or http://127.0.0.1:{})", port, port);
    }
    println!("==================================================\n");

    axum::serve(listener, app).await.unwrap();
}