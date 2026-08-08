use super::{enqueue_webhook, Amount, NetworkClient, PaymentWatch, SolanaCluster};
use async_trait::async_trait;
use uuid::Uuid;

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;

use ed25519_dalek::SigningKey;
use hmac::{Hmac, KeyInit, Mac}; // Added KeyInit here
use sha2::{Sha256, Sha512, Digest};
use sqlx::PgPool;

use bip39::Mnemonic;
use curve25519_dalek::edwards::CompressedEdwardsY;
use rust_decimal::Decimal;



use futures::stream::{self, StreamExt};
use serde_json::{json, Map, Value};

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
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<RpcError>,
}

#[derive(Deserialize)]
struct RpcError {
    message: String,
}

#[derive(Deserialize)]
struct SolBalanceValue {
    value: u64, // Lamports
}

#[derive(Deserialize)]
struct SolTokenAccountsValue {
    value: Vec<serde_json::Value>,
}


type HmacSha512 = Hmac<Sha512>;
const SOLANA_HARDENED_OFFSET: u32 = 0x8000_0000;
const TOKEN_PROGRAM_ID: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
const ASSOCIATED_TOKEN_PROGRAM_ID: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
const PDA_MARKER: &[u8] = b"ProgramDerivedAddress";



// ---------- derivation path ----------

/// Phantom/Solflare-style hardened path: m/44'/501'/{index}'/0'
fn get_solana_derivation_path(index: u32) -> String {
    format!("m/44'/501'/{}'/0'", index)
}

/// SLIP-0010 only supports hardened derivation for ed25519, so every
/// segment must end in `'`. Returns the raw (unhardened) u32 for each segment.
fn parse_hardened_path(path: &str) -> Result<Vec<u32>, String> {
    path.trim_start_matches("m/")
        .split('/')
        .map(|segment| {
            if !segment.ends_with('\'') {
                return Err(format!(
                    "SLIP-0010 ed25519 requires hardened segments, got: {}",
                    segment
                ));
            }
            let raw = segment
                .trim_end_matches('\'')
                .parse::<u32>()
                .map_err(|_| format!("Invalid path segment: {}", segment))?;
            if raw >= SOLANA_HARDENED_OFFSET {
                return Err(format!("Path segment out of range: {}", segment));
            }
            Ok(raw)
        })
        .collect()
}

// ---------- SLIP-0010 ed25519 key derivation ----------

fn slip10_master_key(seed: &[u8]) -> ([u8; 32], [u8; 32]) {
    let mut mac = HmacSha512::new_from_slice(b"ed25519 seed")
        .expect("HMAC accepts a key of any size");
    mac.update(seed);
    let result = mac.finalize().into_bytes();

    let mut key = [0u8; 32];
    let mut chain_code = [0u8; 32];
    key.copy_from_slice(&result[0..32]);
    chain_code.copy_from_slice(&result[32..64]);
    (key, chain_code)
}

fn slip10_derive_child(key: &[u8; 32], chain_code: &[u8; 32], index: u32) -> ([u8; 32], [u8; 32]) {
    let hardened_index = index | SOLANA_HARDENED_OFFSET;
    let mut mac = HmacSha512::new_from_slice(chain_code)
        .expect("HMAC accepts a key of any size");
    mac.update(&[0u8]); // ed25519 SLIP-0010: 0x00 || private_key || ser32(index)
    mac.update(key);
    mac.update(&hardened_index.to_be_bytes());
    let result = mac.finalize().into_bytes();

    let mut child_key = [0u8; 32];
    let mut child_chain_code = [0u8; 32];
    child_key.copy_from_slice(&result[0..32]);
    child_chain_code.copy_from_slice(&result[32..64]);
    (child_key, child_chain_code)
}

/// Walks m/44'/501'/{index}'/0' via SLIP-0010 and returns the ed25519 signing key.
fn derive_solana_signing_key(mnemonic: &str, index: u32) -> Result<SigningKey, String> {
    let mnemonic_parsed = Mnemonic::parse(mnemonic).map_err(|e| format!("Invalid mnemonic: {}", e))?;
    let seed = mnemonic_parsed.to_seed("");

    let path_str = get_solana_derivation_path(index);
    let segments = parse_hardened_path(&path_str)?;

    let (mut key, mut chain_code) = slip10_master_key(&seed);
    for segment in segments {
        let (child_key, child_chain_code) = slip10_derive_child(&key, &chain_code, segment);
        key = child_key;
        chain_code = child_chain_code;
    }

    Ok(SigningKey::from_bytes(&key))
}

/// Derives the base58-encoded wallet (owner) address for a given index.
fn derive_solana_address(mnemonic: &str, index: u32) -> Result<String, String> {
    let signing_key = derive_solana_signing_key(mnemonic, index)?;
    let public_key_bytes = signing_key.verifying_key().to_bytes();
    Ok(bs58::encode(public_key_bytes).into_string())
}

// ---------- ATA / PDA derivation (no solana-* crates) ----------

fn decode_pubkey(address: &str) -> Result<[u8; 32], String> {
    let bytes = bs58::decode(address)
        .into_vec()
        .map_err(|e| format!("Invalid base58 address '{}': {}", address, e))?;
    bytes
        .try_into()
        .map_err(|_| format!("Address '{}' is not a valid 32-byte pubkey", address))
}

/// A candidate PDA is only valid if it does NOT lie on the ed25519 curve.
fn is_on_curve(bytes: &[u8; 32]) -> bool {
    CompressedEdwardsY(*bytes).decompress().is_some()
}

/// Standalone reimplementation of `Pubkey::find_program_address`.
fn find_program_address(seeds: &[&[u8]], program_id: &[u8; 32]) -> Result<([u8; 32], u8), String> {
    for bump in (0..=255u8).rev() {
        let mut hasher = Sha256::new();
        for seed in seeds {
            hasher.update(seed);
        }
        hasher.update(&[bump]);
        hasher.update(program_id);
        hasher.update(PDA_MARKER);
        let hash = hasher.finalize();

        let mut candidate = [0u8; 32];
        candidate.copy_from_slice(&hash);

        if !is_on_curve(&candidate) {
            return Ok((candidate, bump));
        }
    }
    Err("Unable to find a valid program derived address".to_string())
}

/// Derives the Associated Token Account address for `owner_address` + `mint_address`.
fn derive_associated_token_address(owner_address: &str, mint_address: &str) -> Result<String, String> {
    let owner_bytes = decode_pubkey(owner_address)?;
    let mint_bytes = decode_pubkey(mint_address)?;
    let token_program_bytes = decode_pubkey(TOKEN_PROGRAM_ID)?;
    let associated_token_program_bytes = decode_pubkey(ASSOCIATED_TOKEN_PROGRAM_ID)?;

    let seeds: [&[u8]; 3] = [&owner_bytes, &token_program_bytes, &mint_bytes];
    let (ata_bytes, _bump) = find_program_address(&seeds, &associated_token_program_bytes)?;

    Ok(bs58::encode(ata_bytes).into_string())
}

fn get_derivation_path(index: u32) -> String {
    format!("m/44'/501'/{}'/0'", index)
}
fn parse_derivation_path(path: &str) -> Result<Vec<u32>, String> {
    if !path.starts_with("m/") {
        return Err("Path must start with 'm/'".to_string());
    }
    let mut indices = Vec::new();
    for part in path["m/".len()..].split('/') {
        if part.is_empty() { continue; }
        let is_hardened = part.ends_with('\'');
        let num_str = if is_hardened {
            &part[..part.len() - 1]
        } else {
            part
        };
        let val: u32 = num_str.parse().map_err(|e| format!("Invalid path segment: {}", e))?;
        if is_hardened {
            indices.push(val | 0x8000_0000);
        } else {
            indices.push(val);
        }
    }
    Ok(indices)
}

