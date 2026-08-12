
-- =========================================================
-- 12. Checkout views (catalogue of frontend files, by ID)
-- =========================================================
CREATE TABLE IF NOT EXISTS checkout_views (
                                              id           VARCHAR(64)  PRIMARY KEY,          -- 'evm', 'sol', 'esplora'
    path         TEXT         NOT NULL,             -- '/checkout/evm.html'
    description  TEXT,
    created_at   TIMESTAMPTZ  NOT NULL DEFAULT now(),
    updated_at   TIMESTAMPTZ  NOT NULL DEFAULT now(),
    -- must be a site-relative path: blocks '//evil.com' and 'https://evil.com'
    -- from turning the redirect handler into an open redirect if this table
    -- is ever writable from a dashboard.
    CONSTRAINT checkout_views_path_relative
    CHECK (path LIKE '/%' AND path NOT LIKE '//%')
    );

-- =========================================================
-- 13. Token -> checkout view mapping
-- =========================================================
CREATE TABLE IF NOT EXISTS token_checkout_views (
                                                    token_id    VARCHAR(100) PRIMARY KEY,
    view_id     VARCHAR(64)  NOT NULL
    REFERENCES checkout_views(id) ON UPDATE CASCADE ON DELETE RESTRICT,
    updated_at  TIMESTAMPTZ  NOT NULL DEFAULT now()
    );

CREATE INDEX IF NOT EXISTS token_checkout_views_view_idx
    ON token_checkout_views (view_id);