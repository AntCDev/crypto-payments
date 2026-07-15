use super::{Amount, NetworkClient, PaymentWatch};
use async_trait::async_trait;
use uuid::Uuid;

use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;

use bip32::{DerivationPath, PrivateKey, XPrv};
use bip39::Mnemonic;
use sha3::{Digest, Keccak256};

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

// ==========================================
// ### NETWORK IMPLEMENTATION ###
// ==========================================
pub struct EVMNetwork {
    rpc_url: String,
    client: reqwest::Client,
    pending: Mutex<HashMap<Uuid, PaymentWatch>>,
}

impl EVMNetwork {
    /// Helper to execute JSON-RPC calls against the configured endpoint
    async fn call_rpc(&self, method: &'static str, params: serde_json::Value) -> Result<String, String> {
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

        let rpc_res: RpcResponse = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse JSON response: {}", e))?;

        if let Some(err) = rpc_res.error {
            return Err(format!("RPC Error: {}", err.message));
        }

        rpc_res.result.ok_or_else(|| "No result found in RPC response".to_string())
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
}

#[async_trait]
impl NetworkClient for EVMNetwork {
    fn new(rpc_url: &str) -> Self {
        Self {
            rpc_url: rpc_url.to_string(),
            client: reqwest::Client::new(),
            pending: Mutex::new(HashMap::new()),
        }
    }

    // --- WALLET METHODS ---

    fn derive_address(&self, mnemonic: &str, index: u32) -> Result<String, String> {
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
        let params = json!([address, "latest"]);
        let hex_balance = self.call_rpc("eth_getBalance", params).await?;
        Self::parse_hex_balance(&hex_balance)
    }

    async fn get_token_balance(
        &self,
        token_address: &str,
        address: &str,
        _decimals: u8, // Prefixed with underscore as raw u128 logic does not require scaling here
    ) -> Result<Amount, String> {
        let clean_addr = address.trim_start_matches("0x");
        let data = format!("0x70a08231{:0>64}", clean_addr); // balanceOf selector

        let params = json!([
            {
                "to": token_address,
                "data": data
            },
            "latest"
        ]);
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

    async fn watch_payments(&self) -> Result<(), String> {
        println!("EVMNetwork::watch_payments processing loop started on {}", self.rpc_url);

        // Persistent tracking state scoped to the worker loop lifecycle
        struct TrackingState {
            target_balance: u128, // Changed from f64 to u128
            detection_block: Option<u64>,
        }
        let mut tracking_registry: HashMap<Uuid, TrackingState> = HashMap::new();

        loop {
            // Take a short-lived lock snapshot to prevent blocking incoming registrations
            let current_watches: Vec<PaymentWatch> = match self.pending.lock() {
                Ok(p) => p.values().cloned().collect(),
                Err(_) => return Err("Pending payments lock poisoned".to_string()),
            };

            // Clean up old internal tracking entries that were unregistered externally
            tracking_registry.retain(|id, _| current_watches.iter().any(|w| w.invoice_id == *id));

            if !current_watches.is_empty() {
                let current_block = self.get_current_block().await?;
                let mut completed_invoices = Vec::new();

                for watch in current_watches {
                    // Fetch or initialize the target threshold state dynamically
                    let state = if let Some(s) = tracking_registry.get_mut(&watch.invoice_id) {
                        s
                    } else {
                        let initial_balance = match &watch.token_address {
                            Some(token) => self.get_token_balance(token, &watch.address, watch.decimals).await?.0,
                            None => self.get_native_balance(&watch.address).await?.0,
                        };
                        tracking_registry.insert(watch.invoice_id, TrackingState {
                            target_balance: initial_balance + watch.target_amount, // Safe integer addition
                            detection_block: None,
                        });
                        tracking_registry.get_mut(&watch.invoice_id).unwrap()
                    };

                    // Evaluate current balance status
                    let current_balance = match &watch.token_address {
                        Some(token) => self.get_token_balance(token, &watch.address, watch.decimals).await?.0,
                        None => self.get_native_balance(&watch.address).await?.0,
                    };

                    if state.detection_block.is_none() {
                        if current_balance >= state.target_balance {
                            state.detection_block = Some(current_block);
                            println!(
                                "Invoice {}: Payment detected at block {}! Awaiting {} confirmations...",
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
                            "Invoice {}: Confirmation progress: {}/{}",
                            watch.invoice_id, confirmations, watch.required_confirmations
                        );

                        if confirmations >= watch.required_confirmations as u64 {
                            if current_balance >= state.target_balance {
                                println!("Invoice {}: Payment fully confirmed successfully!", watch.invoice_id);
                                completed_invoices.push(watch.invoice_id);
                            } else {
                                println!("Invoice {}: ⚠ Chain re-org detected! Resetting tracker.", watch.invoice_id);
                                state.detection_block = None;
                            }
                        }
                    }
                }

                // Automatically unregister fully confirmed payments
                for id in completed_invoices {
                    self.unregister_payment(id);
                }
            }

            sleep(Duration::from_secs(10)).await;
        }
    }
}