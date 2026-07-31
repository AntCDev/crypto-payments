use super::{Amount, NetworkClient, PaymentWatch};
use async_trait::async_trait;
use uuid::Uuid;

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;

use bip32::{DerivationPath, PrivateKey, XPrv};
use bip39::Mnemonic;
use sha2::Sha256;
use sha3::{Digest, Keccak256};
use sqlx::PgPool;
use std::collections::{HashMap, VecDeque};
use rust_decimal::Decimal;

// ==========================================
// ### PRIVATE RPC STRUCTS ###
// ==========================================
#[derive(Serialize)]
struct RpcRequest {
    jsonrpc: &'static str,
    method: &'static str,
    params: serde_json::Value,
    id: u32,
}

#[derive(Deserialize)]
struct RpcResponse {
    result: Option<String>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    message: String,
}


#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Log {
    pub address: String,
    pub topics: Vec<String>,
    pub data: String,
    #[serde(rename = "blockNumber")]
    pub block_number: String,
    #[serde(rename = "transactionHash")]
    pub transaction_hash: String,
    #[serde(rename = "transactionIndex")]
    pub transaction_index: String,
    #[serde(rename = "blockHash")]
    pub block_hash: String,
    #[serde(rename = "logIndex")]
    pub log_index: String,
    pub removed: bool,
}

#[derive(Deserialize)]
struct RpcResponseLogs {
    result: Option<Vec<Log>>,
    error: Option<RpcError>,
}

struct BlockRecord {
    number: u64,
    hash: String,
}


// ─────────────────────────────────────────────────────────────────────────────
// Tunables. All of these become per-merchant / per-chain config later.
// ─────────────────────────────────────────────────────────────────────────────
const FINAL_CONFIRMATIONS: i64 = 48;    // -> 'system_confirmed', we stop polling it
const POLL_INTERVAL_SECS: u64 = 12;     // ~1 block on mainnet; per-chain later
const MAX_BLOCKS_PER_TICK: u64 = 250;   // catch-up throttle so a long outage doesn't nuke the RPC
const MAX_REORG_DEPTH: u64 = 64;        // how far back we're willing to unwind

const NETWORK_TYPE: &str = "evm";
const SCAN_SCOPE_ADDRESSES: &str = "addresses";

fn hex_to_u64(hex_str: &str) -> u64 {
    u64::from_str_radix(hex_str.trim_start_matches("0x"), 16).unwrap_or(0)
}
fn hex_to_u128(s: &str) -> Result<u128, String> {
    u128::from_str_radix(s.trim_start_matches("0x"), 16)
        .map_err(|e| format!("bad hex u128 '{s}': {e}"))
}

/// Base-unit (wei) -> Decimal. NUMERIC(78,0) can hold a full uint256 but
/// rust_decimal caps out at ~7.9e28, which is fine for wei amounts
/// (that's ~79 billion ETH) but *will* bite on a token with 24 decimals.
/// TODO: move the money type to a u256/BigInt wrapper and bind as string.
fn wei_to_decimal(v: u128) -> Result<rust_decimal::Decimal, String> {
    rust_decimal::Decimal::from_str_exact(&v.to_string()).map_err(|e| format!("amount {v} doesn't fit Decimal: {e}"))
}

#[derive(Clone)]
struct WatchedInvoice {
    invoice_id: Uuid,
    merchant_id: Uuid,
    address_lc: String,
    amount_requested: rust_decimal::Decimal,
    /// Per-invoice merchant threshold, snapshotted at invoice creation.
    /// Falls back to FINAL_CONFIRMATIONS if the merchant never set one.
    required_confirmations: i64,
    /// Don't credit anything that landed before the invoice existed.
    created_block: Option<i64>,
}

/// One canonical block, only the bits we need.
struct BlockView {
    number: u64,
    hash: String,
    parent_hash: String,
    /// (tx_hash, to_lc, value_wei)
    transfers: Vec<(String, String, u128)>,
}

// ==========================================
// ### NETWORK IMPLEMENTATION ###
// ==========================================
pub struct EVMNetwork {
    chain_id: u64,
    pub network_name: String,
    rpc_urls: Vec<String>,
    pub contract_address: Option<String>,
    client: reqwest::Client,
    pending: Mutex<HashMap<Uuid, PaymentWatch>>,
}

