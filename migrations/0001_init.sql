-- =========================================================
-- 1. Merchants
-- =========================================================
CREATE TABLE IF NOT EXISTS merchants (
                                         id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                                         name                     VARCHAR(255) NOT NULL,
                                         slug                     VARCHAR(100) NOT NULL UNIQUE,
                                         password_hash            TEXT NOT NULL,
                                         api_key_id               VARCHAR(64) NOT NULL UNIQUE,
                                         api_key_secret_hash      TEXT NOT NULL,
                                         webhook_url              TEXT,
                                         webhook_secret_encrypted BYTEA,
                                         webhook_secret_nonce     BYTEA,
                                         status                   VARCHAR(20) NOT NULL DEFAULT 'active'
                                             CHECK (status IN ('active', 'suspended', 'disabled')),
                                         created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
                                         updated_at               TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- =========================================================
-- 2. Merchant key material
-- =========================================================
CREATE TABLE IF NOT EXISTS merchant_key_material (
                                                     id                  UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                                                     merchant_id         UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                                                     key_family          VARCHAR(50) NOT NULL,
                                                     encrypted_secret    BYTEA NOT NULL,
                                                     encryption_nonce    BYTEA NOT NULL,
                                                     encryption_version  SMALLINT NOT NULL DEFAULT 1,
                                                     created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
                                                     UNIQUE (merchant_id, key_family)
);

-- =========================================================
-- 3. Merchant network indices
-- =========================================================
CREATE TABLE IF NOT EXISTS merchant_network_indices (
                                                        merchant_id    UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                                                        network        VARCHAR(50) NOT NULL,
                                                        account_index  INT NOT NULL DEFAULT 0,
                                                        next_index     INT NOT NULL DEFAULT 1,
                                                        updated_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
                                                        PRIMARY KEY (merchant_id, network, account_index)
);

-- =========================================================
-- 4. Merchant wallets
-- =========================================================
CREATE TABLE IF NOT EXISTS merchant_wallets (
                                                merchant_id   UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                                                network_type  VARCHAR(20) NOT NULL,
                                                address       VARCHAR(255) NOT NULL,
                                                created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
                                                PRIMARY KEY (merchant_id, network_type)
);

-- =========================================================
-- 5. Invoices
-- =========================================================
CREATE TABLE IF NOT EXISTS invoices (
                                        id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                                        merchant_id              UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                                        token_id                 VARCHAR(100) NOT NULL,
                                        token_address            VARCHAR(255),
                                        token_program            VARCHAR(255),
                                        amount_requested         NUMERIC(78, 0) NOT NULL,
                                        amount_received          NUMERIC(78, 0) NOT NULL DEFAULT 0,
                                        wallet_address           VARCHAR(255) NOT NULL,
                                        wallet_index             INT NOT NULL,
                                        payment_reference        VARCHAR(255),
                                        tx_hash                  VARCHAR(255),
                                        status                   VARCHAR(50) NOT NULL DEFAULT 'pending'
                                            CHECK (status IN ('pending', 'paid', 'underpaid', 'overpaid', 'expired')),
                                        data                     TEXT,
                                        created_at               TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
                                        expires_at               TIMESTAMP WITH TIME ZONE NOT NULL,
                                        updated_at               TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
                                        created_block            BIGINT,
                                        required_confirmations   SMALLINT,
                                        token_decimals           SMALLINT,
                                        network_type             VARCHAR(20),
                                        chain_ref                VARCHAR(50)
);

-- Hot-path indexes
CREATE INDEX IF NOT EXISTS invoices_sol_watch_idx
    ON invoices (network_type, chain_ref, status, expires_at);

CREATE INDEX IF NOT EXISTS invoices_wallet_addr_idx
    ON invoices (network_type, chain_ref, wallet_address);

CREATE INDEX IF NOT EXISTS invoices_payment_ref_idx
    ON invoices (network_type, chain_ref, payment_reference);

-- expire_invoices sweep
CREATE INDEX IF NOT EXISTS invoices_expiry_idx
    ON invoices (network_type, chain_ref, expires_at)
    WHERE status IN ('pending', 'underpaid');

-- =========================================================
-- 6. Payments
-- =========================================================
CREATE TABLE IF NOT EXISTS payments (
                                        id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                                        invoice_id     UUID NOT NULL REFERENCES invoices(id) ON DELETE CASCADE,
                                        tx_hash        VARCHAR(255) NOT NULL,
                                        amount         NUMERIC(78, 0) NOT NULL,
                                        block_number   BIGINT NOT NULL,
                                        block_hash     VARCHAR(255) NOT NULL,
                                        confirmations  INT NOT NULL DEFAULT 0,
                                        status         VARCHAR(50) NOT NULL DEFAULT 'detected'
                                            CHECK (status IN ('detected', 'merchant_confirmed', 'system_confirmed', 'orphaned')),
                                        payment_path   VARCHAR(16)
                                            CHECK (payment_path IS NULL OR payment_path IN ('direct', 'reference')),
                                        created_at     TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP,
                                        updated_at     TIMESTAMP WITH TIME ZONE NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- ON CONFLICT target (only one copy needed; old duplicate payments_invoice_tx_uniq dropped)
CREATE UNIQUE INDEX IF NOT EXISTS payments_invoice_tx_uq
    ON payments (invoice_id, tx_hash);

CREATE INDEX IF NOT EXISTS payments_invoice_status_idx
    ON payments (invoice_id, status);

-- reconcile_statuses only ever reads open payments
CREATE INDEX IF NOT EXISTS payments_open_idx
    ON payments (invoice_id, block_number)
    WHERE status IN ('detected', 'merchant_confirmed');

-- tx_hash pre-filter in tick()
CREATE INDEX IF NOT EXISTS payments_txhash_idx
    ON payments (tx_hash);

-- =========================================================
-- 7. Network scan state
-- =========================================================
CREATE TABLE IF NOT EXISTS network_scan_state (
                                                  network_type     VARCHAR(20) NOT NULL,
                                                  chain_ref        VARCHAR(50) NOT NULL,
                                                  scope            VARCHAR(20) NOT NULL,
                                                  last_block       BIGINT NOT NULL,
                                                  last_block_hash  VARCHAR(255) NOT NULL,
                                                  updated_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
                                                  PRIMARY KEY (network_type, chain_ref, scope)
);

-- =========================================================
-- 8. Network seen blocks
-- =========================================================
CREATE TABLE IF NOT EXISTS network_seen_blocks (
                                                   network_type  VARCHAR(20) NOT NULL,
                                                   chain_ref     VARCHAR(50) NOT NULL,
                                                   scope         VARCHAR(20) NOT NULL,
                                                   block_number  BIGINT NOT NULL,
                                                   block_hash    VARCHAR(255) NOT NULL,
                                                   parent_hash   VARCHAR(255) NOT NULL,
                                                   seen_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
                                                   PRIMARY KEY (network_type, chain_ref, scope, block_number)
);

-- =========================================================
-- 9. Network address cursors (SOL watcher)
-- =========================================================
CREATE TABLE IF NOT EXISTS network_address_cursors (
                                                       network_type    VARCHAR(20) NOT NULL,
                                                       chain_ref       VARCHAR(50) NOT NULL,
                                                       address         VARCHAR(255) NOT NULL,
                                                       last_signature  VARCHAR(128) NOT NULL,
                                                       last_slot       BIGINT NOT NULL,
                                                       updated_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
                                                       PRIMARY KEY (network_type, chain_ref, address)
);

-- =========================================================
-- 10. Webhook events
-- =========================================================
CREATE TABLE IF NOT EXISTS webhook_events (
                                              id                   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                                              merchant_id          UUID NOT NULL REFERENCES merchants(id) ON DELETE CASCADE,
                                              url                  TEXT NOT NULL,
                                              event_type           VARCHAR(100) NOT NULL,
                                              event_data           JSONB NOT NULL,
                                              dedupe_key           VARCHAR(255) NOT NULL,
                                              status               VARCHAR(20) NOT NULL DEFAULT 'pending'
                                                  CHECK (status IN ('pending', 'processing', 'completed', 'failed', 'dead', 'cancelled')),
                                              attempt_count        SMALLINT NOT NULL DEFAULT 0,
                                              max_attempts         SMALLINT NOT NULL DEFAULT 10,
                                              next_attempt_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
                                              last_attempt_at      TIMESTAMPTZ,
                                              last_response_code   SMALLINT,
                                              last_error           TEXT,
                                              locked_at            TIMESTAMPTZ,
                                              locked_by            VARCHAR(100),
                                              created_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
                                              updated_at           TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX IF NOT EXISTS webhook_events_dedupe_uniq
    ON webhook_events (merchant_id, dedupe_key);

CREATE INDEX IF NOT EXISTS webhook_events_ready_idx
    ON webhook_events (next_attempt_at)
    WHERE status = 'pending';

-- =========================================================
-- 11. Webhook delivery attempts
-- =========================================================
CREATE TABLE IF NOT EXISTS webhook_delivery_attempts (
                                                         id                 UUID PRIMARY KEY DEFAULT gen_random_uuid(),
                                                         webhook_event_id   UUID NOT NULL REFERENCES webhook_events(id) ON DELETE CASCADE,
                                                         attempt_number     SMALLINT NOT NULL,
                                                         response_code      SMALLINT,
                                                         error              TEXT,
                                                         duration_ms        INT,
                                                         attempted_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS webhook_delivery_attempts_event_idx
    ON webhook_delivery_attempts (webhook_event_id);