// ─────────────────────────────────────────────────────────────────────────────
// Tunables. All of these become per-merchant / per-cluster config later.
// ─────────────────────────────────────────────────────────────────────────────

const NETWORK_TYPE: &str = "solana";

/// ~400ms slots, but RPC providers rate-limit harder than they block. 2s is a
/// good compromise: a `confirmed` transaction is visible within one tick.
const POLL_INTERVAL_SECS: u64 = 2;

/// `getSignaturesForAddress` hard cap is 1000.
const SIG_PAGE_LIMIT: usize = 1000;

/// Catch-up throttle per address per tick. A fresh invoice with an old
/// `created_slot` can't pin the loop for an unbounded number of pages.
const MAX_SIG_PAGES_PER_ADDRESS: usize = 5;

/// JSON-RPC batch size for `getTransaction`. Providers vary; 50 is safe.
const MAX_TX_PER_BATCH: usize = 50;

/// `getSignatureStatuses` hard cap is 256 signatures per call.
const MAX_STATUS_PER_BATCH: usize = 256;

/// How many addresses we poll concurrently.
const ADDRESS_CONCURRENCY: usize = 8;

/// Canonical confirmation numbers written back to `payments.confirmations`.
/// These exist so the rest of the stack (API, webhooks, merchant dashboard)
/// keeps speaking the same language as the EVM path. They are labels, not
/// counts — nothing on Solana derives meaning from the arithmetic.
const CONF_DETECTED: i64 = 1;
const CONF_CONFIRMED: i64 = 16;
const CONF_FINALIZED: i64 = 32;

/// Commitment used for detection. `confirmed` = supermajority voted, ~1 slot.
const DETECT_COMMITMENT: &str = "confirmed";

// ─────────────────────────────────────────────────────────────────────────────
// Confirmation levels
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum ConfirmLevel {
    Detected = 0,
    Confirmed = 1,
    Finalized = 2,
}

impl ConfirmLevel {
    /// Clamp a merchant's numeric threshold onto the three buckets Solana
    /// actually has. Anything <= 1 is "I ship on sight", 32+ is "I wait for
    /// irreversibility", everything in between is the middle bucket.
    fn from_required(required: i64) -> Self {
        if required >= CONF_FINALIZED {
            Self::Finalized
        } else if required >= 2 {
            Self::Confirmed
        } else {
            Self::Detected
        }
    }

    fn from_rpc(s: &str) -> Option<Self> {
        match s {
            "processed" => Some(Self::Detected),
            "confirmed" => Some(Self::Confirmed),
            "finalized" => Some(Self::Finalized),
            _ => None,
        }
    }

