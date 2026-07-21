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
use sha2::Sha256;
use sha3::{Digest, Keccak256};
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
    chain_id: u64,
    pub network_name: String, // Added field
    rpc_urls: Vec<String>,
    client: reqwest::Client,
    pending: Mutex<HashMap<Uuid, PaymentWatch>>,
}

impl EVMNetwork {
    pub fn new(chain_id: u64, rpc_urls: Vec<String>) -> Self {
        assert!(!rpc_urls.is_empty(), "EVMNetwork requires at least one RPC URL");
        let network_name = format!("EVM_{}", chain_id);

        Self {
            chain_id,
            network_name,
            rpc_urls,
            client: reqwest::Client::new(),
            pending: Mutex::new(HashMap::new()),
        }
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
}

pub fn derive_reference_bytes(invoice_id: Uuid) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(invoice_id.as_bytes());
    hasher.finalize().into()
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
            self.network_name // Dynamically uses "EVM_1", "EVM_8453", etc.
        )
            .fetch_one(pool)
            .await
            .map_err(|e| format!("Failed to update merchant network index: {}", e))?;

        let index = (row.next_index - 1) as u32;
        let address = self.derive_address(mnemonic, index)?;

        // bytes32 reference for contract call param / indexed log topic
        let reference_bytes = derive_reference_bytes(invoice_id);
        let reference = format!("0x{}", hex::encode(reference_bytes));

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

    async fn watch_payments(&self) -> Result<(), String> {
        println!(
            "EVMNetwork::watch_payments processing loop started for {} (Endpoints configured: {})",
            self.network_name,
            self.rpc_urls.len()
        );

        // Persistent tracking state scoped to the worker loop lifecycle
        struct TrackingState {
            target_balance: u128,
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
                            target_balance: initial_balance + watch.target_amount,
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

            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    }
}