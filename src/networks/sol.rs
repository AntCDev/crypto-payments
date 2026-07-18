use super::{Amount, NetworkClient, PaymentWatch};
use async_trait::async_trait;
use uuid::Uuid;

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;

use ed25519_dalek::SigningKey;
use hmac::{Hmac, Mac};
use sha2::Sha512;
use sqlx::PgPool;

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

// ==========================================
// ### NETWORK IMPLEMENTATION ###
// ==========================================
pub struct SolanaNetwork {
    rpc_url: String,
    network_name: String,
    client: reqwest::Client,
    pending: Mutex<HashMap<Uuid, PaymentWatch>>,
}

impl SolanaNetwork {
    /// Generic RPC helper to handle different Solana JSON-RPC response shapes cleanly
    async fn call_rpc<T: serde::de::DeserializeOwned>(
        &self,
        method: &'static str,
        params: serde_json::Value,
    ) -> Result<T, String> {
        let payload = RpcRequest {
            jsonrpc: "2.0",
            method,
            params,
            id: 1,
        };

        let response = self.client
            .post(&self.rpc_url)
            .json(&payload)
            .send()
            .await
            .map_err(|e| format!("HTTP Request failed: {}", e))?;

        let rpc_res: RpcResponse<T> = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse JSON response: {}", e))?;

        if let Some(err) = rpc_res.error {
            return Err(format!("RPC Error: {}", err.message));
        }

        rpc_res.result.ok_or_else(|| "No result found in RPC response".to_string())
    }

    pub fn derive_address(&self, mnemonic: &str, index: u32) -> Result<String, String> {
        let mnemonic_parsed = bip39::Mnemonic::parse(mnemonic)
            .map_err(|e| format!("Invalid mnemonic: {}", e))?;

        let seed = mnemonic_parsed.to_seed("");

        let path_str = self.get_derivation_path(index);
        let indices = parse_derivation_path(&path_str)?;

        type HmacSha512 = Hmac<Sha512>;
        let mut mac = HmacSha512::new_from_slice(b"ed25519 seed")
            .map_err(|e| format!("HMAC initialization failed: {}", e))?;
        mac.update(&seed);
        let hmac_result = mac.finalize().into_bytes();

        let mut secret_key: [u8; 32] = hmac_result[0..32].try_into().unwrap();
        let mut chain_code: [u8; 32] = hmac_result[32..64].try_into().unwrap();

        for idx in indices {
            if idx < 0x8000_0000 {
                return Err("SLIP-0010 Ed25519 only supports hardened derivation paths".to_string());
            }

            let mut mac = HmacSha512::new_from_slice(&chain_code)
                .map_err(|e| format!("HMAC initialization failed: {}", e))?;

            mac.update(&[0x00]);
            mac.update(&secret_key);
            mac.update(&idx.to_be_bytes());

            let result = mac.finalize().into_bytes();
            secret_key.copy_from_slice(&result[0..32]);
            chain_code.copy_from_slice(&result[32..64]);
        }

        let signing_key = SigningKey::from_bytes(&secret_key);
        let verifying_key = signing_key.verifying_key();

        Ok(bs58::encode(verifying_key.to_bytes()).into_string())
    }
}

#[async_trait]
impl NetworkClient for SolanaNetwork {
    fn new(rpc_url: &str) -> Self {
        let network_name = "SOL".to_string();
        
        Self {
            rpc_url: rpc_url.to_string(),
            network_name,
            client: reqwest::Client::new(),
            pending: Mutex::new(HashMap::new()),
        }
    }

    // --- WALLET METHODS ---
    async fn get_derive_address(&self, pool: &PgPool, merchant_id: Uuid, mnemonic: &str ) -> Result<(String, u32), String> {
        // Solana is an account-based architecture, so like EVM, it does not suffer from UTXO address gaps.
        // We safely default to index 0 for the merchant's deployment layout.
        let index = 0;
        let address = self.derive_address(mnemonic, index)?;
        Ok((address, index))
    }

    fn get_derivation_path(&self, index: u32) -> String {
        format!("m/44'/501'/{}'/0'", index)
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

        // Native SOL RPC returns Lamports directly as a u64, safely convert to u128
        Ok(Amount(balance_info.value as u128))
    }

    async fn get_token_balance(
        &self,
        token_address: &str,
        address: &str,
        _decimals: u8, // Unused since we deal strictly in raw u128 atomic units now
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

        // Safe extraction layer through Solana's heavily nested JSON structure
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

    async fn watch_payments(&self) -> Result<(), String> {
        println!("SolanaNetwork::watch_payments processing loop started on {}", self.rpc_url);

        struct TrackingState {
            target_balance: u128, // Changed from f64 to u128
            detection_block: Option<u64>,
        }
        let mut tracking_registry: HashMap<Uuid, TrackingState> = HashMap::new();

        loop {
            // Snapshot active watches under short lock
            let current_watches: Vec<PaymentWatch> = match self.pending.lock() {
                Ok(p) => p.values().cloned().collect(),
                Err(_) => return Err("Pending payments lock poisoned".to_string()),
            };

            // Clean tracking register of components dropped externally
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

            // Solana block times are fast, loop operates at a swift 2-second heartbeat
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