    fn as_confirmations(self) -> i64 {
        match self {
            Self::Detected => CONF_DETECTED,
            Self::Confirmed => CONF_CONFIRMED,
            Self::Finalized => CONF_FINALIZED,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Detected => "detected",
            Self::Confirmed => "confirmed",
            Self::Finalized => "finalized",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Watched state
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct WatchedInvoice {
    invoice_id: Uuid,
    merchant_id: Uuid,
    /// Direct path target. Native: HD-derived pubkey. SPL: its ATA for `mint`.
    deposit_address: String,
    /// Reference path key: the HD-derived pubkey, attached read-only to the
    /// transfer. Unique per invoice, which is what makes attribution possible
    /// when one merchant has many invoices open at once.
    payment_reference: String,
    /// Reference path credit target: merchant main wallet (native) or the
    /// merchant's ATA for `mint` (SPL).
    merchant_target: String,
    /// Base units. Lamports for native, token atoms for SPL. Never human-readable.
    amount_requested: Decimal,
    /// None => native SOL.
    mint: Option<String>,
    level: ConfirmLevel,
    created_slot: Option<i64>,
}

impl WatchedInvoice {
    /// Every address whose signature feed can carry money for this invoice.
    /// The merchant target is deliberately NOT in here — it's shared across
    /// every invoice for that merchant and gets hit by unrelated traffic. We
    /// reach it through the reference instead, which is 1:1 with the invoice.
    fn watch_addresses(&self) -> [&str; 2] {
        [&self.deposit_address, &self.payment_reference]
    }
}

/// What a fetched transaction did, reduced to just the balance movements.
///
/// Deltas rather than parsed instructions on purpose: a payer can move SPL
/// tokens with `transfer`, `transferChecked`, a CPI from some aggregator, or
/// several instructions in one transaction. Pre/post balances are the ground
/// truth for all of them and can't be spoofed by instruction shape.
struct TxView {
    signature: String,
    slot: i64,
    failed: bool,
    account_keys: HashSet<String>,
    /// address -> lamport delta
    native_delta: HashMap<String, i128>,
    /// (token account address, mint) -> atom delta
    token_delta: HashMap<(String, String), i128>,
}

impl TxView {
    fn native_credit(&self, address: &str) -> i128 {
        self.native_delta.get(address).copied().unwrap_or(0)
    }

    fn token_credit(&self, address: &str, mint: &str) -> i128 {
        self.token_delta
            .get(&(address.to_string(), mint.to_string()))
            .copied()
            .unwrap_or(0)
    }

    /// Credit landing on `address` for this invoice's asset.
    fn credit(&self, address: &str, mint: Option<&str>) -> i128 {
        match mint {
            None => self.native_credit(address),
            Some(m) => self.token_credit(address, m),
        }
    }
}

/// A signature we found on an address feed, before we've fetched the body.
#[derive(Clone)]
struct SigRef {
    signature: String,
    slot: i64,
    /// `err` from the signature listing — lets us drop failures without
    /// spending a `getTransaction` on them.
    failed: bool,
}

// ═══════════════════════════════════════════════════════════════════════════
// ### NETWORK IMPLEMENTATION ###
// ═══════════════════════════════════════════════════════════════════════════

pub struct SolanaNetwork {
    cluster: SolanaCluster,
    rpc_urls: Vec<String>,
    pub network_name: String,
    client: reqwest::Client,
    pending: Mutex<HashMap<Uuid, PaymentWatch>>,
}

impl SolanaNetwork {
    pub fn new(cluster: SolanaCluster, rpc_urls: Vec<String>) -> Self {
        assert!(!rpc_urls.is_empty(), "SolanaNetwork requires at least one RPC URL");
        let network_name = format!("SOL_{:?}", cluster);

        Self {
            cluster,
            rpc_urls,
            network_name,
            client: reqwest::Client::new(),
            pending: Mutex::new(HashMap::new()),
        }
    }

    fn chain_ref(&self) -> String {
        format!("{:?}", self.cluster).to_lowercase()
    }

    // ── RPC ──────────────────────────────────────────────────────────────────
    //
    // Note on quorum: the EVM code cross-checks providers because a bad block
    // read silently credits or drops money. On Solana the equivalent guarantee
    // comes from commitment, not from polling several nodes — two honest nodes
    // legitimately disagree about the last ~32 slots, so quorum on
    // `getSignaturesForAddress` would fail constantly at the tip while adding
    // nothing at `finalized`. So: failover for reads, and let finality be the
    // arbiter. `reconcile_statuses` is what actually decides whether money is
    // real, and it only ever promotes to `system_confirmed` on `finalized`.

    async fn rpc(&self, method: &'static str, params: Value) -> Result<Value, String> {
        let mut last_err = String::new();
        for url in &self.rpc_urls {
            match self.call_rpc_single_json(url, method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => last_err = e,
            }
        }
        Err(format!("[{}] all endpoints failed for {method}: {last_err}", self.network_name))
    }

    /// JSON-RPC 2.0 batch. Results come back keyed by `id`, in arbitrary order,
    /// so we reindex before returning. One HTTP round trip for up to
    /// `MAX_TX_PER_BATCH` transactions instead of N.
    async fn rpc_batch(
        &self,
        calls: &[(&'static str, Value)],
    ) -> Result<Vec<Result<Value, String>>, String> {
        if calls.is_empty() {
            return Ok(Vec::new());
        }

        let payload: Vec<Value> = calls
            .iter()
            .enumerate()
            .map(|(i, (method, params))| {
                json!({ "jsonrpc": "2.0", "id": i, "method": method, "params": params })
            })
            .collect();

        let mut last_err = String::new();
        for url in &self.rpc_urls {
            let resp = match self.client.post(url).json(&payload).send().await {
                Ok(r) => r,
                Err(e) => {
                    last_err = format!("HTTP request to {url} failed: {e}");
                    continue;
                }
            };

            let body: Value = match resp.json().await {
                Ok(v) => v,
                Err(e) => {
                    last_err = format!("Failed to parse batch response from {url}: {e}");
                    continue;
                }
            };

            let Some(entries) = body.as_array() else {
                // Some providers return a single error object instead of an
                // array when the whole batch is rejected (size limits, auth).
                last_err = format!("Non-array batch response from {url}: {body}");
                continue;
            };

            let mut out: Vec<Result<Value, String>> =
                (0..calls.len()).map(|i| Err(format!("no response for batch id {i}"))).collect();

            for entry in entries {
                let Some(id) = entry.get("id").and_then(|v| v.as_u64()) else { continue };
                let idx = id as usize;
                if idx >= out.len() {
                    continue;
                }
                out[idx] = if let Some(err) = entry.get("error") {
                    Err(format!("RPC error from {url}: {err}"))
                } else {
                    Ok(entry.get("result").cloned().unwrap_or(Value::Null))
                };
            }

            return Ok(out);
        }

        Err(format!("[{}] all endpoints failed for batch: {last_err}", self.network_name))
    }

    async fn call_rpc_single_json(
        &self,
        url: &str,
        method: &'static str,
        params: Value,
    ) -> Result<Value, String> {
        let payload = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });

        let response = self
            .client
            .post(url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("HTTP request to {url} failed: {e}"))?;

        let rpc_res: Value = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse JSON response from {url}: {e}"))?;

        if let Some(err) = rpc_res.get("error") {
            return Err(format!("RPC Error from {url}: {err}"));
        }

        rpc_res
            .get("result")
            .cloned()
            .ok_or_else(|| format!("No result in RPC response from {url}"))
    }

    async fn get_slot(&self, commitment: &str) -> Result<i64, String> {
        let v = self.rpc("getSlot", json!([{ "commitment": commitment }])).await?;
        v.as_i64().ok_or_else(|| format!("getSlot returned non-integer: {v}"))
    }

    // ── The service loop ─────────────────────────────────────────────────────

    pub async fn watch_addresses(&self, pool: &PgPool) -> Result<(), String> {
        println!(
            "SolanaNetwork::watch_addresses service started for {}",
            self.network_name
        );

        loop {
            if let Err(e) = self.tick(pool).await {
                // Transient by assumption: RPC hiccup, provider lagging, rate
                // limit. Cursors only advance on success, so the next tick
                // simply redoes the work.
                eprintln!(
                    "SolanaNetwork::watch_addresses tick failed [{}]: {e}",
                    self.network_name
                );
            }
            tokio::time::sleep(std::time::Duration::from_secs(POLL_INTERVAL_SECS)).await;
        }
    }

    async fn tick(&self, pool: &PgPool) -> Result<(), String> {
        let watched = self.load_watched_invoices(pool).await?;

        // Finalized slot is the watermark the persisted cursors are allowed to
        // reach. Everything above it is rescan territory every tick.
        let finalized_slot = self.get_slot("finalized").await?;

        if watched.is_empty() {
            // Nothing to do. Don't touch cursors — an address with no live
            // invoice and no in-flight payment is garbage-collected below, and
            // the rest keep their position for when their invoice comes back.
            self.prune_address_cursors(pool, &watched).await?;
            return Ok(());
        }

        // ── 1. Address -> invoices index ─────────────────────────────────────
        // One address can map to several invoices only in pathological cases
        // (a merchant reusing a deposit address across invoices, which the
        // creation path should prevent). The Vec keeps it correct anyway.
        let mut by_address: HashMap<String, Vec<WatchedInvoice>> = HashMap::new();
        for w in &watched {
            for addr in w.watch_addresses() {
                if addr.is_empty() {
                    continue;
                }
                by_address.entry(addr.to_string()).or_default().push(w.clone());
            }
        }

        let addresses: Vec<String> = by_address.keys().cloned().collect();

        // ── 2. Discover new signatures, concurrently, one cursor per address ──
        let discovered: Vec<(String, Result<Vec<SigRef>, String>)> = stream::iter(addresses)
            .map(|addr| async move {
                let res = self.discover_signatures(pool, &addr).await;
                (addr, res)
            })
            .buffer_unordered(ADDRESS_CONCURRENCY)
            .collect()
            .await;

        // Dedupe: the same transaction shows up on two feeds whenever a payer
        // hits the deposit address and the reference in one go, and the same
        // signature can legitimately serve two invoices. Fetch once, apply once
        // per (invoice, path).
        let mut sig_slots: HashMap<String, SigRef> = HashMap::new();
        let mut per_address: HashMap<String, Vec<SigRef>> = HashMap::new();

        for (addr, res) in discovered {
            match res {
                Ok(sigs) => {
                    for s in &sigs {
                        sig_slots.entry(s.signature.clone()).or_insert_with(|| s.clone());
                    }
                    per_address.insert(addr, sigs);
                }
                Err(e) => {
                    // Partial failure is fine: this address just doesn't advance
                    // its cursor this tick and retries on the next one.
                    eprintln!("[{}] signature scan failed for {addr}: {e}", self.network_name);
                }
            }
        }

        // ── 3. Fetch bodies in batches, oldest first ─────────────────────────
        let mut to_fetch: Vec<SigRef> = sig_slots
            .into_values()
            .filter(|s| !s.failed) // failed txs moved no money; nothing to credit
            .collect();
        to_fetch.sort_by_key(|s| s.slot);

        let mut applied: HashSet<String> = HashSet::new();

        for chunk in to_fetch.chunks(MAX_TX_PER_BATCH) {
            let calls: Vec<(&'static str, Value)> = chunk
                .iter()
                .map(|s| {
                    (
                        "getTransaction",
                        json!([
                            s.signature,
                            {
                                "encoding": "jsonParsed",
                                "commitment": DETECT_COMMITMENT,
                                "maxSupportedTransactionVersion": 0
                            }
                        ]),
                    )
                })
                .collect();

            let results = self.rpc_batch(&calls).await?;

            for (sig_ref, result) in chunk.iter().zip(results) {
                let raw = match result {
                    Ok(Value::Null) => {
                        // Listed by the index but not yet servable at this
                        // commitment on the node we hit. Leave it; the cursor
                        // won't advance past it and we'll get it next tick.
                        continue;
                    }
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "[{}] getTransaction {} failed: {e}",
                            self.network_name, sig_ref.signature
                        );
                        continue;
                    }
                };

                let tx = match parse_tx_view(&sig_ref.signature, &raw) {
                    Ok(t) => t,
                    Err(e) => {
                        eprintln!(
                            "[{}] could not parse tx {}: {e}",
                            self.network_name, sig_ref.signature
                        );
                        continue;
                    }
                };

                if tx.failed {
                    applied.insert(tx.signature.clone());
                    continue;
                }

                self.apply_transaction(pool, &tx, &watched).await?;
                applied.insert(tx.signature.clone());
            }
        }

        // ── 4. Advance cursors, but only up to the finalized watermark ───────
        //
        // The persisted cursor is a *resume point*, not a "how far have I
        // looked" marker. Parking it at the newest finalized signature means:
        //   - a restart never resumes from a slot that can later fork away;
        //   - the unfinalized window (~32 slots) is rescanned every tick, so a
        //     transaction that gets re-landed on the winning fork is picked up
        //     without any explicit reorg machinery;
        //   - the rescan is a handful of signatures per address, so it's free.
        for (addr, sigs) in &per_address {
            // Stop at the first signature we couldn't apply — never skip a hole.
            let mut best: Option<&SigRef> = None;
            for s in sigs {
                if s.slot > finalized_slot {
                    break;
                }
                if !s.failed && !applied.contains(&s.signature) {
                    break;
                }
                best = Some(s);
            }
            if let Some(s) = best {
                self.save_address_cursor(pool, addr, &s.signature, s.slot).await?;
            }
        }

        // ── 5. Promote / orphan everything in flight ─────────────────────────
        self.reconcile_statuses(pool, finalized_slot, &watched).await?;

        // ── 6. Housekeeping ──────────────────────────────────────────────────
        self.expire_invoices(pool).await?;
        self.prune_address_cursors(pool, &watched).await?;
        self.drop_settled_from_pending(pool, &watched).await?;

        Ok(())
    }

    // ── Discovery ────────────────────────────────────────────────────────────

    /// Everything new on this address's feed, oldest first.
    ///
    /// `getSignaturesForAddress` returns newest-first and stops early when it
    /// hits `until`. We page backwards from the tip until we hit the cursor,
    /// then reverse. Two independent stop conditions, because `until` alone is
    /// not safe: if the cursor signature is ever unreachable (pruned history on
    /// a non-archival node, or a fork that dropped it), `until` never matches
    /// and we'd page back to genesis. The slot floor catches that.
    async fn discover_signatures(
        &self,
        pool: &PgPool,
        address: &str,
    ) -> Result<Vec<SigRef>, String> {
        let cursor = self.load_address_cursor(pool, address).await?;

        // Cold start floors at the earliest `created_slot` of the invoices on
        // this address — an invoice can't be paid before it existed, so
        // anything older is somebody else's history.
        let floor_slot = match &cursor {
            Some((_, slot)) => *slot,
            None => self.cold_start_floor(pool, address).await?,
        };
        let until_sig = cursor.as_ref().map(|(s, _)| s.clone());

        let mut out: Vec<SigRef> = Vec::new();
        let mut before: Option<String> = None;

        for _page in 0..MAX_SIG_PAGES_PER_ADDRESS {
            let mut opts = Map::new();
            opts.insert("limit".into(), json!(SIG_PAGE_LIMIT));
            opts.insert("commitment".into(), json!(DETECT_COMMITMENT));
            if let Some(u) = &until_sig {
                opts.insert("until".into(), json!(u));
            }
            if let Some(b) = &before {
                opts.insert("before".into(), json!(b));
            }

            let page = self
                .rpc("getSignaturesForAddress", json!([address, Value::Object(opts)]))
                .await?;

            let entries = page
                .as_array()
                .ok_or_else(|| "getSignaturesForAddress returned non-array".to_string())?;

            if entries.is_empty() {
                break;
            }

            let mut hit_floor = false;
            let mut last_sig = None;

            for e in entries {
                let signature = e
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| "signature entry missing `signature`".to_string())?
                    .to_string();
                let slot = e.get("slot").and_then(|v| v.as_i64()).unwrap_or(0);

                last_sig = Some(signature.clone());

                if slot <= floor_slot && cursor.is_some() {
                    // At or below the resume point. Anything here is either the
                    // cursor itself or already applied.
                    hit_floor = true;
                    break;
                }
                if slot < floor_slot {
                    hit_floor = true;
                    break;
                }

                out.push(SigRef {
                    signature,
                    slot,
                    failed: e.get("err").map(|v| !v.is_null()).unwrap_or(false),
                });
            }

            if hit_floor || entries.len() < SIG_PAGE_LIMIT {
                break;
            }
            before = last_sig;
        }

        out.reverse(); // oldest first — money must be credited in order
        Ok(out)
    }

    /// Earliest slot worth looking at for a cold address.
    async fn cold_start_floor(&self, pool: &PgPool, address: &str) -> Result<i64, String> {
        let floor = sqlx::query_scalar::<_, Option<i64>>(
            r#"
    SELECT MIN(i.created_block)
      FROM invoices i
     WHERE i.network_type = $1
       AND i.chain_ref = $2
       AND (i.wallet_address = $3 OR i.payment_reference = $3)
    "#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(address)
            .fetch_one(pool)
            .await
            .map_err(|e| format!("cold_start_floor: {e}"))?;


        // No created_slot recorded (older rows) => take the whole feed, capped
        // by MAX_SIG_PAGES_PER_ADDRESS.
        Ok(floor.unwrap_or(0).max(0))
    }

    // ── Attribution ──────────────────────────────────────────────────────────

    /// Which invoice does this transaction pay, and by which path?
    ///
    /// Direct path first: a positive delta on the invoice's own deposit address
    /// in the invoice's own asset is unambiguous — nobody else can be paid at
    /// that address, and a native transfer can never satisfy a token invoice
    /// (or vice versa) because we look at a different balance table entirely.
    ///
    /// Reference path second, and only when exactly one of our references is
    /// present. Two references in one transaction would mean one credit to the
    /// merchant wallet with two claimants and no way to split it; that's a
    /// human problem, so we log loudly and credit nothing rather than guess.
    fn classify<'a>(
        &self,
        tx: &TxView,
        watched: &'a [WatchedInvoice],
    ) -> Vec<(&'a WatchedInvoice, Decimal, &'static str)> {
        let mut out: Vec<(&WatchedInvoice, Decimal, &'static str)> = Vec::new();

        // ── direct ──
        for inv in watched {
            if !tx.account_keys.contains(&inv.deposit_address) {
                continue;
            }
            if let Some(created) = inv.created_slot {
                if tx.slot < created {
                    continue;
                }
            }
            let credit = tx.credit(&inv.deposit_address, inv.mint.as_deref());
            if credit > 0 {
                match i128_to_decimal(credit) {
                    Ok(d) => out.push((inv, d, "direct")),
                    Err(e) => eprintln!("[{}] amount overflow on {}: {e}", self.network_name, tx.signature),
                }
            }
        }

        // ── reference ──
        let refs: Vec<&WatchedInvoice> = watched
            .iter()
            .filter(|inv| {
                !inv.payment_reference.is_empty()
                    && tx.account_keys.contains(&inv.payment_reference)
            })
            .collect();

        if refs.len() > 1 {
            eprintln!(
                "[{}] tx {} carries {} of our references ({}); refusing to attribute a \
                 merchant-wallet credit that can't be split. Manual review required.",
                self.network_name,
                tx.signature,
                refs.len(),
                refs.iter().map(|i| i.invoice_id.to_string()).collect::<Vec<_>>().join(", ")
            );
            return out;
        }

        if let Some(inv) = refs.first() {
            if let Some(created) = inv.created_slot {
                if tx.slot < created {
                    return out;
                }
            }
            // Already credited on the direct path in this same transaction?
            // Can happen if the payer's wallet routes through the deposit
            // address. Don't double count.
            if out.iter().any(|(i, _, _)| i.invoice_id == inv.invoice_id) {
                return out;
            }

            let credit = tx.credit(&inv.merchant_target, inv.mint.as_deref());
            if credit > 0 {
                match i128_to_decimal(credit) {
                    Ok(d) => out.push((inv, d, "reference")),
                    Err(e) => eprintln!("[{}] amount overflow on {}: {e}", self.network_name, tx.signature),
                }
            } else {
                // The reference is there but the merchant's balance in the
                // expected asset didn't move. Wrong mint, wrong destination, or
                // a memo-only transaction. Never credit on the reference alone.
                println!(
                    "[{}] tx {} carries reference for invoice {} but no {} credit to {} — ignored",
                    self.network_name,
                    tx.signature,
                    inv.invoice_id,
                    inv.mint.as_deref().unwrap_or("SOL"),
                    inv.merchant_target
                );
            }
        }

        out
    }

    /// Record every credit this transaction produced.
    ///
    /// Idempotent on (invoice_id, signature): rescanning the unfinalized window
    /// every tick hits the ON CONFLICT path and does nothing. The only UPDATE
    /// that can fire is the un-orphan / relocate case, for a transaction that
    /// was dropped and then re-landed.
    async fn apply_transaction(
        &self,
        pool: &PgPool,
        tx: &TxView,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        let credits = self.classify(tx, watched);

        for (inv, amount, path) in credits {
            let mut db_tx = pool
                .begin()
                .await
                .map_err(|e| format!("apply_transaction begin tx: {e}"))?;

            let inserted = sqlx::query(
                r#"
                INSERT INTO payments
                    (invoice_id, tx_hash, amount, block_number, block_hash,
                     confirmations, status, payment_path)
                VALUES ($1, $2, $3, $4, '', $5, 'detected', $6)
                ON CONFLICT (invoice_id, tx_hash) DO NOTHING
                "#,
            )
                .bind(inv.invoice_id)
                .bind(&tx.signature)
                .bind(amount)
                .bind(tx.slot)
                .bind(CONF_DETECTED)
                .bind(path)
                .execute(&mut *db_tx)
                .await
                .map_err(|e| format!("insert payment: {e}"))?
                .rows_affected()
                == 1;

            if inserted {
                println!(
                    "[{}] detected {} {} via {} path -> invoice {} (sig {}, slot {})",
                    self.network_name,
                    amount,
                    inv.mint.as_deref().unwrap_or("lamports"),
                    path,
                    inv.invoice_id,
                    tx.signature,
                    tx.slot
                );

                let mut fields = Map::new();
                fields.insert("Signature".into(), json!(tx.signature));
                fields.insert("AmountBaseUnits".into(), json!(amount.to_string()));
                fields.insert("Slot".into(), json!(tx.slot));
                fields.insert("Mint".into(), json!(inv.mint));
                fields.insert("PaymentPath".into(), json!(path));
                fields.insert("Confirmations".into(), json!(CONF_DETECTED));
                fields.insert("ConfirmationLevel".into(), json!("detected"));

                enqueue_webhook(
                    &mut db_tx,
                    inv.invoice_id,
                    "payment.detected",
                    &tx.signature,
                    fields,
                )
                    .await?;
            } else {
                // Seen before. The only thing worth writing is a resurrection:
                // a transaction we orphaned that has since re-landed. Amount is
                // never rewritten — the same signature always moved the same
                // money, and letting it change would let a replay inflate a
                // total.
                sqlx::query(
                    r#"
                    UPDATE payments
                       SET block_number = $2,
                           status = 'detected',
                           confirmations = $3,
                           updated_at = now()
                     WHERE invoice_id = $1
                       AND tx_hash = $4
                       AND status = 'orphaned'
                    "#,
                )
                    .bind(inv.invoice_id)
                    .bind(tx.slot)
                    .bind(CONF_DETECTED)
                    .bind(&tx.signature)
                    .execute(&mut *db_tx)
                    .await
                    .map_err(|e| format!("relocate payment: {e}"))?;
            }

            db_tx
                .commit()
                .await
                .map_err(|e| format!("apply_transaction commit: {e}"))?;

            self.recompute_invoice_totals(pool, inv.invoice_id, std::slice::from_ref(inv))
                .await?;
        }

        Ok(())
    }

    // ── Confirmation / finality ──────────────────────────────────────────────

    /// The Solana analogue of `refresh_confirmations` + `handle_reorg`, folded
    /// into one pass because on Solana they're the same question:
    /// "what does the cluster currently think of this signature?"
    ///
    ///   status == null && slot <= finalized  -> dropped for good  -> orphaned
    ///   status.err != null                   -> landed but failed -> orphaned
    ///   confirmationStatus                   -> the level it reached
    ///
    /// A transaction that got re-landed on a different fork keeps its signature
    /// and is simply found again — there is no EVM-style "re-mined into another
    /// block" case to write code for, because the signature is the identity,
    /// not the (block, index) pair.
    async fn reconcile_statuses(
        &self,
        pool: &PgPool,
        finalized_slot: i64,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        let ids: Vec<Uuid> = watched.iter().map(|w| w.invoice_id).collect();
        if ids.is_empty() {
            return Ok(());
        }

        let thresholds: HashMap<Uuid, ConfirmLevel> =
            watched.iter().map(|w| (w.invoice_id, w.level)).collect();

        // Anything already `system_confirmed` is finalized and irreversible —
        // it never needs looking at again. That's what keeps this bounded.
        let rows = sqlx::query_as::<_, (Uuid, Uuid, String, i64, String)>(
            r#"
            SELECT p.id, p.invoice_id, p.tx_hash, p.block_number, p.status
              FROM payments p
             WHERE p.invoice_id = ANY($1)
               AND p.status IN ('detected', 'merchant_confirmed')
             ORDER BY p.block_number ASC
            "#,
        )
            .bind(&ids)
            .fetch_all(pool)
            .await
            .map_err(|e| format!("reconcile_statuses select: {e}"))?;

        if rows.is_empty() {
            return Ok(());
        }

        let mut touched_invoices: HashSet<Uuid> = HashSet::new();

        for chunk in rows.chunks(MAX_STATUS_PER_BATCH) {
            let sigs: Vec<&str> = chunk.iter().map(|r| r.2.as_str()).collect();

            let res = self
                .rpc(
                    "getSignatureStatuses",
                    json!([sigs, { "searchTransactionHistory": true }]),
                )
                .await?;

            let statuses = res
                .get("value")
                .and_then(|v| v.as_array())
                .ok_or_else(|| "getSignatureStatuses returned no value array".to_string())?;

            for ((payment_id, invoice_id, signature, slot, status), st) in
                chunk.iter().zip(statuses)
            {
                let Some(level_target) = thresholds.get(invoice_id).copied() else { continue };
                touched_invoices.insert(*invoice_id);

                // ── dropped ──
                if st.is_null() {
                    // Below the finalized root and the cluster has never heard
                    // of it: it is gone and cannot come back. Above the root,
                    // it may just be propagating — leave it alone.
                    if *slot <= finalized_slot {
                        self.orphan_payment(
                            pool,
                            *payment_id,
                            *invoice_id,
                            signature,
                            *slot,
                            status,
                            "dropped before finalization",
                        )
                            .await?;
                    }
                    continue;
                }

                // ── landed but the transaction itself failed ──
                if st.get("err").map(|v| !v.is_null()).unwrap_or(false) {
                    self.orphan_payment(
                        pool,
                        *payment_id,
                        *invoice_id,
                        signature,
                        *slot,
                        status,
                        "transaction failed on-chain",
                    )
                        .await?;
                    continue;
                }

                let Some(reached) = st
                    .get("confirmationStatus")
                    .and_then(|v| v.as_str())
                    .and_then(ConfirmLevel::from_rpc)
                else {
                    continue;
                };

                self.promote_payment(
                    pool,
                    *payment_id,
                    *invoice_id,
                    signature,
                    *slot,
                    status,
                    reached,
                    level_target,
                )
                    .await?;
            }
        }

        for invoice_id in touched_invoices {
            self.recompute_invoice_totals(pool, invoice_id, watched).await?;
        }

        Ok(())
    }

    /// Write the level back and fire the threshold webhooks. Every state change
    /// is a guarded UPDATE, so two workers racing can't both emit.
    #[allow(clippy::too_many_arguments)]
    async fn promote_payment(
        &self,
        pool: &PgPool,
        payment_id: Uuid,
        invoice_id: Uuid,
        signature: &str,
        slot: i64,
        current_status: &str,
        reached: ConfirmLevel,
        required: ConfirmLevel,
    ) -> Result<(), String> {
        let conf = reached.as_confirmations();

        sqlx::query(
            r#"
            UPDATE payments
               SET confirmations = $2, updated_at = now()
             WHERE id = $1 AND confirmations <> $2
            "#,
        )
            .bind(payment_id)
            .bind(conf)
            .execute(pool)
            .await
            .map_err(|e| format!("update confirmations: {e}"))?;

        // ── merchant threshold ───────────────────────────────────────────────
        if current_status == "detected" && reached >= required {
            let promoted = sqlx::query(
                r#"
                UPDATE payments
                   SET status = 'merchant_confirmed', updated_at = now()
                 WHERE id = $1 AND status = 'detected'
                "#,
            )
                .bind(payment_id)
                .execute(pool)
                .await
                .map_err(|e| format!("promote merchant_confirmed: {e}"))?
                .rows_affected()
                == 1;

            if promoted {
                println!(
                    "[{}] payment {} reached {} (merchant requires {}) -> merchant_confirmed",
                    self.network_name,
                    payment_id,
                    reached.label(),
                    required.label()
                );

                let mut tx = pool
                    .begin()
                    .await
                    .map_err(|e| format!("promote begin tx (confirmed): {e}"))?;

                let mut fields = Map::new();
                fields.insert("PaymentId".into(), json!(payment_id));
                fields.insert("Signature".into(), json!(signature));
                fields.insert("Slot".into(), json!(slot));
                fields.insert("Confirmations".into(), json!(conf));
                fields.insert("ConfirmationLevel".into(), json!(reached.label()));
                fields.insert("RequiredConfirmations".into(), json!(required.as_confirmations()));
                fields.insert("RequiredLevel".into(), json!(required.label()));

                enqueue_webhook(
                    &mut tx,
                    invoice_id,
                    "payment.confirmed",
                    &payment_id.to_string(),
                    fields,
                )
                    .await?;

                tx.commit()
                    .await
                    .map_err(|e| format!("promote commit tx (confirmed): {e}"))?;
            }
        }

        // ── finality ─────────────────────────────────────────────────────────
        // Terminal. Once the cluster roots a slot it cannot be undone, so this
        // is also what drops the payment out of `reconcile_statuses` forever.
        if reached == ConfirmLevel::Finalized {
            let finalized = sqlx::query(
                r#"
                UPDATE payments
                   SET status = 'system_confirmed',
                       confirmations = $2,
                       updated_at = now()
                 WHERE id = $1 AND status <> 'system_confirmed'
                "#,
            )
                .bind(payment_id)
                .bind(CONF_FINALIZED)
                .execute(pool)
                .await
                .map_err(|e| format!("promote system_confirmed: {e}"))?
                .rows_affected()
                == 1;

            if finalized {
                println!(
                    "[{}] payment {} finalized at slot {}, no longer polled",
                    self.network_name, payment_id, slot
                );

                let mut tx = pool
                    .begin()
                    .await
                    .map_err(|e| format!("promote begin tx (finalized): {e}"))?;

                let mut fields = Map::new();
                fields.insert("PaymentId".into(), json!(payment_id));
                fields.insert("Signature".into(), json!(signature));
                fields.insert("Slot".into(), json!(slot));
                fields.insert("Confirmations".into(), json!(CONF_FINALIZED));
                fields.insert("ConfirmationLevel".into(), json!("finalized"));

                enqueue_webhook(
                    &mut tx,
                    invoice_id,
                    "payment.finalized",
                    &payment_id.to_string(),
                    fields,
                )
                    .await?;

                tx.commit()
                    .await
                    .map_err(|e| format!("promote commit tx (finalized): {e}"))?;
            }
        }

        Ok(())
    }

    /// The one case that always notifies the merchant: money we told them about
    /// is gone, so anything they shipped on the back of it has to be walked
    /// back on their side.
    #[allow(clippy::too_many_arguments)]
    async fn orphan_payment(
        &self,
        pool: &PgPool,
        payment_id: Uuid,
        invoice_id: Uuid,
        signature: &str,
        slot: i64,
        previous_status: &str,
        reason: &str,
    ) -> Result<(), String> {
        let mut tx = pool
            .begin()
            .await
            .map_err(|e| format!("orphan_payment begin tx: {e}"))?;

        let orphaned = sqlx::query(
            r#"
            UPDATE payments
               SET status = 'orphaned', confirmations = 0, updated_at = now()
             WHERE id = $1 AND status <> 'orphaned'
            "#,
        )
            .bind(payment_id)
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("orphan update: {e}"))?
            .rows_affected()
            == 1;

        if !orphaned {
            tx.rollback().await.ok();
            return Ok(());
        }

        println!(
            "[{}] payment {} orphaned ({}), sig {} at slot {}, prev status {}",
            self.network_name, payment_id, reason, signature, slot, previous_status
        );

        let mut fields = Map::new();
        fields.insert("PaymentId".into(), json!(payment_id));
        fields.insert("Signature".into(), json!(signature));
        fields.insert("Slot".into(), json!(slot));
        fields.insert("PreviousStatus".into(), json!(previous_status));
        fields.insert("Reason".into(), json!(reason));

        // TODO: skip this call entirely once merchant webhook settings exist
        //       and this merchant has opted out of orphaned notifications.
        enqueue_webhook(
            &mut tx,
            invoice_id,
            "payment.orphaned",
            &payment_id.to_string(),
            fields,
        )
            .await?;

        tx.commit()
            .await
            .map_err(|e| format!("orphan_payment commit tx: {e}"))?;

        Ok(())
    }

    // ── Invoice totals ───────────────────────────────────────────────────────

    /// Rebuild invoices.amount_received / status from the non-orphaned payments.
    /// Always a full recompute, never a delta, so rescans, duplicate ticks and
    /// dropped transactions all converge on the same number.
    ///
    /// This is also where multi-transfer underpayment resolves itself: three
    /// partial sends to the deposit address are three payment rows summing to
    /// the requested amount, and the invoice flips 'underpaid' -> 'paid' on the
    /// third without any special casing. The reference path is expected to be a
    /// single WalletConnect transaction, but it goes through exactly the same
    /// arithmetic, so a partial there behaves identically instead of being a
    /// code path nobody ever tested.
    async fn recompute_invoice_totals(
        &self,
        pool: &PgPool,
        invoice_id: Uuid,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        let Some(inv) = watched.iter().find(|w| w.invoice_id == invoice_id) else {
            return Ok(());
        };

        let received = sqlx::query_scalar::<_, Decimal>(
            r#"
        SELECT COALESCE(SUM(amount), 0)
          FROM payments
         WHERE invoice_id = $1 AND status <> 'orphaned'
        "#,
        )
            .bind(invoice_id)
            .fetch_one(pool)
            .await
            .map_err(|e| format!("sum payments: {e}"))?;

        let new_status = if received >= inv.amount_requested {
            if received > inv.amount_requested { "overpaid" } else { "paid" }
        } else if received > Decimal::ZERO {
            "underpaid"
        } else {
            "pending"
        };

        let status_change = sqlx::query_as::<_, (String, String)>(
            r#"
        WITH old AS (SELECT status AS prev FROM invoices WHERE id = $1)
        UPDATE invoices i
           SET amount_received = $2,
               status = CASE WHEN i.status = 'expired' THEN i.status ELSE $3 END,
               updated_at = now()
          FROM old
         WHERE i.id = $1
           AND (i.amount_received <> $2 OR (i.status <> $3 AND i.status <> 'expired'))
        RETURNING old.prev, i.status
        "#,
        )
            .bind(invoice_id)
            .bind(received)
            .bind(new_status)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("update invoice totals: {e}"))?;

        if let Some((prev, current)) = status_change {
            let was_settled = matches!(prev.as_str(), "paid" | "overpaid");
            let is_settled  = matches!(current.as_str(), "paid" | "overpaid");

            if is_settled && !was_settled {
                println!(
                    "[{}] invoice {} settled: received {} / requested {} ({})",
                    self.network_name, invoice_id, received, inv.amount_requested, current
                );

                let mut tx = pool
                    .begin()
                    .await
                    .map_err(|e| format!("recompute begin tx: {e}"))?;

                let mut fields = Map::new();
                fields.insert("AmountReceived".into(), json!(received.to_string()));
                fields.insert("AmountRequested".into(), json!(inv.amount_requested.to_string()));
                fields.insert("Overpaid".into(), json!(current == "overpaid"));
                fields.insert("Mint".into(), json!(inv.mint));

                let dedupe_key = format!("{}:{}", invoice_id, current);
                enqueue_webhook(&mut tx, invoice_id, "payment.finished", &dedupe_key, fields).await?;

                tx.commit()
                    .await
                    .map_err(|e| format!("recompute commit tx: {e}"))?;

                // TODO: make the trigger policy a merchant setting:
                //       'on_detected' (fire now, current behaviour),
                //       'on_confirmed' (every contributing payment must be
                //       merchant_confirmed first), or 'on_finalized'.
                // TODO: underpaid tolerance (dust / rounding) belongs here too,
                //       currently strict >=.
            } else if !is_settled && was_settled {
                // A dropped transaction clawed us back below the requested
                // amount. payment.orphaned already explained why, so no second
                // event here.
                println!(
                    "[{}] invoice {} fell back to {} after an orphan (received {})",
                    self.network_name, invoice_id, current, received
                );
            }
        }

        Ok(())
    }

