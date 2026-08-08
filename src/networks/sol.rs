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
// Tunables
// ─────────────────────────────────────────────────────────────────────────────

const NETWORK_TYPE: &str = "solana";

/// ~400ms slots, but RPC providers rate-limit harder than they block.
const POLL_INTERVAL_SECS: u64 = 2;

/// `getSignaturesForAddress` hard cap.
const SIG_PAGE_LIMIT: usize = 1000;

/// Catch-up throttle per address per tick.
const MAX_SIG_PAGES_PER_ADDRESS: usize = 5;

/// JSON-RPC batch size for `getTransaction`.
const MAX_TX_PER_BATCH: usize = 50;

/// `getSignatureStatuses` hard cap.
const MAX_STATUS_PER_BATCH: usize = 256;

/// How many addresses we poll concurrently.
const ADDRESS_CONCURRENCY: usize = 8;

/// Canonical confirmation numbers written to `payments.confirmations`.
/// i32 because the column is INT, not BIGINT — binding i64 into an INT4
/// parameter is a decode error at runtime, which is what the old code did.
const CONF_DETECTED: i32 = 1;
const CONF_CONFIRMED: i32 = 16;
const CONF_FINALIZED: i32 = 32;

const DETECT_COMMITMENT: &str = "confirmed";

/// `invoices.created_block` is written from whatever slot the creating node
/// reported. A different node in the pool can be a few slots behind, and a very
/// fast payer can land in the same slot. Back the floor off so we never reject
/// a legitimate payment for being "before" the invoice existed.
const SLOT_SKEW_MARGIN: i64 = 64;

/// Before declaring a payment dropped we require it to be this far below the
/// finalized root. One flaky node returning null for a transaction it simply
/// hasn't indexed would otherwise orphan real money and fire a webhook.
const ORPHAN_GRACE_SLOTS: i64 = 150;

