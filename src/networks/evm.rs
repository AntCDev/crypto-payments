use crate::services::watcher::CryptoWatcher;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use tokio::time::sleep;

use crate::services::wallet::CryptoWalletManager;
use bip32::{DerivationPath, PrivateKey, XPrv};
use bip39::Mnemonic;
use sha3::{Digest, Keccak256};


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

// ### WATCHER ###
pub struct EvmWatcher {
    rpc_url: String,
    client: reqwest::Client,
    token_address: Option<String>, // None = Native ETH, Some = ERC-20 Token (USDC, etc.)
    decimals: u8,                  // 18 for ETH, 6 for USDC
}

impl EvmWatcher {
    pub fn new(rpc_url: String, token_address: Option<String>, decimals: u8) -> Self {
        Self {
            rpc_url,
            client: reqwest::Client::new(),
            token_address,
            decimals,
        }
    }

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
}

#[async_trait]
impl CryptoWatcher for EvmWatcher {
    async fn get_balance(&self, address: &str) -> Result<f64, String> {
        let hex_balance = if let Some(ref contract_addr) = self.token_address {
            let clean_addr = address.trim_start_matches("0x");
            let data = format!("0x70a08231{:0>64}", clean_addr); // Left-pad address to 32 bytes (64 hex chars)

            let params = json!([
                {
                    "to": contract_addr,
                    "data": data
                },
                "latest"
            ]);
            self.call_rpc("eth_call", params).await?
        } else {
            // --- Native ETH Balance Logic ---
            let params = json!([address, "latest"]);
            self.call_rpc("eth_getBalance", params).await?
        };

        let clean_hex = hex_balance.trim_start_matches("0x");

        if clean_hex.is_empty() {
            return Ok(0.0);
        }

        let raw_units = u128::from_str_radix(clean_hex, 16)
            .map_err(|_| "Failed to parse hex balance".to_string())?;

        let float_balance = raw_units as f64 / 10f64.powi(self.decimals as i32);
        Ok(float_balance)
    }

    async fn get_current_block(&self, _rpc_url: &str) -> Result<u64, String> {
        let hex_block = self.call_rpc("eth_blockNumber", json!([])).await?;
        let clean_hex = hex_block.trim_start_matches("0x");

        u64::from_str_radix(clean_hex, 16)
            .map_err(|_| "Failed to parse hex block number".to_string())
    }

    async fn watch_payment(
        &self,
        address: &str,
        target_amount: f64,
        required_confirmations: u64
    ) -> Result<(), String> {
        let asset_name = match &self.token_address {
            Some(_) => "Token",
            None => "ETH"
        };
        println!("Starting payment watch for {} for {} {}...", address, target_amount, asset_name);

        let initial_balance = self.get_balance(address).await?;
        let target_balance = initial_balance + target_amount;
        let mut detection_block: Option<u64> = None;

        loop {
            if detection_block.is_none() {
                let current_balance = self.get_balance(address).await?;
                if current_balance >= target_balance {
                    let current_block = self.get_current_block(&self.rpc_url).await?;
                    detection_block = Some(current_block);
                    println!("Payment detected at block {}! Waiting for {} confirmations...", current_block, required_confirmations);
                }
            }
            else if let Some(detected_at) = detection_block {
                let current_block = self.get_current_block(&self.rpc_url).await?;

                let confirmations = if current_block >= detected_at {
                    (current_block - detected_at) + 1
                } else {
                    0
                };

                println!("Progress: {}/{} confirmations", confirmations, required_confirmations);

                if confirmations >= required_confirmations {
                    let final_balance = self.get_balance(address).await?;
                    if final_balance >= target_balance {
                        println!("Payment fully confirmed!");
                        return Ok(());
                    } else {
                        println!("⚠ Re-org detected! Balance dropped. Resetting watch state.");
                        detection_block = None;
                    }
                }
            }

            sleep(Duration::from_secs(10)).await;
        }
    }
}


// ### WALLET ###
pub struct EvmWalletManager {
    _is_testnet: bool,
}

impl EvmWalletManager {
    pub fn new(is_testnet: bool) -> Self {
        Self { _is_testnet: is_testnet }
    }
}

#[async_trait]
impl CryptoWalletManager for EvmWalletManager {
    async fn derive_address(&self, mnemonic: &str, index: u32) -> Result<String, String> {
        // 1. Parse the mnemonic phrase
        let mnemonic_parsed = Mnemonic::parse(mnemonic)
            .map_err(|e| format!("Invalid mnemonic: {}", e))?;

        // 2. Generate the cryptographic seed directly as a 64-byte array
        let seed = mnemonic_parsed.to_seed("");

        // 3. Construct and parse the exact BIP44 derivation path
        let path_str = self.get_derivation_path(index)?;
        let path: DerivationPath = path_str
            .parse()
            .map_err(|e| format!("Failed to parse derivation path: {}", e))?;

        // 4. Directly derive the child extended private key from the seed and path
        let child_xprv = XPrv::derive_from_path(&seed, &path)
            .map_err(|e| format!("Failed to derive child key at path: {}", e))?;

        // 5. Extract the public key point from the derived private key
        let secret_key = child_xprv.private_key();
        let public_key = secret_key.public_key();

        // 6. Get the uncompressed SEC1 representation (65 bytes: 0x04 prefix + 32-byte X + 32-byte Y)
        let public_key_point = public_key.to_encoded_point(false);
        let point_bytes = public_key_point.as_bytes();

        // 7. EVM addresses are the last 20 bytes of the Keccak256 hash of the uncompressed 64-byte public key coordinate
        let mut hasher = Keccak256::new();
        hasher.update(&point_bytes[1..]); // Skip the 0x04 prefix byte
        let hash_result = hasher.finalize();

        // Take last 20 bytes
        let address_bytes = &hash_result[12..];

        Ok(format!("0x{}", hex::encode(address_bytes)))
    }

    fn get_derivation_path(&self, index: u32) -> Result<String, String> {
        Ok(format!("m/44'/60'/0'/0/{}", index))
    }

    fn validate_address(&self, address: &str) -> bool {
        let clean_addr = address.trim_start_matches("0x");

        if clean_addr.len() != 40 {
            return false;
        }

        clean_addr.chars().all(|c| c.is_ascii_hexdigit())
    }
}