    // ── Loading / scan state ─────────────────────────────────────────────────

    /// Everything worth watching: live invoices, plus dead invoices that still
    /// have payments counting toward a level. Same shape as the EVM scan plan,
    /// minus the block-range arithmetic — on Solana the "range" is just
    /// "whatever is newer than this address's cursor".
    async fn load_watched_invoices(&self, pool: &PgPool) -> Result<Vec<WatchedInvoice>, String> {
        // merchant_target is not a column — it's the merchant's main solana wallet
        // (native) or that wallet's ATA for the mint (SPL). We fetch the wallet via
        // merchant_wallets and derive the ATA in Rust.
        let rows = sqlx::query_as::<
            _,
            (
                Uuid,           // id
                Uuid,           // merchant_id
                String,         // wallet_address (deposit target)
                Option<String>, // payment_reference
                Option<String>, // merchant main wallet (nullable if unconfigured)
                Decimal,        // amount_requested
                Option<String>, // token_address (mint; null/"0"/"" => native)
                Option<i16>,    // required_confirmations
                Option<i64>,    // created_block
            ),
        >(
            r#"
        SELECT i.id,
               i.merchant_id,
               i.wallet_address,
               i.payment_reference,
               mw.address,
               i.amount_requested,
               i.token_address,
               i.required_confirmations,
               i.created_block
          FROM invoices i
          LEFT JOIN merchant_wallets mw
                 ON mw.merchant_id = i.merchant_id
                AND mw.network_type = $1
         WHERE i.network_type = $1
           AND i.chain_ref = $2
           AND i.wallet_address IS NOT NULL
           AND (
                 (i.status IN ('pending','underpaid') AND i.expires_at > now())
              OR EXISTS (
                   SELECT 1 FROM payments p
                    WHERE p.invoice_id = i.id
                      AND p.status IN ('detected','merchant_confirmed')
                 )
               )
        "#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .fetch_all(pool)
            .await
            .map_err(|e| format!("load_watched_invoices: {e}"))?;

        let mut out = Vec::with_capacity(rows.len());
        for (
            invoice_id,
            merchant_id,
            deposit_address,
            payment_reference,
            merchant_wallet,
            amount_requested,
            mint_raw,
            required_confirmations,
            created_slot,
        ) in rows
        {
            // Creation stores NULL for native; be defensive about "0"/"" too.
            let mint = mint_raw.filter(|m| !m.is_empty() && m != "0");
            let payment_reference = payment_reference.unwrap_or_default();

            let merchant_target = match (&merchant_wallet, &mint) {
                (Some(w), None)    => w.clone(),
                (Some(w), Some(m)) => derive_associated_token_address(w, m).unwrap_or_default(),
                (None, _)          => String::new(), // no wallet configured -> reference path inert
            };

            if merchant_target.is_empty() {
                eprintln!(
                    "[{}] invoice {} has no resolvable merchant target (missing merchant_wallets \
                 row for 'solana'?); reference path disabled for it",
                    self.network_name, invoice_id
                );
            }

            out.push(WatchedInvoice {
                invoice_id,
                merchant_id,
                deposit_address,
                payment_reference,
                merchant_target,
                amount_requested,
                mint,
                level: ConfirmLevel::from_required(required_confirmations.unwrap_or(1) as i64),
                created_slot,
            });
        }

        Ok(out)
    }


    async fn load_address_cursor(
        &self,
        pool: &PgPool,
        address: &str,
    ) -> Result<Option<(String, i64)>, String> {
        sqlx::query_as::<_, (String, i64)>(
            r#"
            SELECT last_signature, last_slot
              FROM network_address_cursors
             WHERE network_type = $1 AND chain_ref = $2 AND address = $3
            "#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(address)
            .fetch_optional(pool)
            .await
            .map_err(|e| format!("load_address_cursor({address}): {e}"))
    }

    /// Monotonic by slot: a stale worker or a late-arriving tick can never walk
    /// the cursor backwards and cause a re-scan storm.
    async fn save_address_cursor(
        &self,
        pool: &PgPool,
        address: &str,
        signature: &str,
        slot: i64,
    ) -> Result<(), String> {
        sqlx::query(
            r#"
            INSERT INTO network_address_cursors
                (network_type, chain_ref, address, last_signature, last_slot, updated_at)
            VALUES ($1, $2, $3, $4, $5, now())
            ON CONFLICT (network_type, chain_ref, address) DO UPDATE
               SET last_signature = EXCLUDED.last_signature,
                   last_slot = EXCLUDED.last_slot,
                   updated_at = now()
             WHERE network_address_cursors.last_slot < EXCLUDED.last_slot
            "#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(address)
            .bind(signature)
            .bind(slot)
            .execute(pool)
            .await
            .map_err(|e| format!("save_address_cursor({address}): {e}"))?;
        Ok(())
    }

    /// Drop cursors for addresses that no invoice cares about any more. Without
    /// this the table grows one row per invoice forever.
    async fn prune_address_cursors(
        &self,
        pool: &PgPool,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        let mut live: Vec<String> = Vec::with_capacity(watched.len() * 2);
        for w in watched {
            live.push(w.deposit_address.clone());
            if !w.payment_reference.is_empty() {
                live.push(w.payment_reference.clone());
            }
        }

        sqlx::query(
            r#"
            DELETE FROM network_address_cursors
             WHERE network_type = $1
               AND chain_ref = $2
               AND address <> ALL($3)
               AND updated_at < now() - interval '7 days'
            "#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .bind(&live)
            .execute(pool)
            .await
            .map_err(|e| format!("prune_address_cursors: {e}"))?;
        Ok(())
    }

    /// Wall-clock expiry. Nothing chain-derived — an invoice expires when its
    /// timer runs out, and any payment already recorded keeps counting toward
    /// finality regardless (the merchant still got the money, they just have to
    /// decide what to do about a late payer).
    async fn expire_invoices(&self, pool: &PgPool) -> Result<(), String> {
        let expired = sqlx::query_scalar::<_, Uuid>(
            r#"
            UPDATE invoices
               SET status = 'expired', updated_at = now()
             WHERE network_type = $1
               AND chain_ref = $2
               AND status = 'pending'
               AND expires_at <= now()
            RETURNING id
            "#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .fetch_all(pool)
            .await
            .map_err(|e| format!("expire_invoices: {e}"))?;

        for invoice_id in expired {
            println!("[{}] invoice {} expired unpaid", self.network_name, invoice_id);

            let mut tx = pool
                .begin()
                .await
                .map_err(|e| format!("expire_invoices begin tx: {e}"))?;

            let mut fields = Map::new();
            fields.insert("InvoiceId".into(), json!(invoice_id));

            enqueue_webhook(
                &mut tx,
                invoice_id,
                "invoice.expired",
                &invoice_id.to_string(),
                fields,
            )
                .await?;

            tx.commit()
                .await
                .map_err(|e| format!("expire_invoices commit tx: {e}"))?;
        }

        Ok(())
    }

    /// Keeps the in-memory hint map from growing forever. The DB query at the
    /// top of the tick already excludes these.
    async fn drop_settled_from_pending(
        &self,
        pool: &PgPool,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        let ids: Vec<Uuid> = watched.iter().map(|w| w.invoice_id).collect();
        if ids.is_empty() {
            return Ok(());
        }

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
            .fetch_all(pool)
            .await
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
}

// ═══════════════════════════════════════════════════════════════════════════
// Transaction parsing
// ═══════════════════════════════════════════════════════════════════════════

/// Reduce a `jsonParsed` transaction to balance deltas.
///
/// Deliberately does not look at instructions. Pre/post balances are the only
/// thing that can't lie about how much money moved: they cover `transfer`,
/// `transferChecked`, CPIs from routers and aggregators, several transfers in
/// one transaction, and account-creation-plus-transfer in one shot. Parsing
/// instruction shapes would mean maintaining a list of every way somebody can
/// send you a token.
///
/// `jsonParsed` merges address-lookup-table accounts into `message.accountKeys`
/// in balance-index order, so v0 transactions need no special handling — but
/// the account key list is still validated against the balance array length,
/// because a provider that doesn't do the merge would otherwise silently
/// misattribute every delta.
fn parse_tx_view(signature: &str, raw: &Value) -> Result<TxView, String> {
    let slot = raw
        .get("slot")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| "transaction missing slot".to_string())?;

    let meta = raw
        .get("meta")
        .ok_or_else(|| "transaction missing meta".to_string())?;

    let failed = meta.get("err").map(|v| !v.is_null()).unwrap_or(false);

    let keys_raw = raw
        .get("transaction")
        .and_then(|t| t.get("message"))
        .and_then(|m| m.get("accountKeys"))
        .and_then(|k| k.as_array())
        .ok_or_else(|| "transaction missing accountKeys".to_string())?;

    let mut ordered_keys: Vec<String> = Vec::with_capacity(keys_raw.len());
    for k in keys_raw {
        let pk = match k {
            // jsonParsed: { pubkey, signer, writable, source }
            Value::Object(o) => o
                .get("pubkey")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "accountKey object missing pubkey".to_string())?,
            // base58 encoding fallback
            Value::String(s) => s.as_str(),
            _ => return Err("unexpected accountKey shape".to_string()),
        };
        ordered_keys.push(pk.to_string());
    }

    let account_keys: HashSet<String> = ordered_keys.iter().cloned().collect();

    // ── native lamport deltas ──
    let pre = meta
        .get("preBalances")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "meta missing preBalances".to_string())?;
    let post = meta
        .get("postBalances")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "meta missing postBalances".to_string())?;

    if pre.len() != post.len() {
        return Err(format!(
            "pre/postBalances length mismatch on {signature}: {} vs {}",
            pre.len(),
            post.len()
        ));
    }
    if pre.len() > ordered_keys.len() {
        // The provider gave us balances for accounts it didn't name. Refusing
        // is the only safe move — guessing the alignment credits the wrong
        // address.
        return Err(format!(
            "{signature}: {} balances but only {} account keys (lookup tables not \
             expanded by this provider)",
            pre.len(),
            ordered_keys.len()
        ));
    }

    let mut native_delta: HashMap<String, i128> = HashMap::new();
    for (i, key) in ordered_keys.iter().enumerate().take(pre.len()) {
        let before = pre[i].as_i64().unwrap_or(0) as i128;
        let after = post[i].as_i64().unwrap_or(0) as i128;
        let d = after - before;
        if d != 0 {
            *native_delta.entry(key.clone()).or_insert(0) += d;
        }
    }

    // ── SPL token deltas ──
    // Keyed on (token account address, mint) rather than owner, because a
    // transfer names the ATA, not the wallet behind it — and because that's the
    // address the invoice actually stores and watches.
    let mut token_delta: HashMap<(String, String), i128> = HashMap::new();

    let mut fold = |arr: Option<&Value>, sign: i128| -> Result<(), String> {
        let Some(entries) = arr.and_then(|v| v.as_array()) else { return Ok(()) };
        for e in entries {
            let idx = e.get("accountIndex").and_then(|v| v.as_u64()).unwrap_or(u64::MAX) as usize;
            let Some(addr) = ordered_keys.get(idx) else { continue };
            let Some(mint) = e.get("mint").and_then(|v| v.as_str()) else { continue };
            let amount_str = e
                .get("uiTokenAmount")
                .and_then(|u| u.get("amount"))
                .and_then(|v| v.as_str())
                .unwrap_or("0");
            let amount: i128 = amount_str
                .parse()
                .map_err(|_| format!("{signature}: unparseable token amount {amount_str}"))?;
            *token_delta.entry((addr.clone(), mint.to_string())).or_insert(0) += sign * amount;
        }
        Ok(())
    };

    fold(meta.get("preTokenBalances"), -1)?;
    fold(meta.get("postTokenBalances"), 1)?;

    token_delta.retain(|_, v| *v != 0);

    Ok(TxView {
        signature: signature.to_string(),
        slot,
        failed,
        account_keys,
        native_delta,
        token_delta,
    })
}

fn i128_to_decimal(v: i128) -> Result<Decimal, String> {
    // Base units, scale 0 — decimals are a presentation-layer concern, same as
    // the EVM side.
    Decimal::try_from_i128_with_scale(v, 0).map_err(|e| format!("amount out of Decimal range: {e}"))
}

#[async_trait]
impl NetworkClient for SolanaNetwork {
    // --- WALLET METHODS ---
    async fn get_derive_address(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        _invoice_id: Uuid,
        mnemonic: &str,
        token_address: Option<&str>,
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
            .map_err(|e| format!("Failed to update merchant network index: {}", e))?;

        let index = u32::try_from(row.next_index - 1)
            .map_err(|_| format!("Invalid wallet index: {}", row.next_index - 1))?;

        // The HD-derived owner pubkey. This is simultaneously:
        //   - the deposit address, when the invoice is for native SOL
        //   - the ATA *owner*, when the invoice is for an SPL mint
        //   - the Solana Pay `reference` in both cases
        let owner_address = derive_solana_address(mnemonic, index)?;

        let deposit_address = match token_address {
            Some(mint) => derive_associated_token_address(&owner_address, mint)?,
            None => owner_address.clone(),
        };

        Ok((deposit_address, index, Some(owner_address)))
    }

    fn validate_address(&self, address: &str) -> bool {
        todo!()
    }

    async fn get_native_balance(&self, address: &str) -> Result<Amount, String> {
        todo!()
    }

    async fn get_token_balance(&self, token_address: &str, address: &str, decimals: u8) -> Result<Amount, String> {
        todo!()
    }

    /// Current slot at detection commitment. Stored as invoices.created_block so
    /// cold-start scans never page below the invoice's own birth.
    async fn get_current_block(&self) -> Result<u64, String> {
        let slot = self.get_slot(DETECT_COMMITMENT).await?;
        u64::try_from(slot).map_err(|_| "getSlot returned negative".to_string())
    }

    fn register_payment(&self, watch: PaymentWatch) {
        todo!()
    }

    fn unregister_payment(&self, invoice_id: Uuid) {
        todo!()
    }

    async fn watch_payments(&self, pool: &PgPool) -> Result<(), String> {
        todo!()
    }
}