/// Cold-start ceiling when an invoice has no created_block at all.
const COLD_START_LOOKBACK_SLOTS: i64 = 216_000; // ~24h

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
    /// `invoices.required_confirmations` is SMALLINT -> i16.
    fn from_required(required: i16) -> Self {
        let required = i32::from(required);
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

    fn as_confirmations(self) -> i32 {
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
    #[allow(dead_code)]
    merchant_id: Uuid,
    /// `invoices.wallet_address`. ATA for SPL, owner pubkey for native.
    /// Watched, but never used as a credit key — see `owner_address`.
    deposit_address: String,
    /// `invoices.payment_reference` — the HD-derived owner pubkey. THIS is the
    /// credit key for both assets and both paths. Falls back to
    /// `deposit_address` for legacy rows where the reference was never stored
    /// (correct for native, where the two are the same string anyway).
    owner_address: String,
    amount_requested: Decimal,
    /// None => native SOL.
    mint: Option<String>,
    level: ConfirmLevel,
    created_slot: Option<i64>,
}

impl WatchedInvoice {
    /// Signature feeds that can carry money for this invoice.
    ///
    /// For SPL these are two distinct addresses and both are worth watching:
    /// the ATA feed catches every transfer, and the owner feed catches the
    /// smart path (the reference is a read-only account key) even in the window
    /// before the ATA exists on a provider's index.
    ///
    /// For native SOL they are the SAME string, so this returns one address.
    /// The old `[&str; 2]` returned the same address twice, which pushed the
    /// invoice into `by_address` twice — harmless, but it hid the fact that
    /// native has only one feed.
    fn watch_addresses(&self) -> Vec<&str> {
        let mut v = Vec::with_capacity(2);
        if !self.deposit_address.is_empty() {
            v.push(self.deposit_address.as_str());
        }
        if !self.owner_address.is_empty() && self.owner_address != self.deposit_address {
            v.push(self.owner_address.as_str());
        }
        v
    }

    fn floor_slot(&self) -> Option<i64> {
        self.created_slot.map(|s| (s - SLOT_SKEW_MARGIN).max(0))
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
    /// (token OWNER, mint) -> atom delta. Primary key for SPL crediting.
    token_delta_by_owner: HashMap<(String, String), i128>,
    /// (token ACCOUNT, mint) -> atom delta. Fallback for providers that omit
    /// `owner` on pre/postTokenBalances.
    token_delta_by_account: HashMap<(String, String), i128>,
}

impl TxView {
    fn native_credit(&self, address: &str) -> i128 {
        self.native_delta.get(address).copied().unwrap_or(0)
    }

    /// Credit for this invoice, in base units. One key, one lookup — this is
    /// what makes duplication structurally impossible.
    fn credit_for(&self, inv: &WatchedInvoice) -> i128 {
        match inv.mint.as_deref() {
            None => self.native_credit(&inv.owner_address),
            Some(mint) => {
                let key = (inv.owner_address.clone(), mint.to_string());
                if let Some(v) = self.token_delta_by_owner.get(&key) {
                    return *v;
                }
                // Provider didn't give us `owner`. Fall back to the ATA we
                // derived ourselves. Never additive with the branch above.
                self.token_delta_by_account
                    .get(&(inv.deposit_address.clone(), mint.to_string()))
                    .copied()
                    .unwrap_or(0)
            }
        }
    }
}

/// A signature we found on an address feed, before we've fetched the body.
#[derive(Clone)]
struct SigRef {
    signature: String,
    slot: i64,
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
}

impl SolanaNetwork {
    pub fn new(cluster: SolanaCluster, rpc_urls: Vec<String>) -> Self {
        assert!(
            !rpc_urls.is_empty(),
            "SolanaNetwork requires at least one RPC URL"
        );
        let network_name = format!("SOL_{cluster:?}");

        Self {
            cluster,
            rpc_urls,
            network_name,
            client: reqwest::Client::new(),
        }
    }

    /// MUST be the exact string `create_invoice_payment` writes into
    /// `invoices.chain_ref`. If they ever disagree the watcher silently sees
    /// zero invoices and every payment looks like it vanished.
    pub fn chain_ref(&self) -> String {
        format!("{:?}", self.cluster).to_lowercase()
    }

    // ── RPC ──────────────────────────────────────────────────────────────────

    async fn rpc(&self, method: &'static str, params: Value) -> Result<Value, String> {
        let mut last_err = String::new();
        for url in &self.rpc_urls {
            match self.call_rpc_single_json(url, method, params.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => last_err = e,
            }
        }
        Err(format!(
            "[{}] all endpoints failed for {method}: {last_err}",
            self.network_name
        ))
    }

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
                last_err = format!("Non-array batch response from {url}: {body}");
                continue;
            };

            let mut out: Vec<Result<Value, String>> = (0..calls.len())
                .map(|i| Err(format!("no response for batch id {i}")))
                .collect();

            for entry in entries {
                let Some(id) = entry.get("id").and_then(Value::as_u64) else {
                    continue;
                };
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

        Err(format!(
            "[{}] all endpoints failed for batch: {last_err}",
            self.network_name
        ))
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

    pub async fn get_slot(&self, commitment: &str) -> Result<i64, String> {
        let v = self
            .rpc("getSlot", json!([{ "commitment": commitment }]))
            .await?;
        v.as_i64()
            .ok_or_else(|| format!("getSlot returned non-integer: {v}"))
    }

    // ── The service loop ─────────────────────────────────────────────────────

    pub async fn watch_addresses(&self, pool: &PgPool) -> Result<(), String> {
        println!(
            "SolanaNetwork::watch_addresses service started for {}",
            self.network_name
        );

        loop {
            if let Err(e) = self.tick(pool).await {
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
        let finalized_slot = self.get_slot("finalized").await?;

        // Housekeeping that must happen whether or not anything is watched.
        self.expire_invoices(pool).await?;

        if watched.is_empty() {
            self.prune_address_cursors(pool, &watched).await?;
            return Ok(());
        }

        // ── 1. Address set ───────────────────────────────────────────────────
        let mut addresses: HashSet<String> = HashSet::new();
        for w in &watched {
            for addr in w.watch_addresses() {
                addresses.insert(addr.to_string());
            }
        }
        let addresses: Vec<String> = addresses.into_iter().collect();

        // ── 2. Discover new signatures, concurrently ─────────────────────────
        let discovered: Vec<(String, Result<Vec<SigRef>, String>)> = stream::iter(addresses)
            .map(|addr| async move {
                let res = self.discover_signatures(pool, &addr).await;
                (addr, res)
            })
            .buffer_unordered(ADDRESS_CONCURRENCY)
            .collect()
            .await;

        let mut sig_slots: HashMap<String, SigRef> = HashMap::new();
        let mut per_address: HashMap<String, Vec<SigRef>> = HashMap::new();

        for (addr, res) in discovered {
            match res {
                Ok(sigs) => {
                    for s in &sigs {
                        sig_slots
                            .entry(s.signature.clone())
                            .or_insert_with(|| s.clone());
                    }
                    per_address.insert(addr, sigs);
                }
                Err(e) => {
                    eprintln!(
                        "[{}] signature scan failed for {addr}: {e}",
                        self.network_name
                    );
                }
            }
        }

        // ── 3. Fetch bodies in batches, oldest first ─────────────────────────
        let mut to_fetch: Vec<SigRef> = sig_slots
            .into_values()
            .filter(|s| !s.failed)
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
                    // Listed by the index but not servable at this commitment
                    // on the node we hit. The cursor will not advance past it.
                    Ok(Value::Null) => continue,
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

                // Was `?`. One bad transaction used to abort the whole tick,
                // which also stopped every OTHER address advancing its cursor.
                // Now it just stalls this one signature; the cursor logic below
                // refuses to step over it, so nothing is skipped.
                match self.apply_transaction(pool, &tx, &watched).await {
                    Ok(()) => {
                        applied.insert(tx.signature.clone());
                    }
                    Err(e) => {
                        eprintln!(
                            "[{}] apply_transaction {} failed, will retry: {e}",
                            self.network_name, tx.signature
                        );
                    }
                }
            }
        }

        // ── 4. Advance cursors, capped at the finalized watermark ────────────
        for (addr, sigs) in &per_address {
            let mut best: Option<&SigRef> = None;
            for s in sigs {
                if s.slot > finalized_slot {
                    break;
                }
                if !s.failed && !applied.contains(&s.signature) {
                    break; // never step over a hole
                }
                best = Some(s);
            }
            if let Some(s) = best {
                self.save_address_cursor(pool, addr, &s.signature, s.slot)
                    .await?;
            }
        }

        // ── 5. Promote / orphan everything in flight ─────────────────────────
        self.reconcile_statuses(pool, finalized_slot, &watched)
            .await?;

        // ── 6. Housekeeping ──────────────────────────────────────────────────
        self.prune_address_cursors(pool, &watched).await?;

        Ok(())
    }

    // ── Discovery ────────────────────────────────────────────────────────────

    /// Everything new on this address's feed, oldest first.
    ///
    /// Two independent stop conditions:
    ///   1. the cursor SIGNATURE, matched exactly;
    ///   2. a strict slot floor, as a backstop for when the cursor signature is
    ///      unreachable (pruned history on a non-archival node) and `until`
    ///      would otherwise page back to genesis.
    ///
    /// The old version stopped on `slot <= floor_slot`, which silently dropped
    /// any transaction that landed in the SAME slot as the cursor signature but
    /// after it. Slots hold many transactions, so that is a real money-losing
    /// case, not a theoretical one. The floor is now strict (`<`), and the
    /// same-slot siblings get re-fetched and hit ON CONFLICT DO NOTHING.
    async fn discover_signatures(
        &self,
        pool: &PgPool,
        address: &str,
    ) -> Result<Vec<SigRef>, String> {
        let cursor = self.load_address_cursor(pool, address).await?;

        let (until_sig, floor_slot) = match &cursor {
            Some((sig, slot)) => (Some(sig.clone()), *slot),
            None => (None, self.cold_start_floor(pool, address).await?),
        };

        let mut out: Vec<SigRef> = Vec::new();
        let mut before: Option<String> = None;

        'pages: for _ in 0..MAX_SIG_PAGES_PER_ADDRESS {
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
                .rpc(
                    "getSignaturesForAddress",
                    json!([address, Value::Object(opts)]),
                )
                .await?;

            let entries = page
                .as_array()
                .ok_or_else(|| "getSignaturesForAddress returned non-array".to_string())?;

            if entries.is_empty() {
                break;
            }

            let page_len = entries.len();
            let mut last_sig = None;

            for e in entries {
                let signature = e
                    .get("signature")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "signature entry missing `signature`".to_string())?
                    .to_string();
                let slot = e.get("slot").and_then(Value::as_i64).unwrap_or(0);

                last_sig = Some(signature.clone());

                if until_sig.as_deref() == Some(signature.as_str()) {
                    break 'pages;
                }
                if slot < floor_slot {
                    break 'pages;
                }

                out.push(SigRef {
                    signature,
                    slot,
                    failed: e.get("err").map(|v| !v.is_null()).unwrap_or(false),
                });
            }

            if page_len < SIG_PAGE_LIMIT {
                break;
            }
            before = last_sig;
        }

        out.reverse(); // oldest first — money must be credited in order
        Ok(out)
    }

    /// Earliest slot worth looking at for a cold address.
    ///
    /// Uses `invoices.created_block` (your existing column) rather than the
    /// non-existent `created_slot`, and matches on the columns that actually
    /// exist: `wallet_address` / `payment_reference`.
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

        match floor {
            Some(slot) => Ok((slot - SLOT_SKEW_MARGIN).max(0)),
            // No created_block recorded. `0` used to mean "read the entire
            // history of this address", which for a reused address is unbounded
            // work every cold start. Bound it to a day.
            None => {
                let tip = self.get_slot("finalized").await?;
                Ok((tip - COLD_START_LOOKBACK_SLOTS).max(0))
            }
        }
    }

    // ── Attribution ──────────────────────────────────────────────────────────

    /// Which invoices does this transaction pay, and how much?
    ///
    /// One credit per invoice per transaction, from a single balance key.
    /// There is no direct-vs-reference branch any more, so there is no path
    /// through this function that can credit the same invoice twice for the
    /// same signature — which is what you were worried about for native SOL,
    /// where `wallet_address == payment_reference` makes the two old paths the
    /// same address.
    ///
    /// The multi-reference "refuse to attribute" case is also gone. It existed
    /// because a shared merchant wallet credit could not be split between two
    /// claimants. With per-invoice credit keys, two references in one
    /// transaction is just two independent credits, each unambiguous.
    ///
    /// `payment_path` is advisory. For native SOL it is always "direct"
    /// (the reference IS the destination — the two paths are observationally
    /// identical on-chain). For SPL, a naive payment that creates the ATA in
    /// the same transaction also names the owner, so it can be labelled
    /// "reference". Do not build accounting on this column.
    fn classify<'a>(
        &self,
        tx: &TxView,
        watched: &'a [WatchedInvoice],
    ) -> Vec<(&'a WatchedInvoice, Decimal, &'static str)> {
        let mut out: Vec<(&WatchedInvoice, Decimal, &'static str)> = Vec::new();

        for inv in watched {
            if inv.owner_address.is_empty() {
                continue;
            }
            if let Some(floor) = inv.floor_slot() {
                if tx.slot < floor {
                    continue;
                }
            }

            let credit = tx.credit_for(inv);
            if credit <= 0 {
                continue;
            }

            let referenced = inv.owner_address != inv.deposit_address
                && tx.account_keys.contains(&inv.owner_address);
            let path = if referenced { "reference" } else { "direct" };

            match i128_to_decimal(credit) {
                Ok(d) => out.push((inv, d, path)),
                Err(e) => eprintln!(
                    "[{}] amount overflow on {}: {e}",
                    self.network_name, tx.signature
                ),
            }
        }

        out
    }

    /// Record every credit this transaction produced.
    ///
    /// Idempotent on (invoice_id, tx_hash) — which now has a unique index, see
    /// the migration. Without it the ON CONFLICT clause was a hard runtime
    /// error on every single insert.
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

                // Dedupe key namespaced by invoice: one signature can credit
                // two invoices, and the bare signature would collide.
                let dedupe_key = format!("{}:{}", inv.invoice_id, tx.signature);
                enqueue_webhook(
                    &mut db_tx,
                    inv.invoice_id,
                    "payment.detected",
                    &dedupe_key,
                    fields,
                )
                    .await?;
            } else {
                // Resurrection only: a transaction we orphaned that re-landed.
                // Amount is never rewritten.
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
                .and_then(Value::as_array)
                .ok_or_else(|| "getSignatureStatuses returned no value array".to_string())?;

            // zip() silently truncates. A short array from a misbehaving
            // provider would leave the tail unexamined every tick — quiet, and
            // it never resolves.
            if statuses.len() != chunk.len() {
                return Err(format!(
                    "getSignatureStatuses returned {} statuses for {} signatures",
                    statuses.len(),
                    chunk.len()
                ));
            }

            for ((payment_id, invoice_id, signature, slot, status), st) in
                chunk.iter().zip(statuses)
            {
                let Some(level_target) = thresholds.get(invoice_id).copied() else {
                    continue;
                };
                touched_invoices.insert(*invoice_id);

                // ── dropped ──
                if st.is_null() {
                    // Grace window: one lagging or partially-indexed node
                    // returning null must not orphan real money.
                    if *slot + ORPHAN_GRACE_SLOTS <= finalized_slot {
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

                // ── landed but failed ──
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
                    .and_then(Value::as_str)
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
            self.recompute_invoice_totals(pool, invoice_id, watched)
                .await?;
        }

        Ok(())
    }

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
                fields.insert(
                    "RequiredConfirmations".into(),
                    json!(required.as_confirmations()),
                );
                fields.insert("RequiredLevel".into(), json!(required.label()));

                // Was `payment_id.to_string()` — identical to the key used by
                // the finalized and orphaned events for the same payment.
                let dedupe_key = format!("payment.confirmed:{payment_id}");
                enqueue_webhook(
                    &mut tx,
                    invoice_id,
                    "payment.confirmed",
                    &dedupe_key,
                    fields,
                )
                    .await?;

                tx.commit()
                    .await
                    .map_err(|e| format!("promote commit tx (confirmed): {e}"))?;
            }
        }

        // ── finality ─────────────────────────────────────────────────────────
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

                let dedupe_key = format!("payment.finalized:{payment_id}");
                enqueue_webhook(
                    &mut tx,
                    invoice_id,
                    "payment.finalized",
                    &dedupe_key,
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

        let dedupe_key = format!("payment.orphaned:{payment_id}");
        enqueue_webhook(
            &mut tx,
            invoice_id,
            "payment.orphaned",
            &dedupe_key,
            fields,
        )
            .await?;

        tx.commit()
            .await
            .map_err(|e| format!("orphan_payment commit tx: {e}"))?;

        Ok(())
    }

    // ── Invoice totals ───────────────────────────────────────────────────────

    /// Rebuild invoices.amount_received / status from non-orphaned payments.
    ///
    /// Now runs in one transaction with `SELECT ... FOR UPDATE`, so two workers
    /// (or the apply path racing the reconcile path) can't both read the same
    /// old status and both emit `payment.finished`.
    ///
    /// Also fixes a false positive: the old version decided "settled" from the
    /// status it WANTED to write, while the UPDATE had a
    /// `CASE WHEN status = 'expired'` guard that could refuse to write it. An
    /// expired invoice that later received money therefore emitted
    /// `payment.finished` while staying `expired` in the database. It now
    /// compares the stored before/after values.
    async fn recompute_invoice_totals(
        &self,
        pool: &PgPool,
        invoice_id: Uuid,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        let Some(inv) = watched.iter().find(|w| w.invoice_id == invoice_id) else {
            return Ok(());
        };

        let mut db_tx = pool
            .begin()
            .await
            .map_err(|e| format!("recompute begin tx: {e}"))?;

        let received = sqlx::query_scalar::<_, Decimal>(
            r#"
            SELECT COALESCE(SUM(amount), 0)
              FROM payments
             WHERE invoice_id = $1 AND status <> 'orphaned'
            "#,
        )
            .bind(invoice_id)
            .fetch_one(&mut *db_tx)
            .await
            .map_err(|e| format!("sum payments: {e}"))?;

        let target_status = if received >= inv.amount_requested {
            if received > inv.amount_requested {
                "overpaid"
            } else {
                "paid"
            }
        } else if received > Decimal::ZERO {
            "underpaid"
        } else {
            "pending"
        };

        let changed = sqlx::query_as::<_, (String, String)>(
            r#"
            WITH prev AS (
                SELECT id, status AS old_status
                  FROM invoices
                 WHERE id = $1
                   FOR UPDATE
            )
            UPDATE invoices i
               SET amount_received = $2,
                   status = CASE WHEN prev.old_status = 'expired'
                                 THEN 'expired'::varchar
                                 ELSE $3::varchar END,
                   updated_at = now()
              FROM prev
             WHERE i.id = prev.id
               AND (i.amount_received IS DISTINCT FROM $2
                    OR i.status IS DISTINCT FROM CASE WHEN prev.old_status = 'expired'
                                                      THEN 'expired'::varchar
                                                      ELSE $3::varchar END)
            RETURNING prev.old_status, i.status
            "#,
        )
            .bind(invoice_id)
            .bind(received)
            .bind(target_status)
            .fetch_optional(&mut *db_tx)
            .await
            .map_err(|e| format!("update invoice totals: {e}"))?;

        if let Some((old_status, new_status)) = changed {
            let was_settled = matches!(old_status.as_str(), "paid" | "overpaid");
            let is_settled = matches!(new_status.as_str(), "paid" | "overpaid");

            if is_settled && !was_settled {
                println!(
                    "[{}] invoice {} settled: received {} / requested {} ({})",
                    self.network_name, invoice_id, received, inv.amount_requested, new_status
                );

                let mut fields = Map::new();
                fields.insert("AmountReceived".into(), json!(received.to_string()));
                fields.insert(
                    "AmountRequested".into(),
                    json!(inv.amount_requested.to_string()),
                );
                fields.insert("Overpaid".into(), json!(new_status == "overpaid"));
                fields.insert("Mint".into(), json!(inv.mint));

                let dedupe_key = format!("{invoice_id}:{new_status}");
                enqueue_webhook(
                    &mut db_tx,
                    invoice_id,
                    "payment.finished",
                    &dedupe_key,
                    fields,
                )
                    .await?;

                // TODO: trigger policy as a merchant setting
                //       ('on_detected' | 'on_confirmed' | 'on_finalized').
                // TODO: underpaid dust tolerance; currently strict >=.
            } else if !is_settled && was_settled {
                println!(
                    "[{}] invoice {} fell back to {} after an orphan (received {})",
                    self.network_name, invoice_id, new_status, received
                );
            }
        }

        db_tx
            .commit()
            .await
            .map_err(|e| format!("recompute commit tx: {e}"))?;

        Ok(())
    }

    // ── Loading / scan state ─────────────────────────────────────────────────

    /// Column names match your actual schema. The old query referenced
    /// `deposit_address`, `merchant_target_address`, `mint_address` and
    /// `created_slot`, none of which exist — this function could never have
    /// returned a row.
    ///
    /// Types match too: `required_confirmations` is SMALLINT, so it decodes as
    /// i16, not i64.
    async fn load_watched_invoices(&self, pool: &PgPool) -> Result<Vec<WatchedInvoice>, String> {
        let rows = sqlx::query_as::<
            _,
            (
                Uuid,
                Uuid,
                String,
                String,
                Decimal,
                Option<String>,
                Option<i16>,
                Option<i64>,
            ),
        >(
            r#"
            SELECT i.id,
                   i.merchant_id,
                   i.wallet_address,
                   COALESCE(i.payment_reference, i.wallet_address),
                   i.amount_requested,
                   i.token_address,
                   i.required_confirmations,
                   i.created_block
              FROM invoices i
             WHERE i.network_type = $1
               AND i.chain_ref = $2
               AND i.wallet_address <> ''
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

        Ok(rows
            .into_iter()
            .map(
                |(
                     invoice_id,
                     merchant_id,
                     deposit_address,
                     owner_address,
                     amount_requested,
                     mint,
                     required_confirmations,
                     created_slot,
                 )| WatchedInvoice {
                    invoice_id,
                    merchant_id,
                    deposit_address,
                    owner_address,
                    amount_requested,
                    mint,
                    level: ConfirmLevel::from_required(required_confirmations.unwrap_or(1)),
                    created_slot,
                },
            )
            .collect())
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

    /// Monotonic by slot.
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
             WHERE network_address_cursors.last_slot <= EXCLUDED.last_slot
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

    /// `<=` above, not `<`: several signatures can share a slot, so the cursor
    /// must be able to move forward WITHIN a slot. With `<` the cursor stuck at
    /// the first signature of a busy slot and re-scanned it forever.
    async fn prune_address_cursors(
        &self,
        pool: &PgPool,
        watched: &[WatchedInvoice],
    ) -> Result<(), String> {
        let mut live: Vec<String> = Vec::with_capacity(watched.len() * 2);
        for w in watched {
            for a in w.watch_addresses() {
                live.push(a.to_string());
            }
        }

        sqlx::query(
            r#"
            DELETE FROM network_address_cursors
             WHERE network_type = $1
               AND chain_ref = $2
               AND address <> ALL($3::text[])
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

    /// Wall-clock expiry.
    ///
    /// Now covers `underpaid` as well as `pending`. Previously an underpaid
    /// invoice past its expiry dropped out of `load_watched_invoices` (which
    /// requires `expires_at > now()`) but stayed `underpaid` in the database
    /// forever — no expiry event, no terminal state, and the merchant's
    /// dashboard shows a row that will never move again.
    async fn expire_invoices(&self, pool: &PgPool) -> Result<(), String> {
        let expired = sqlx::query_as::<_, (Uuid, Decimal, String)>(
            r#"
            UPDATE invoices
               SET status = 'expired', updated_at = now()
             WHERE network_type = $1
               AND chain_ref = $2
               AND status IN ('pending', 'underpaid')
               AND expires_at <= now()
            RETURNING id, amount_received, 'expired'::varchar
            "#,
        )
            .bind(NETWORK_TYPE)
            .bind(self.chain_ref())
            .fetch_all(pool)
            .await
            .map_err(|e| format!("expire_invoices: {e}"))?;

        for (invoice_id, amount_received, _) in expired {
            println!(
                "[{}] invoice {} expired (received {})",
                self.network_name, invoice_id, amount_received
            );

            let mut tx = pool
                .begin()
                .await
                .map_err(|e| format!("expire_invoices begin tx: {e}"))?;

            let mut fields = Map::new();
            fields.insert("InvoiceId".into(), json!(invoice_id));
            fields.insert("AmountReceived".into(), json!(amount_received.to_string()));
            fields.insert("Partial".into(), json!(amount_received > Decimal::ZERO));

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
}

// ═══════════════════════════════════════════════════════════════════════════
// Transaction parsing
// ═══════════════════════════════════════════════════════════════════════════

/// Reduce a `jsonParsed` transaction to balance deltas.
///
/// Token deltas are now keyed by OWNER as well as by token account. Owner is
/// the primary key for crediting: it survives a payer using a non-ATA token
/// account, and it makes the naive and smart paths land on the same bucket
/// without any branching upstream.
fn parse_tx_view(signature: &str, raw: &Value) -> Result<TxView, String> {
    let slot = raw
        .get("slot")
        .and_then(Value::as_i64)
        .ok_or_else(|| "transaction missing slot".to_string())?;

    let meta = raw
        .get("meta")
        .ok_or_else(|| "transaction missing meta".to_string())?;

    let failed = meta.get("err").map(|v| !v.is_null()).unwrap_or(false);

    let keys_raw = raw
        .get("transaction")
        .and_then(|t| t.get("message"))
        .and_then(|m| m.get("accountKeys"))
        .and_then(Value::as_array)
        .ok_or_else(|| "transaction missing accountKeys".to_string())?;

    let mut ordered_keys: Vec<String> = Vec::with_capacity(keys_raw.len());
    for k in keys_raw {
        let pk = match k {
            Value::Object(o) => o
                .get("pubkey")
                .and_then(Value::as_str)
                .ok_or_else(|| "accountKey object missing pubkey".to_string())?,
            Value::String(s) => s.as_str(),
            _ => return Err("unexpected accountKey shape".to_string()),
        };
        ordered_keys.push(pk.to_string());
    }

    let account_keys: HashSet<String> = ordered_keys.iter().cloned().collect();

    // ── native lamport deltas ──
    let pre = meta
        .get("preBalances")
        .and_then(Value::as_array)
        .ok_or_else(|| "meta missing preBalances".to_string())?;
    let post = meta
        .get("postBalances")
        .and_then(Value::as_array)
        .ok_or_else(|| "meta missing postBalances".to_string())?;

    // Strict equality now. `pre.len() > keys.len()` was checked, but the
    // reverse (more keys than balances) was silently truncated with take(),
    // and any mismatch at all means the provider is not merging lookup-table
    // addresses in balance-index order — which misattributes every delta.
    if pre.len() != post.len() || pre.len() != ordered_keys.len() {
        return Err(format!(
            "{signature}: balance/account-key length mismatch \
             (pre {}, post {}, keys {}) — provider is not expanding address \
             lookup tables into accountKeys",
            pre.len(),
            post.len(),
            ordered_keys.len()
        ));
    }

    let mut native_delta: HashMap<String, i128> = HashMap::new();
    for (i, key) in ordered_keys.iter().enumerate() {
        let before = i128::from(pre[i].as_u64().unwrap_or(0));
        let after = i128::from(post[i].as_u64().unwrap_or(0));
        let d = after - before;
        if d != 0 {
            *native_delta.entry(key.clone()).or_insert(0) += d;
        }
    }

    // ── SPL token deltas ──
    let mut token_delta_by_owner: HashMap<(String, String), i128> = HashMap::new();
    let mut token_delta_by_account: HashMap<(String, String), i128> = HashMap::new();

    let mut fold = |arr: Option<&Value>, sign: i128| -> Result<(), String> {
        let Some(entries) = arr.and_then(Value::as_array) else {
            return Ok(());
        };
        for e in entries {
            let idx = e
                .get("accountIndex")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX) as usize;
            let Some(addr) = ordered_keys.get(idx) else {
                continue;
            };
            let Some(mint) = e.get("mint").and_then(Value::as_str) else {
                continue;
            };
            let amount_str = e
                .get("uiTokenAmount")
                .and_then(|u| u.get("amount"))
                .and_then(Value::as_str)
                .unwrap_or("0");
            let amount: i128 = amount_str
                .parse()
                .map_err(|_| format!("{signature}: unparseable token amount {amount_str}"))?;

            *token_delta_by_account
                .entry((addr.clone(), mint.to_string()))
                .or_insert(0) += sign * amount;

            if let Some(owner) = e.get("owner").and_then(Value::as_str) {
                *token_delta_by_owner
                    .entry((owner.to_string(), mint.to_string()))
                    .or_insert(0) += sign * amount;
            }
        }
        Ok(())
    };

    fold(meta.get("preTokenBalances"), -1)?;
    fold(meta.get("postTokenBalances"), 1)?;

    token_delta_by_owner.retain(|_, v| *v != 0);
    token_delta_by_account.retain(|_, v| *v != 0);

    Ok(TxView {
        signature: signature.to_string(),
        slot,
        failed,
        account_keys,
        native_delta,
        token_delta_by_owner,
        token_delta_by_account,
    })
}

fn i128_to_decimal(v: i128) -> Result<Decimal, String> {
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

    async fn get_current_block(&self) -> Result<u64, String> {
        todo!()
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