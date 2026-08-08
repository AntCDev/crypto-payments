use super::{Amount, NetworkClient, PaymentWatch, SolanaCluster};
use async_trait::async_trait;
use uuid::Uuid;

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;

use ed25519_dalek::SigningKey;
use hmac::{Hmac, KeyInit, Mac}; // Added KeyInit here
use sha2::{Sha256, Sha512, Digest};
use sqlx::PgPool;

use bip39::Mnemonic;
use curve25519_dalek::edwards::CompressedEdwardsY;

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
            segment
                .trim_end_matches('\'')
                .parse::<u32>()
                .map_err(|_| format!("Invalid path segment: {}", segment))
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


// ==========================================
// ### NETWORK IMPLEMENTATION ###
// ==========================================
pub struct SolanaNetwork {
    rpc_urls: Vec<String>,
    pub network_name: String,
    client: reqwest::Client,
    pending: Mutex<HashMap<Uuid, PaymentWatch>>,
}

impl SolanaNetwork {
    /// Constructor matching the NetworkRegistry initialization signature
    pub fn new(cluster: SolanaCluster, rpc_urls: Vec<String>) -> Self {
        assert!(!rpc_urls.is_empty(), "SolanaNetwork requires at least one RPC URL");

        // Dynamically creates names like "SOL_MainnetBeta", "SOL_Devnet", etc.
        let network_name = format!("SOL_{:?}", cluster);

        Self {
            rpc_urls,
            network_name,
            client: reqwest::Client::new(),
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Single RPC helper returning a raw JSON Value to allow safe variant comparison during Quorum checks
    async fn call_rpc_single(
        &self,
        url: &str,
        method: &'static str,
        params: serde_json::Value
    ) -> Result<serde_json::Value, String> {
        let payload = RpcRequest {
            jsonrpc: "2.0",
            method,
            params,
            id: 1,
        };

        let response = self.client
            .post(url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("HTTP Request failed to {url}: {e}"))?;

        let rpc_res: RpcResponse<serde_json::Value> = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse JSON response from {url}: {e}"))?;

        if let Some(err) = rpc_res.error {
            return Err(format!("RPC Error from {url}: {}", err.message));
        }

        rpc_res.result.ok_or_else(|| format!("No result found in RPC response from {url}"))
    }

    /// Fans out to all Solana endpoints and enforces a 2-node agreement quorum.
    /// Deserializes into the target type T only after consensus is established.
    async fn call_rpc<T: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<T, String> {
        // Fast path for single-endpoint configurations (e.g., local test/dev nodes)
        if self.rpc_urls.len() == 1 {
            let raw_val = self.call_rpc_single(&self.rpc_urls[0], method, params).await?;
            return serde_json::from_value(raw_val)
                .map_err(|e| format!("Failed to deserialize response: {e}"));
        }

        let futures = self.rpc_urls.iter()
            .map(|url| self.call_rpc_single(url, method, params.clone()));
        let results: Vec<Result<serde_json::Value, String>> = futures::future::join_all(futures).await;

        let oks: Vec<&serde_json::Value> = results.iter().filter_map(|r| r.as_ref().ok()).collect();

        if oks.len() < 2 {
            let errs: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
            return Err(format!(
                "Quorum failed for {method} on network {}: only {}/{} endpoints responded. Errors: {:?}",
                self.network_name, oks.len(), self.rpc_urls.len(), errs
            ));
        }

        // Identify the first value agreed upon by at least 2 distinct endpoints
        let mut quorum_winner = None;
        for candidate in &oks {
            if oks.iter().filter(|v| *v == candidate).count() >= 2 {
                quorum_winner = Some(*candidate);
                break;
            }
        }

        if let Some(winner) = quorum_winner {
            serde_json::from_value(winner.clone())
                .map_err(|e| format!("Failed to deserialize quorum-verified JSON response: {e}"))
        } else {
            Err(format!(
                "Quorum disagreement for {method} on network {}: endpoints returned mismatched state: {:?}",
                self.network_name, oks
            ))
        }
    }

}

#[async_trait]
impl NetworkClient for SolanaNetwork {
    // --- WALLET METHODS ---
    async fn get_derive_address(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        mnemonic: &str,
        token_address: Option<&str>,
    ) -> Result<(String, u32, Option<String>), String> {
        // Same custodial index-tracking pattern as your BTC implementation
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

        let index = (row.next_index - 1) as u32;
        let wallet_address = derive_solana_address(mnemonic, index)?;

        let deposit_address = match token_address {
            Some(mint) => derive_associated_token_address(&wallet_address, mint)?,
            None => wallet_address,
        };

        let reference = format!("0x{}", hex::encode(invoice_id.as_bytes()));
        Ok((deposit_address, index, Some(reference)))
    }


    fn validate_address(&self, address: &str) -> bool {
        if address.len() < 32 || address.len() > 44 {
            return false;
        }

        match bs58::decode(address).into_vec() {
            Ok(bytes) => bytes.len() == 32,
            Err(_) => false,
        }
    }

    // --- CHAIN STATE METHODS ---
    async fn get_native_balance(&self, address: &str) -> Result<Amount, String> {
        let params = json!([address]);
        let balance_info: SolBalanceValue = self.call_rpc("getBalance", params).await?;

        Ok(Amount(balance_info.value as u128))
    }

    async fn get_token_balance(
        &self,
        token_address: &str,
        address: &str,
        _decimals: u8,
    ) -> Result<Amount, String> {
        let params = json!([
            address,
            { "mint": token_address },
            { "encoding": "jsonParsed" }
        ]);

        let response: SolTokenAccountsValue = self.call_rpc("getTokenAccountsByOwner", params).await?;

        if response.value.is_empty() {
            return Ok(Amount(0));
        }

        let amount_str = response.value[0]
            .get("account")
            .and_then(|a| a.get("data"))
            .and_then(|d| d.get("parsed"))
            .and_then(|p| p.get("info"))
            .and_then(|i| i.get("tokenAmount"))
            .and_then(|t| t.get("amount"))
            .and_then(|amt| amt.as_str())
            .ok_or_else(|| "Failed to navigate token balance fields in RPC response".to_string())?;

        let raw_units = amount_str.parse::<u128>()
            .map_err(|_| "Failed to parse token balance integer".to_string())?;

        Ok(Amount(raw_units))
    }

    async fn get_current_block(&self) -> Result<u64, String> {
        let block_height: u64 = self.call_rpc("getBlockHeight", json!([])).await?;
        Ok(block_height)
    }

    // --- BATCHED WATCHING METHODS ---
    fn register_payment(&self, watch: PaymentWatch) {
        if let Ok(mut pending) = self.pending.lock() {
            println!("SolanaNetwork::register_payment for invoice: {}", watch.invoice_id);
            pending.insert(watch.invoice_id, watch);
        }
    }

    fn unregister_payment(&self, invoice_id: Uuid) {
        if let Ok(mut pending) = self.pending.lock() {
            println!("SolanaNetwork::unregister_payment for invoice: {}", invoice_id);
            pending.remove(&invoice_id);
        }
    }

    async fn watch_payments(&self, pool: &PgPool) -> Result<(), String> {
        println!("SolanaNetwork::watch_payments processing loop started on endpoints: {:?}", self.rpc_urls);

        struct TrackingState {
            target_balance: u128,
            detection_block: Option<u64>,
        }
        let mut tracking_registry: HashMap<Uuid, TrackingState> = HashMap::new();

        loop {
            let current_watches: Vec<PaymentWatch> = match self.pending.lock() {
                Ok(p) => p.values().cloned().collect(),
                Err(_) => return Err("Pending payments lock poisoned".to_string()),
            };

            tracking_registry.retain(|id, _| current_watches.iter().any(|w| w.invoice_id == *id));

            if !current_watches.is_empty() {
                let current_block = self.get_current_block().await?;
                let mut completed_invoices = Vec::new();

                for watch in current_watches {
                    let state = if let Some(s) = tracking_registry.get_mut(&watch.invoice_id) {
                        s
                    } else {
                        let initial_balance = match &watch.token_address {
                            Some(token) => self.get_token_balance(token, &watch.address, watch.decimals).await?.0,
                            None => self.get_native_balance(&watch.address).await?.0,
                        };
                        tracking_registry.insert(watch.invoice_id, TrackingState {
                            target_balance: initial_balance + watch.target_amount,
                            detection_block: None,
                        });
                        tracking_registry.get_mut(&watch.invoice_id).unwrap()
                    };

                    let current_balance = match &watch.token_address {
                        Some(token) => self.get_token_balance(token, &watch.address, watch.decimals).await?.0,
                        None => self.get_native_balance(&watch.address).await?.0,
                    };

                    if state.detection_block.is_none() {
                        if current_balance >= state.target_balance {
                            state.detection_block = Some(current_block);
                            println!(
                                "Solana Invoice {}: Payment detected at block {}! Awaiting {} confirmations...",
                                watch.invoice_id, current_block, watch.required_confirmations
                            );
                        }
                    } else if let Some(detected_at) = state.detection_block {
                        let confirmations = if current_block >= detected_at {
                            (current_block - detected_at) + 1
                        } else {
                            0
                        };

                        println!(
                            "Solana Invoice {}: Confirmation progress: {}/{}",
                            watch.invoice_id, confirmations, watch.required_confirmations
                        );

                        if confirmations >= watch.required_confirmations as u64 {
                            if current_balance >= state.target_balance {
                                println!("Solana Invoice {}: Payment fully confirmed successfully!", watch.invoice_id);
                                completed_invoices.push(watch.invoice_id);
                            } else {
                                println!("Solana Invoice {}: ⚠ Fork/Re-org detected! Resetting tracker.", watch.invoice_id);
                                state.detection_block = None;
                            }
                        }
                    }
                }

                for id in completed_invoices {
                    self.unregister_payment(id);
                }
            }

            sleep(Duration::from_secs(2)).await;
        }
    }
}

// ==========================================
// ### PRIVATE UTILITY FUNCTIONS ###
// ==========================================
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