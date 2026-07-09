use crate::services::watcher::CryptoWatcher;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use tokio::time::sleep;
use hmac::{Hmac, Mac};
use sha2::Sha512;
use ed25519_dalek::SigningKey;
use crate::services::wallet::CryptoWalletManager;

// Generic structures for Solana JSON-RPC
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

// Solana's getBalance returns an object containing context and the value
#[derive(Deserialize)]
struct SolBalanceValue {
    value: u64, // Lamports
}

// Used to grab the arbitrary nested array from getTokenAccountsByOwner
#[derive(Deserialize)]
struct SolTokenAccountsValue {
    value: Vec<serde_json::Value>,
}

// ### WATCHER ###

pub struct SolWatcher {
    rpc_url: String,
    client: reqwest::Client,
    token_address: Option<String>, // None = Native SOL, Some = SPL Token Mint Address (e.g. USDC)
    decimals: u8,                  // 9 for SOL, 6 for USDC
}

impl SolWatcher {
    pub fn new(rpc_url: String, token_address: Option<String>, decimals: u8) -> Self {
        Self {
            rpc_url,
            client: reqwest::Client::new(),
            token_address,
            decimals,
        }
    }

    // Generic RPC helper to handle different Solana response types cleanly
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
}

#[async_trait]
impl CryptoWatcher for SolWatcher {
    async fn get_balance(&self, address: &str) -> Result<f64, String> {
        if let Some(ref mint_address) = self.token_address {
            // --- SPL Token Balance Logic ---
            let params = json!([
                address,
                { "mint": mint_address },
                { "encoding": "jsonParsed" }
            ]);

            let response: SolTokenAccountsValue = self.call_rpc("getTokenAccountsByOwner", params).await?;

            // If the user has never held or initialized this token account, the array is empty -> balance is 0.0
            if response.value.is_empty() {
                return Ok(0.0);
            }

            // Safely traverse Solana's deeply nested jsonParsed layout
            let amount_str = response.value[0]
                .get("account")
                .and_then(|a| a.get("data"))
                .and_then(|d| d.get("parsed"))
                .and_then(|p| p.get("info"))
                .and_then(|i| i.get("tokenAmount"))
                .and_then(|t| t.get("amount"))
                .and_then(|amt| amt.as_str())
                .ok_or_else(|| "Failed to navigate token balance fields in RPC response".to_string())?;

            // SPL token amounts are base-10 strings
            let raw_units = amount_str.parse::<u128>()
                .map_err(|_| "Failed to parse token balance integer".to_string())?;

            let token_balance = raw_units as f64 / 10f64.powi(self.decimals as i32);
            Ok(token_balance)
        } else {
            // --- Native SOL Balance Logic ---
            let params = json!([address]);
            let balance_info: SolBalanceValue = self.call_rpc("getBalance", params).await?;

            // 1 SOL = 10^9 Lamports
            let sol_balance = balance_info.value as f64 / 10f64.powi(self.decimals as i32);
            Ok(sol_balance)
        }
    }

    async fn get_current_block(&self, _rpc_url: &str) -> Result<u64, String> {
        let block_height: u64 = self.call_rpc("getBlockHeight", json!([])).await?;
        Ok(block_height)
    }

    async fn watch_payment(
        &self,
        address: &str,
        target_amount: f64,
        required_confirmations: u64,
    ) -> Result<(), String> {
        let asset_name = match &self.token_address {
            Some(_) => "Token",
            None => "SOL",
        };
        println!("Starting Solana payment watch for {} for {} {}...", address, target_amount, asset_name);

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
                        println!("Solana payment fully confirmed!");
                        return Ok(());
                    } else {
                        println!("⚠ Fork detected! Balance dropped below threshold. Resetting watch state.");
                        detection_block = None;
                    }
                }
            }

            sleep(Duration::from_secs(2)).await;
        }
    }
}

// ### WALLET ###
pub struct SolWalletManager {
    _is_testnet: bool,
}

impl SolWalletManager {
    pub fn new(is_testnet: bool) -> Self {
        // Note: Solana uses the exact same address format for mainnet, testnet, and devnet.
        Self { _is_testnet: is_testnet }
    }
}

#[async_trait]
impl CryptoWalletManager for SolWalletManager {
    async fn derive_address(&self, mnemonic: &str, index: u32) -> Result<String, String> {
        // 1. Parse the mnemonic phrase using BIP39
        let mnemonic_parsed = bip39::Mnemonic::parse(mnemonic)
            .map_err(|e| format!("Invalid mnemonic: {}", e))?;

        // 2. Generate the 64-byte cryptographic seed
        let seed = mnemonic_parsed.to_seed("");

        // 3. Construct and parse the derivation path into raw indices
        let path_str = self.get_derivation_path(index)?;
        let indices = parse_derivation_path(&path_str)?;

        // 4. Derive Master Key using the SLIP-0010 master formula for ed25519
        type HmacSha512 = Hmac<Sha512>;
        let mut mac = HmacSha512::new_from_slice(b"ed25519 seed")
            .map_err(|e| format!("HMAC initialization failed: {}", e))?;
        mac.update(&seed);
        let hmac_result = mac.finalize().into_bytes();

        let mut secret_key: [u8; 32] = hmac_result[0..32].try_into().unwrap();
        let mut chain_code: [u8; 32] = hmac_result[32..64].try_into().unwrap();

        // 5. Iterate through path indices following SLIP-0010 child key derivation rules
        for index in indices {
            if index < 0x8000_0000 {
                return Err("SLIP-0010 Ed25519 only supports hardened derivation paths".to_string());
            }

            let mut mac = HmacSha512::new_from_slice(&chain_code)
                .map_err(|e| format!("HMAC initialization failed: {}", e))?;

            mac.update(&[0x00]);
            mac.update(&secret_key);
            mac.update(&index.to_be_bytes());

            let result = mac.finalize().into_bytes();
            secret_key.copy_from_slice(&result[0..32]);
            chain_code.copy_from_slice(&result[32..64]);
        }

        // 6. Generate the Ed25519 Public Key from the final derived secret key scalar
        let signing_key = SigningKey::from_bytes(&secret_key);
        let verifying_key = signing_key.verifying_key();

        // 7. Solana addresses are simply the Base58 representation of the raw 32-byte public key point
        Ok(bs58::encode(verifying_key.to_bytes()).into_string())
    }

    fn get_derivation_path(&self, index: u32) -> Result<String, String> {
        // Modern Solana wallets (Phantom, Solflare) group multi-accounts at the third index.
        // Format: m/44'/501'/{index}'/0'
        Ok(format!("m/44'/501'/{}'/0'", index))
    }

    fn validate_address(&self, address: &str) -> bool {
        // Standard Solana addresses are between 32 and 44 characters due to base58 variable length
        if address.len() < 32 || address.len() > 44 {
            return false;
        }

        // Validating means checking if it decodes cleanly into exactly 32 bytes
        match bs58::decode(address).into_vec() {
            Ok(bytes) => bytes.len() == 32,
            Err(_) => false,
        }
    }
}

/// Helper function to parse a BIP44/SLIP10 derivation path string into raw u32 bits
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
            indices.push(val | 0x8000_0000); // Apply bitmask for hardened index flag
        } else {
            indices.push(val);
        }
    }
    Ok(indices)
}