impl EVMNetwork {
    const REORG_WINDOW: usize = 64;
    pub fn new(chain_id: u64, rpc_urls: Vec<String>, contract_address: Option<String>) -> Self {
        assert!(!rpc_urls.is_empty(), "EVMNetwork requires at least one RPC URL");
        let network_name = format!("EVM_{}", chain_id);

        Self {
            chain_id,
            network_name,
            rpc_urls,
            contract_address,
            client: reqwest::Client::new(),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn chain_ref(&self) -> String {
        self.chain_id.to_string()
    }
    async fn get_block_number(&self) -> Result<u64, String> {
        let hex = self.call_rpc("eth_blockNumber", serde_json::json!([])).await?;
        u64::from_str_radix(hex.trim_start_matches("0x"), 16)
            .map_err(|e| format!("Failed to parse block number '{hex}': {e}"))
    }

    /// `full = true` pulls the tx bodies so we can do native-value matching in
    /// one round trip instead of N `eth_getTransactionByHash` calls.
    async fn get_block(&self, number: u64, full: bool) -> Result<Option<BlockView>, String> {
        let raw = self.call_rpc_json(
            "eth_getBlockByNumber",
            serde_json::json!([format!("0x{:x}", number), full]),
        ).await?;

        if raw.is_null() {
            // Tip raced ahead of us / provider hasn't got the block yet.
            return Ok(None);
        }
        Self::parse_block(&raw).map(Some)
    }

    fn parse_block(raw: &serde_json::Value) -> Result<BlockView, String> {
        let number = raw.get("number").and_then(|v| v.as_str())
            .map(hex_to_u64).ok_or("block missing number")?;
        let hash = raw.get("hash").and_then(|v| v.as_str())
            .ok_or("block missing hash")?.to_lowercase();
        let parent_hash = raw.get("parentHash").and_then(|v| v.as_str())
            .unwrap_or_default().to_lowercase();

        let mut transfers = Vec::new();
        if let Some(txs) = raw.get("transactions").and_then(|v| v.as_array()) {
            for tx in txs {
                // Contract creations have `to: null` — nothing to match.
                let to = match tx.get("to").and_then(|v| v.as_str()) {
                    Some(t) => t.to_lowercase(),
                    None => continue,
                };
                let value = tx.get("value").and_then(|v| v.as_str())
                    .map(hex_to_u128).transpose()?.unwrap_or(0);
                if value == 0 {
                    continue; // ERC-20 transfers land in watch_logs, not here.
                }
                let tx_hash = match tx.get("hash").and_then(|v| v.as_str()) {
                    Some(h) => h.to_lowercase(),
                    None => continue,
                };
                transfers.push((tx_hash, to, value));
            }
        }
        Ok(BlockView { number, hash, parent_hash, transfers })
    }


    /// Used during reorg handling: "does this tx still exist, and where?"
    /// Returns Ok(None) if the node has never heard of it (dropped),
    /// Ok(Some((None, _))) if it's back in the mempool (mined_block = None).
    async fn locate_tx(&self, tx_hash: &str) -> Result<Option<(Option<u64>, Option<String>)>, String> {
        let raw = self.call_rpc_json(
            "eth_getTransactionByHash",
            serde_json::json!([tx_hash]),
        ).await?;

        if raw.is_null() {
            return Ok(None);
        }
        let bn = raw.get("blockNumber").and_then(|v| v.as_str()).map(hex_to_u64);
        let bh = raw.get("blockHash").and_then(|v| v.as_str()).map(|s| s.to_lowercase());
        Ok(Some((bn, bh)))
    }

    // ── Scan state ───────────────────────────────────────────────────────────

    async fn load_cursor(&self, pool: &PgPool) -> Result<Option<(i64, String)>, String> {
        sqlx::query_as::<_, (i64, String)>(
            r#"
            SELECT last_block, last_block_hash
              FROM network_scan_state
             WHERE network_type = $1 AND chain_ref = $2 AND scope = $3
            "#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(SCAN_SCOPE_ADDRESSES)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("load_cursor: {e}"))
    }

    async fn save_cursor(&self, pool: &PgPool, number: i64, hash: &str) -> Result<(), String> {
        sqlx::query(
            r#"
            INSERT INTO network_scan_state
                (network_type, chain_ref, scope, last_block, last_block_hash, updated_at)
            VALUES ($1, $2, $3, $4, $5, now())
            ON CONFLICT (network_type, chain_ref, scope) DO UPDATE
               SET last_block = EXCLUDED.last_block,
                   last_block_hash = EXCLUDED.last_block_hash,
                   updated_at = now()
            "#,
        )
            .bind(NETWORK_TYPE).bind(self.chain_ref()).bind(SCAN_SCOPE_ADDRESSES)
            .bind(number).bind(hash)
            .execute(pool).await
            .map_err(|e| format!("save_cursor: {e}"))?;
        Ok(())
    }

    async fn remember_block(&self, pool: &PgPool, b: &BlockView) -> Result<(), String> {
        sqlx::query(
            r#"
            INSERT INTO network_seen_blocks
                (network_type, chain_ref, scope, block_number, block_hash, parent_hash, seen_at)
            VALUES ($1, $2, $3, $4, $5, $6, now())
            ON CONFLICT (network_type, chain_ref, scope, block_number) DO UPDATE
               SET block_hash = EXCLUDED.block_hash,
                   parent_hash = EXCLUDED.parent_hash,
                   seen_at = now()
            "#,
        )
            .bind(NETWORK_TYPE).bind(self.chain_ref()).bind(SCAN_SCOPE_ADDRESSES)
            .bind(b.number as i64).bind(&b.hash).bind(&b.parent_hash)
            .execute(pool).await
            .map_err(|e| format!("remember_block: {e}"))?;
        Ok(())
    }

    async fn our_hash_at(&self, pool: &PgPool, number: i64) -> Result<Option<String>, String> {
        sqlx::query_scalar::<_, String>(
            r#"
            SELECT block_hash FROM network_seen_blocks
             WHERE network_type = $1 AND chain_ref = $2 AND scope = $3 AND block_number = $4
            "#,
        )
            .bind(NETWORK_TYPE).bind(self.chain_ref()).bind(SCAN_SCOPE_ADDRESSES).bind(number)
            .fetch_optional(pool).await
            .map_err(|e| format!("our_hash_at: {e}"))
    }

    async fn prune_seen_blocks(&self, pool: &PgPool, tip: i64) -> Result<(), String> {
        sqlx::query(
            r#"
            DELETE FROM network_seen_blocks
             WHERE network_type = $1 AND chain_ref = $2 AND scope = $3
               AND block_number < $4
            "#,
        )
            .bind(NETWORK_TYPE).bind(self.chain_ref()).bind(SCAN_SCOPE_ADDRESSES)
            .bind(tip - (MAX_REORG_DEPTH as i64 * 2))
            .execute(pool).await
            .map_err(|e| format!("prune_seen_blocks: {e}"))?;
        Ok(())
    }

    // ── Who are we watching ──────────────────────────────────────────────────

    /// Everything with an open interest on this chain:
    ///   - still-pending, unexpired invoices (we're waiting for money), OR
    ///   - invoices with at least one payment that hasn't reached
    ///     'system_confirmed' yet (money arrived, we're still counting).
    ///
    /// The second clause is the restart-safety bit: an invoice that already went
    /// 'paid' still needs its confirmation counter driven to FINAL_CONFIRMATIONS,
    /// and that must survive a process restart with an empty `self.pending`.
    ///
    /// token_address IS NULL => native currency (ETH/MATIC/...). ERC-20s are
    /// matched by Transfer logs in watch_logs, keyed on the exact token address.
    async fn load_watched_invoices(&self, pool: &PgPool) -> Result<Vec<WatchedInvoice>, String> {
        let rows = sqlx::query_as::<_, (Uuid, Uuid, String, rust_decimal::Decimal, i64, Option<i64>)>(
            r#"
            SELECT i.id,
                   i.merchant_id,
                   lower(i.wallet_address),
                   i.amount_requested,
                   COALESCE(i.required_confirmations, $3)::bigint,
                   i.created_block
              FROM invoices i
             WHERE i.network_type = $1
               AND i.chain_ref    = $2
               AND i.token_address IS NULL
               AND (
                     (i.status = 'pending' AND i.expires_at > now())
                  OR EXISTS (
                        SELECT 1 FROM payments p
                         WHERE p.invoice_id = i.id
                           AND p.status IN ('detected', 'merchant_confirmed')
                     )
               )
            "#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(FINAL_CONFIRMATIONS)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("load_watched_invoices: {e}"))?;

        Ok(rows.into_iter().map(|(invoice_id, merchant_id, address_lc, amount_requested, required_confirmations, created_block)| {
            WatchedInvoice { invoice_id, merchant_id, address_lc, amount_requested, required_confirmations, created_block }
        }).collect())
    }

    // ── The service loop ─────────────────────────────────────────────────────

    pub async fn watch_addresses(&self, pool: &PgPool) -> Result<(), String> {
        println!(
            "EVMNetwork::watch_addresses service started for {} ({})",
            self.network_name, self.chain_id
        );

        loop {
            if let Err(e) = self.tick_addresses(pool).await {
                // Transient by assumption: RPC hiccup, quorum split mid-reorg,
                // provider lagging the tip. The cursor is only advanced on
                // success, so the next tick just redoes the work.
                eprintln!(
                    "EVMNetwork::watch_addresses tick failed [{}]: {e}",
                    self.network_name
                );
            }
            tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;
        }
    }

    async fn tick_addresses(&self, pool: &PgPool) -> Result<(), String> {
        let watched = self.load_watched_invoices(pool).await?;
        let tip = self.get_block_number().await? as i64;

        // Address -> invoices. Same deposit address *can* legitimately show up
        // twice (address reuse across invoices for the same merchant), so this
        // is a multimap, and a matching tx credits every invoice on it.
        // TODO: once addresses are strictly single-use, collapse to a HashMap
        //       and hard-error on duplicates instead of silently double-crediting.
        let mut by_address: HashMap<String, Vec<WatchedInvoice>> = HashMap::new();
        for w in &watched {
            by_address.entry(w.address_lc.clone()).or_default().push(w.clone());
        }

        // 1. Where do we resume from?
        let mut cursor = match self.load_cursor(pool).await? {
            Some((n, h)) => Some((n, h)),
            None => {
                // Cold start. Begin at the oldest open invoice's creation block
                // so a restart after downtime still backfills, else at the tip.
                let start = sqlx::query_scalar::<_, Option<i64>>(
                    r#"
                    SELECT MIN(i.created_block)
                      FROM invoices i
                     WHERE i.network_type = $1 AND i.chain_ref = $2
                       AND i.token_address IS NULL
                       AND (
                             (i.status = 'pending' AND i.expires_at > now())
                          OR EXISTS (SELECT 1 FROM payments p
                                      WHERE p.invoice_id = i.id
                                        AND p.status IN ('detected','merchant_confirmed'))
                       )
                    "#,
                )
                    .bind(NETWORK_TYPE).bind(self.chain_ref())
                    .fetch_one(pool).await
                    .map_err(|e| format!("cold start floor: {e}"))?
                    .unwrap_or(tip);

                // -1 so the first block we actually process is `start`.
                let from = (start - 1).max(0);
                println!(
                    "[{}] no scan cursor, cold-starting at block {}",
                    self.network_name, from + 1
                );
                None.or(Some((from, String::new())))
            }
        };

        // 2. Reorg check + unwind, before we apply anything new.
        if let Some((last_block, last_hash)) = cursor.clone() {
            if !last_hash.is_empty() {
                if let Some(fork_point) = self.detect_fork_point(pool, last_block, &last_hash).await? {
                    if fork_point < last_block {
                        println!(
                            "[{}] reorg detected: cursor was {} , rewinding to {}",
                            self.network_name, last_block, fork_point
                        );
                        self.handle_reorg(pool, fork_point, &watched).await?;
                        let fork_hash = self.our_hash_at(pool, fork_point).await?.unwrap_or_default();
                        cursor = Some((fork_point, fork_hash));
                        self.save_cursor(pool, fork_point, &cursor.as_ref().unwrap().1).await?;
                    }
                }
            }
        }

        let (mut last_block, mut last_hash) = cursor.unwrap();

        // 3. Apply new blocks, throttled so a long outage doesn't melt the RPC.
        //    We re-read tip each tick so catch-up happens over multiple ticks.
        let target = std::cmp::min(tip, last_block + MAX_BLOCKS_PER_TICK as i64);

        let mut n = last_block + 1;
        while n <= target {
            let block = match self.get_block(n as u64, true).await? {
                Some(b) => b,
                None => break, // provider hasn't got it yet; try again next tick
            };

            // Linkage check. If the parent doesn't match what we applied last,
            // a reorg happened *between* the check above and now — bail out and
            // let the next tick's detect_fork_point deal with it properly.
            if !last_hash.is_empty() && block.parent_hash != last_hash {
                println!(
                    "[{}] parent mismatch at block {} (expected parent {}, got {}), deferring to reorg handling",
                    self.network_name, n, last_hash, block.parent_hash
                );
                break;
            }

            self.apply_block(pool, &block, &by_address).await?;
            self.remember_block(pool, &block).await?;

            last_hash = block.hash.clone();
            last_block = n;
            self.save_cursor(pool, last_block, &last_hash).await?;
            n += 1;
        }

        // 4. Recount confirmations against the *current* tip and promote.
        //    Doing this off the DB rather than off the just-scanned blocks means
        //    an invoice re-registered after a crash immediately picks up its real
        //    confirmation count instead of restarting from 0.
        self.refresh_confirmations(pool, tip, &watched).await?;

        self.prune_seen_blocks(pool, last_block).await?;
        Ok(())
    }

    /// Walk back from our cursor comparing our remembered hashes with the
    /// canonical chain. Returns the highest block we still agree on, or None if
    /// nothing changed / we have no history to compare against.
    async fn detect_fork_point(
        &self,
        pool: &PgPool,
        last_block: i64,
        last_hash: &str,
    ) -> Result<Option<i64>, String> {
        let canonical = self.get_block(last_block as u64, false).await?;
        if let Some(b) = &canonical {
            if b.hash == last_hash {
                return Ok(None); // no reorg
            }
        }

        let floor = (last_block - MAX_REORG_DEPTH as i64).max(0);
        let mut probe = last_block - 1;
        while probe >= floor {
            let ours = match self.our_hash_at(pool, probe).await? {
                Some(h) => h,
                // We never saw this block (pruned, or cold-started above it).
                // Nothing older to compare — treat it as the fork point and
                // rescan forward from here.
                None => return Ok(Some(probe)),
            };
            match self.get_block(probe as u64, false).await? {
                Some(b) if b.hash == ours => return Ok(Some(probe)),
                _ => probe -= 1,
            }
        }

        // Deeper than we're willing to unwind. This is not a "retry" situation —
        // it means our assumptions about the chain are wrong (or the RPC set is
        // serving a different chain entirely).
        // TODO: raise an operational alert / freeze this chain's payouts instead
        //       of silently rewinding, and expose it on a health endpoint.
        eprintln!(
            "[{}] reorg deeper than MAX_REORG_DEPTH ({} blocks) — clamping to {}",
            self.network_name, MAX_REORG_DEPTH, floor
        );
        Ok(Some(floor))
    }

    /// Everything strictly above `fork_point` is no longer trustworthy.
    ///
    /// Rule (per spec): a webhook only goes out if the payment's block genuinely
    /// got orphaned and the tx is gone. If the tx merely got re-mined into a
    /// different block, we silently reset its state (confirmations back to 0,
    /// status back to 'detected') and let the normal flow re-confirm it.
    async fn handle_reorg(
        &self,
        pool: &PgPool,
        fork_point: i64,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        let ids: Vec<Uuid> = watched.iter().map(|w| w.invoice_id).collect();
        if ids.is_empty() {
            return Ok(());
        }

        let affected = sqlx::query_as::<_, (Uuid, Uuid, String, i64, String, String)>(
            r#"
            SELECT p.id, p.invoice_id, p.tx_hash, p.block_number, p.block_hash, p.status
              FROM payments p
             WHERE p.invoice_id = ANY($1)
               AND p.block_number > $2
               AND p.status <> 'orphaned'
            "#,
        )
            .bind(&ids)
            .bind(fork_point)
            .fetch_all(pool).await
            .map_err(|e| format!("handle_reorg select: {e}"))?;

        for (payment_id, invoice_id, tx_hash, old_block, old_hash, old_status) in affected {
            match self.locate_tx(&tx_hash).await? {
                // Still mined, and in a block we now consider canonical.
                Some((Some(new_block), Some(new_hash))) => {
                    if new_hash == old_hash && new_block == old_block as u64 {
                        // Same block survived the reorg (it was above the fork
                        // point but on the winning branch after all). Still reset
                        // the counter — refresh_confirmations will recompute it
                        // from the tip on this very tick, so nothing is lost.
                    }
                    sqlx::query(
                        r#"
                        UPDATE payments
                           SET block_number = $2,
                               block_hash   = $3,
                               confirmations = 0,
                               status = 'detected',
                               updated_at = now()
                         WHERE id = $1
                        "#,
                    )
                        .bind(payment_id).bind(new_block as i64).bind(&new_hash)
                        .execute(pool).await
                        .map_err(|e| format!("handle_reorg re-mine update: {e}"))?;

                    println!(
                        "[{}] payment {} re-mined {}@{} -> {}@{}, confirmations reset (no webhook)",
                        self.network_name, payment_id, old_block, old_hash, new_block, new_hash
                    );
                    // No webhook: the money never went away, it just moved
                    // blocks. Merchant-visible state (amount_received) is
                    // unchanged, only the confirmation countdown restarts.
                    // If it had already been reported as merchant_confirmed we
                    // will re-emit payment.confirmed once it re-crosses the
                    // threshold — see refresh_confirmations.
                }

                // Known to the node but back in the mempool, or dropped entirely.
                // Mempool, missing hash/number, or completely dropped
                Some((Some(_), None)) | Some((None, _)) | None => {
                    sqlx::query(
                        r#"
                        UPDATE payments
                           SET status = 'orphaned', confirmations = 0, updated_at = now()
                         WHERE id = $1
                        "#,
                    )
                        .bind(payment_id)
                        .execute(pool).await
                        .map_err(|e| format!("handle_reorg orphan update: {e}"))?;

                    println!(
                        "[{}] payment {} orphaned (tx {} no longer mined, was {}@{}, prev status {})",
                        self.network_name, payment_id, tx_hash, old_block, old_hash, old_status
                    );

                    // ── WEBHOOK ───────────────────────────────────────────────
                    // Register 'payment.orphaned' for invoice `invoice_id`
                    // (payment_id, tx_hash, old block/hash, previous status).
                    // This is the *only* reorg case that notifies the merchant:
                    // the funds they were told about are gone, so anything they
                    // shipped on the back of payment.detected/confirmed needs to
                    // be walked back on their side.
                    //   INSERT INTO webhooks (merchant_id, invoice_id, event, payload, ...)
                    //   VALUES (..., 'payment.orphaned', ...);
                    // TODO: make delivery of this event opt-out-able per merchant
                    //       (some only care about final state) once merchant
                    //       webhook settings exist.
                    // ──────────────────────────────────────────────────────────
                }
            }

            // amount_received / invoice status must be rebuilt from the
            // surviving payments, not decremented, so it stays correct no matter
            // how many times a reorg replays.
            self.recompute_invoice_totals(pool, invoice_id, watched).await?;
        }

        Ok(())
    }

    /// Credit every native transfer in this block that lands on a watched address.
    async fn apply_block(
        &self,
        pool: &PgPool,
        block: &BlockView,
        by_address: &HashMap<String, Vec<WatchedInvoice>>,
    ) -> Result<(), String> {
        for (tx_hash, to_lc, value) in &block.transfers {
            let Some(invoices) = by_address.get(to_lc) else { continue };

            // A simple top-level value transfer that got included cannot have
            // reverted, so we skip the receipt fetch here.
            // TODO: native value can also arrive via a contract's internal call
            //       (SELFDESTRUCT, a router forwarding ETH, a multisend). Those
            //       are invisible to eth_getBlockByNumber and need
            //       trace_block / debug_traceBlockByNumber. Add that behind a
            //       per-chain "supports_traces" flag.
            let amount = wei_to_decimal(*value)?;

            for inv in invoices {
                if let Some(created) = inv.created_block {
                    if (block.number as i64) < created {
                        continue; // predates the invoice, not our money
                    }
                }

                // Idempotent insert on (invoice_id, tx_hash). This is what makes
                // a crash mid-block, a duplicate registration, or a rescan of an
                // already-scanned range harmless. `rows_affected == 1` means we
                // saw it for the first time, which is exactly the webhook edge.
                let inserted = sqlx::query(
                    r#"
                    INSERT INTO payments
                        (invoice_id, tx_hash, amount, block_number, block_hash, confirmations, status)
                    VALUES ($1, $2, $3, $4, $5, 0, 'detected')
                    ON CONFLICT (invoice_id, tx_hash) DO NOTHING
                    "#,
                )
                    .bind(inv.invoice_id)
                    .bind(tx_hash)
                    .bind(amount)
                    .bind(block.number as i64)
                    .bind(&block.hash)
                    .execute(pool).await
                    .map_err(|e| format!("insert payment: {e}"))?
                    .rows_affected() == 1;

                if inserted {
                    println!(
                        "[{}] detected {} wei -> {} (invoice {}, tx {}, block {})",
                        self.network_name, value, to_lc, inv.invoice_id, tx_hash, block.number
                    );

                    // ── WEBHOOK ───────────────────────────────────────────────
                    // First time we've ever seen this tx for this invoice.
                    // Register 'payment.detected' for merchant `inv.merchant_id`
                    // / invoice `inv.invoice_id` with tx_hash, base-unit amount,
                    // block_number, block_hash, confirmations = 0.
                    //   INSERT INTO webhooks (merchant_id, invoice_id, event, payload, ...)
                    //   VALUES (inv.merchant_id, inv.invoice_id, 'payment.detected', ...);
                    // The unique index above is what guarantees exactly-once
                    // here, so the insert of the webhook row belongs in the SAME
                    // transaction as the payment insert.
                    // TODO: wrap payment-insert + webhook-insert in one tx.
                    // ──────────────────────────────────────────────────────────
                } else {
                    // Already known. Could be a rescan, or the same tx re-mined
                    // into a different block after a reorg — keep the location
                    // fresh either way, but never touch the amount.
                    sqlx::query(
                        r#"
                        UPDATE payments
                           SET block_number = $2, block_hash = $3,
                               status = CASE WHEN status = 'orphaned' THEN 'detected' ELSE status END,
                               updated_at = now()
                         WHERE invoice_id = $1 AND tx_hash = $4
                           AND (block_hash <> $3 OR status = 'orphaned')
                        "#,
                    )
                        .bind(inv.invoice_id)
                        .bind(block.number as i64)
                        .bind(&block.hash)
                        .bind(tx_hash)
                        .execute(pool).await
                        .map_err(|e| format!("relocate payment: {e}"))?;
                }

                self.recompute_invoice_totals(pool, inv.invoice_id, std::slice::from_ref(inv)).await?;
            }
        }
        Ok(())
    }

    /// Recount confirmations from the tip for everything still in flight, then
    /// promote across the two thresholds.
    ///
    /// confirmations = tip - block_number + 1, i.e. the including block itself
    /// counts as 1. A payment in the tip block has 1 confirmation.
    async fn refresh_confirmations(
        &self,
        pool: &PgPool,
        tip: i64,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        if watched.is_empty() {
            return Ok(());
        }
        let ids: Vec<Uuid> = watched.iter().map(|w| w.invoice_id).collect();
        let thresholds: HashMap<Uuid, (Uuid, i64)> = watched.iter()
            .map(|w| (w.invoice_id, (w.merchant_id, w.required_confirmations)))
            .collect();

        sqlx::query(
            r#"
            UPDATE payments
               SET confirmations = GREATEST(0, $2 - block_number + 1),
                   updated_at = now()
             WHERE invoice_id = ANY($1)
               AND status IN ('detected', 'merchant_confirmed')
               AND confirmations <> GREATEST(0, $2 - block_number + 1)
            "#,
        )
            .bind(&ids).bind(tip)
            .execute(pool).await
            .map_err(|e| format!("refresh_confirmations: {e}"))?;

        let rows = sqlx::query_as::<_, (Uuid, Uuid, String, i64, String, i64, String)>(
            r#"
            SELECT p.id, p.invoice_id, p.tx_hash, p.block_number, p.block_hash,
                   p.confirmations::bigint, p.status
              FROM payments p
             WHERE p.invoice_id = ANY($1)
               AND p.status IN ('detected', 'merchant_confirmed')
             ORDER BY p.block_number ASC
            "#,
        )
            .bind(&ids)
            .fetch_all(pool).await
            .map_err(|e| format!("refresh_confirmations select: {e}"))?;

        for (payment_id, invoice_id, tx_hash, block_number, _block_hash, confirmations, status) in rows {
            let Some((merchant_id, required)) = thresholds.get(&invoice_id).copied() else { continue };

            // ── merchant threshold ───────────────────────────────────────────
            if status == "detected" && confirmations >= required {
                let promoted = sqlx::query(
                    r#"
                    UPDATE payments
                       SET status = 'merchant_confirmed', updated_at = now()
                     WHERE id = $1 AND status = 'detected'
                    "#,
                )
                    .bind(payment_id)
                    .execute(pool).await
                    .map_err(|e| format!("promote merchant_confirmed: {e}"))?
                    .rows_affected() == 1;

                if promoted {
                    println!(
                        "[{}] payment {} reached {}/{} confirmations -> merchant_confirmed",
                        self.network_name, payment_id, confirmations, required
                    );

                    // ── WEBHOOK ───────────────────────────────────────────────
                    // Register 'payment.confirmed' for merchant `merchant_id` /
                    // invoice `invoice_id`: payment_id, tx_hash, block_number,
                    // confirmations, required_confirmations.
                    // The guarded UPDATE above (status = 'detected' in the WHERE)
                    // is the once-only latch, so two workers racing can't both
                    // emit this.
                    //   INSERT INTO webhooks (..., 'payment.confirmed', ...);
                    // TODO: `required` currently comes from
                    //       invoices.required_confirmations with a fallback to
                    //       FINAL_CONFIRMATIONS. Make it resolve per-merchant /
                    //       per-chain / per-amount (big payments wait longer)
                    //       from a merchant_confirmation_policy table.
                    // ──────────────────────────────────────────────────────────
                }
            }

            // ── final / system threshold ─────────────────────────────────────
            // TODO: FINAL_CONFIRMATIONS is a global constant today; it should be
            //       per-chain (48 blocks on Ethereum ≈ 10 min, on Polygon it's
            //       ~2 min and you probably want a lot more).
            if confirmations >= FINAL_CONFIRMATIONS {
                let finalized = sqlx::query(
                    r#"
                    UPDATE payments
                       SET status = 'system_confirmed', updated_at = now()
                     WHERE id = $1 AND status <> 'system_confirmed'
                    "#,
                )
                    .bind(payment_id)
                    .execute(pool).await
                    .map_err(|e| format!("promote system_confirmed: {e}"))?
                    .rows_affected() == 1;

                if finalized {
                    println!(
                        "[{}] payment {} reached {} confirmations -> system_confirmed (block {}), no longer polled",
                        self.network_name, payment_id, confirmations, block_number
                    );

                    // ── WEBHOOK (optional) ────────────────────────────────────
                    // Register 'payment.finalized' — we now consider this
                    // irreversible and will never emit payment.orphaned for it.
                    // Mostly interesting to merchants who hold shipment until
                    // settlement is final.
                    //   INSERT INTO webhooks (..., 'payment.finalized', ...);
                    // TODO: gate on merchant settings; default off since most
                    //       merchants act on payment.confirmed.
                    // ──────────────────────────────────────────────────────────
                }
            }

            self.recompute_invoice_totals(pool, invoice_id, watched).await?;
        }

        // Anything fully settled drops out of the in-memory hint map so we stop
        // doing work for it. The DB query at the top of the tick already
        // excludes it, this just keeps `pending` from growing forever.
        let done = sqlx::query_scalar::<_, Uuid>(
            r#"
            SELECT i.id FROM invoices i
             WHERE i.id = ANY($1)
               AND i.status <> 'pending'
               AND NOT EXISTS (
                   SELECT 1 FROM payments p
                    WHERE p.invoice_id = i.id
                      AND p.status IN ('detected', 'merchant_confirmed')
               )
            "#,
        )
            .bind(&ids)
            .fetch_all(pool).await
            .map_err(|e| format!("settled invoices: {e}"))?;

        if !done.is_empty() {
            if let Ok(mut pending) = self.pending.lock() {
                for id in done {
                    pending.remove(&id);
                }
            }
        }

        Ok(())
    }

    /// Rebuild invoices.amount_received / status from the non-orphaned payments.
    /// Always a full recompute (never a delta) so reorgs, rescans and duplicate
    /// ticks all converge on the same number.
    async fn recompute_invoice_totals(
        &self,
        pool: &PgPool,
        invoice_id: Uuid,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        let Some(inv) = watched.iter().find(|w| w.invoice_id == invoice_id) else { return Ok(()) };

        // All amounts are base units (wei / smallest token unit), never human
        // readable — decimals are only applied at the presentation layer.
        let received = sqlx::query_scalar::<_, Decimal>(
            r#"
            SELECT COALESCE(SUM(amount), 0)
              FROM payments
             WHERE invoice_id = $1 AND status <> 'orphaned'
            "#,
        )
            .bind(invoice_id)
            .fetch_one(pool).await
            .map_err(|e| format!("sum payments: {e}"))?;

        let new_status = if received >= inv.amount_requested {
            if received > inv.amount_requested { "overpaid" } else { "paid" }
        } else if received > Decimal::ZERO {
            "underpaid"
        } else {
            "pending"
        };

        // Guarded update: only write (and therefore only fire) on a real
        // transition, and never resurrect an 'expired' invoice's status.
        let old_status = sqlx::query_scalar::<_, String>(
            r#"
            UPDATE invoices
               SET amount_received = $2,
                   status = CASE WHEN status = 'expired' THEN status ELSE $3 END,
                   updated_at = now()
             WHERE id = $1
               AND (amount_received <> $2 OR (status <> $3 AND status <> 'expired'))
            RETURNING (SELECT status FROM invoices WHERE id = $1)
            "#,
        )
            .bind(invoice_id)
            .bind(received)
            .bind(new_status)
            .fetch_optional(pool).await
            .map_err(|e| format!("update invoice totals: {e}"))?;

        if let Some(prev) = old_status {
            let was_settled = prev == "paid" || prev == "overpaid";
            let is_settled = new_status == "paid" || new_status == "overpaid";

            if is_settled && !was_settled {
                println!(
                    "[{}] invoice {} settled: received {} / requested {} ({})",
                    self.network_name, invoice_id, received, inv.amount_requested, new_status
                );

                // ── WEBHOOK ───────────────────────────────────────────────────
                // amount_received >= amount_requested, i.e. the invoice is fully
                // funded. THIS is the 'payment.finished' event — register it for
                // merchant `inv.merchant_id` / invoice `invoice_id` with
                // amount_received, amount_requested and the overpaid flag.
                //   INSERT INTO webhooks (..., 'payment.finished', ...);
                //
                // Fired on the *amount* threshold, independent of confirmations
                // — payment.detected/confirmed/finalized carry the confirmation
                // story. The status guard in the UPDATE above makes it once-only.
                // TODO: make the trigger policy a merchant setting:
                //       'on_detected' (fire now, current behaviour),
                //       'on_confirmed' (require every contributing payment to be
                //       merchant_confirmed first), or 'on_finalized'.
                // TODO: also decide the underpaid tolerance here (dust /
                //       rounding), currently strict >=.
                // ──────────────────────────────────────────────────────────────
            } else if !is_settled && was_settled {
                // Reorg clawed us back below the requested amount. The
                // payment.orphaned event already told the merchant why, so we
                // don't emit a second "unfinished" event here.
                // TODO: if merchants ask for it, add 'payment.reverted'.
                println!(
                    "[{}] invoice {} fell back to {} after reorg (received {})",
                    self.network_name, invoice_id, new_status, received
                );
            }
        }

        Ok(())
    }

    async fn call_rpc_single(&self, url: &str, method: &'static str, params: serde_json::Value) -> Result<String, String> {
        let payload = RpcRequest { jsonrpc: "2.0", method, params, id: 1 };

        let response = self.client.post(url).json(&payload).send().await
            .map_err(|e| format!("HTTP request to {url} failed: {e}"))?;

        let rpc_res: RpcResponse = response.json().await
            .map_err(|e| format!("Failed to parse JSON response from {url}: {e}"))?;

        if let Some(err) = rpc_res.error {
            return Err(format!("RPC Error from {url}: {}", err.message));
        }

        rpc_res.result.ok_or_else(|| format!("No result in RPC response from {url}"))
    }

    /// Fans out to every configured endpoint for this chain and only trusts a
    /// result once at least 2 of them agree. With a single-URL config (local
    /// dev, testnets where you only have one provider) it skips straight to
    /// that node — quorum only kicks in when you've actually configured >1 URL.
    async fn call_rpc(&self, method: &'static str, params: serde_json::Value) -> Result<String, String> {
        if self.rpc_urls.len() == 1 {
            return self.call_rpc_single(&self.rpc_urls[0], method, params).await;
        }

        let futures = self.rpc_urls.iter()
            .map(|url| self.call_rpc_single(url, method, params.clone()));
        let results: Vec<Result<String, String>> = futures::future::join_all(futures).await;

        let oks: Vec<&String> = results.iter().filter_map(|r| r.as_ref().ok()).collect();

        if oks.len() < 2 {
            let errs: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
            return Err(format!(
                "Quorum failed for {method} on chain {}: only {}/{} endpoints responded. Errors: {:?}",
                self.chain_id, oks.len(), self.rpc_urls.len(), errs
            ));
        }

        // Return the first value that at least 2 endpoints agree on.
        for candidate in &oks {
            if oks.iter().filter(|v| *v == candidate).count() >= 2 {
                return Ok((*candidate).clone());
            }
        }

        // All responded but none matched — e.g. a 3-way split during a reorg.
        // This isn't something a "tiebreaker" can resolve (there's no majority
        // to break a tie toward), so treat it as transient and let the caller retry.
        Err(format!(
            "Quorum disagreement for {method} on chain {}: endpoints returned different values: {:?}",
            self.chain_id, oks
        ))
    }

    /// Same idea as `call_rpc`, but for methods whose `result` is a JSON
    /// object/array (e.g. `eth_getBlockByNumber`) rather than a plain
    /// string, which is all the existing `call_rpc`/`call_rpc_single`
    /// support. Quorum comparison here is structural (serde_json::Value's
    /// PartialEq), so key-ordering differences between providers' JSON
    /// don't cause false disagreements.
    async fn call_rpc_single_json(
        &self,
        url: &str,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let payload = RpcRequest { jsonrpc: "2.0", method, params, id: 1 };

        let response = self.client.post(url).json(&payload).send().await
            .map_err(|e| format!("HTTP request to {url} failed: {e}"))?;

        let rpc_res: serde_json::Value = response.json().await
            .map_err(|e| format!("Failed to parse JSON response from {url}: {e}"))?;

        if let Some(err) = rpc_res.get("error") {
            return Err(format!("RPC Error from {url}: {err}"));
        }

        rpc_res.get("result").cloned()
            .ok_or_else(|| format!("No result in RPC response from {url}"))
    }

    async fn call_rpc_json(&self, method: &'static str, params: serde_json::Value) -> Result<serde_json::Value, String> {
        if self.rpc_urls.len() == 1 {
            return self.call_rpc_single_json(&self.rpc_urls[0], method, params).await;
        }

        let futures = self.rpc_urls.iter()
            .map(|url| self.call_rpc_single_json(url, method, params.clone()));
        let results: Vec<Result<serde_json::Value, String>> = futures::future::join_all(futures).await;

        let oks: Vec<&serde_json::Value> = results.iter().filter_map(|r| r.as_ref().ok()).collect();

        if oks.len() < 2 {
            let errs: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
            return Err(format!(
                "Quorum failed for {method} on chain {}: only {}/{} endpoints responded. Errors: {:?}",
                self.chain_id, oks.len(), self.rpc_urls.len(), errs
            ));
        }

        for candidate in &oks {
            if oks.iter().filter(|v| *v == candidate).count() >= 2 {
                return Ok((*candidate).clone());
            }
        }

        Err(format!(
            "Quorum disagreement for {method} on chain {}: endpoints returned different values",
            self.chain_id
        ))
    }



    async fn call_rpc_single_logs(&self, url: &str, params: serde_json::Value) -> Result<Vec<Log>, String> {
        let payload = RpcRequest { jsonrpc: "2.0", method: "eth_getLogs", params, id: 1 };

        let response = self.client.post(url).json(&payload).send().await
            .map_err(|e| format!("HTTP request to {url} failed: {e}"))?;

        let rpc_res: RpcResponseLogs = response.json().await
            .map_err(|e| format!("Failed to parse JSON response from {url}: {e}"))?;

        if let Some(err) = rpc_res.error {
            return Err(format!("RPC Error from {url}: {}", err.message));
        }

        rpc_res.result.ok_or_else(|| format!("No result in RPC response from {url}"))
    }

    /// Same quorum logic as call_rpc: single-URL configs skip straight to that
    /// node; multi-URL configs fan out to all of them and only trust a log set
    /// once at least 2 endpoints return the same (order-normalized) set.
    ///
    /// `filter` is a raw eth_getLogs filter object, e.g.:
    ///   serde_json::json!({
    ///       "address": vault_address,
    ///       "topics": [payment_topic0],
    ///       "fromBlock": "0x...",
    ///       "toBlock": "latest"
    ///   })
    pub async fn get_logs(&self, filter: serde_json::Value) -> Result<Vec<Log>, String> {
        let params = serde_json::Value::Array(vec![filter]);

        if self.rpc_urls.len() == 1 {
            return self.call_rpc_single_logs(&self.rpc_urls[0], params).await;
        }

        let futures = self.rpc_urls.iter()
            .map(|url| self.call_rpc_single_logs(url, params.clone()));
        let results: Vec<Result<Vec<Log>, String>> = futures::future::join_all(futures).await;

        let oks: Vec<Vec<Log>> = results.iter()
            .filter_map(|r| r.as_ref().ok())
            .map(|logs| {
                let mut sorted = logs.clone();
                sorted.sort_by_key(|l| (hex_to_u64(&l.block_number), hex_to_u64(&l.log_index)));
                sorted
            })
            .collect();

        if oks.len() < 2 {
            let errs: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
            return Err(format!(
                "Quorum failed for eth_getLogs on chain {}: only {}/{} endpoints responded. Errors: {:?}",
                self.chain_id, oks.len(), self.rpc_urls.len(), errs
            ));
        }

        for candidate in &oks {
            if oks.iter().filter(|v| *v == candidate).count() >= 2 {
                return Ok(candidate.clone());
            }
        }

        Err(format!(
            "Quorum disagreement for eth_getLogs on chain {}: endpoints returned different log sets ({} responses, no 2 matched)",
            self.chain_id, oks.len()
        ))
    }


    /// Internal parser to get raw integer units directly from hexadecimal outputs
    fn parse_hex_balance(hex_str: &str) -> Result<Amount, String> {
        let clean_hex = hex_str.trim_start_matches("0x");
        if clean_hex.is_empty() {
            return Ok(Amount(0));
        }

        let raw_units = u128::from_str_radix(clean_hex, 16)
            .map_err(|_| "Failed to parse hex balance".to_string())?;

        Ok(Amount(raw_units))
    }
    pub fn derive_address(&self, mnemonic: &str, index: u32) -> Result<String, String> {
        let mnemonic_parsed = Mnemonic::parse(mnemonic)
            .map_err(|e| format!("Invalid mnemonic: {}", e))?;

        let seed = mnemonic_parsed.to_seed("");

        let path_str = self.get_derivation_path(index);
        let path: DerivationPath = path_str
            .parse()
            .map_err(|e| format!("Failed to parse derivation path: {}", e))?;

        let child_xprv = XPrv::derive_from_path(&seed, &path)
            .map_err(|e| format!("Failed to derive child key at path: {}", e))?;

        let secret_key = child_xprv.private_key();
        let public_key = secret_key.public_key();

        let public_key_point = public_key.to_encoded_point(false);
        let point_bytes = public_key_point.as_bytes();

        let mut hasher = Keccak256::new();
        hasher.update(&point_bytes[1..]);
        let hash_result = hasher.finalize();

        let address_bytes = &hash_result[12..];

        Ok(format!("0x{}", hex::encode(address_bytes)))
    }

    pub async fn watch_logs(&self, pool: &PgPool) -> Result<(), String> {
        println!(
            "EVMNetwork::watch_logs service started for {} ({})",
            self.network_name, self.chain_id
        );

        loop {
            // TODO: Add event/log-watching logic here
            println!("EVMNetwork::watch_logs tick [{}]", self.network_name);

            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    }
}

#[async_trait]
impl NetworkClient for EVMNetwork {
    async fn get_derive_address(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        mnemonic: &str,
    ) -> Result<(String, u32, Option<String>), String> {
        let row = sqlx::query!(
            r#"
            INSERT INTO merchant_network_indices (merchant_id, network, account_index, next_index)
            VALUES ($1, $2, 0, 1)
            ON CONFLICT (merchant_id, network, account_index)
            DO UPDATE SET
                next_index = merchant_network_indices.next_index + 1,
                updated_at = CURRENT_TIMESTAMP
            RETURNING next_index
            "#,
            merchant_id,
            self.network_name
        )
            .fetch_one(pool)
            .await
            .map_err(|e| format!("Failed to update merchant network index: {e}"))?;

        let index = row.next_index as u32;
        let address = self.derive_address(mnemonic, index)?;

        let reference = format!("0x{}", hex::encode(invoice_id.as_bytes()));

        Ok((address, index, Some(reference)))
    }

    fn get_derivation_path(&self, index: u32) -> String {
        format!("m/44'/60'/0'/0/{index}")
    }

    fn validate_address(&self, address: &str) -> bool {
        let clean_addr = address.trim_start_matches("0x");

        if clean_addr.len() != 40 {
            return false;
        }

        clean_addr.chars().all(|c| c.is_ascii_hexdigit())
    }

    // --- CHAIN STATE METHODS ---

    async fn get_native_balance(&self, address: &str) -> Result<Amount, String> {
        let hex_balance = self.call_rpc("eth_getBalance", json!([address, "latest"])).await?;
        Self::parse_hex_balance(&hex_balance)
    }

    async fn get_token_balance(&self, token_address: &str, address: &str, _decimals: u8) -> Result<Amount, String> {
        let clean_addr = address.trim_start_matches("0x");
        let data = format!("0x70a08231{:0>64}", clean_addr);
        let params = json!([{ "to": token_address, "data": data }, "latest"]);
        let hex_balance = self.call_rpc("eth_call", params).await?;
        Self::parse_hex_balance(&hex_balance)
    }

    async fn get_current_block(&self) -> Result<u64, String> {
        let hex_block = self.call_rpc("eth_blockNumber", json!([])).await?;
        let clean_hex = hex_block.trim_start_matches("0x");

        u64::from_str_radix(clean_hex, 16)
            .map_err(|_| "Failed to parse hex block number".to_string())
    }

    // --- BATCHED WATCHING METHODS ---

    fn register_payment(&self, watch: PaymentWatch) {
        if let Ok(mut pending) = self.pending.lock() {
            println!("EVMNetwork::register_payment for invoice: {}", watch.invoice_id);
            pending.insert(watch.invoice_id, watch);
        }
    }

    fn unregister_payment(&self, invoice_id: Uuid) {
        if let Ok(mut pending) = self.pending.lock() {
            println!("EVMNetwork::unregister_payment for invoice: {}", invoice_id);
            pending.remove(&invoice_id);
        }
    }

    async fn watch_payments(&self, pool: &PgPool) -> Result<(), String> {
        println!(
            "EVMNetwork::watch_payments spinning up sub-services for {} ({})",
            self.network_name, self.chain_id
        );

        // Run both services concurrently on the current instance
        let (addresses_res, logs_res) = tokio::join!(
            self.watch_addresses(pool),
            self.watch_logs(pool)
        );

        // Bubble up error if either service fails
        addresses_res?;
        logs_res?;

        Ok(())
    }
}