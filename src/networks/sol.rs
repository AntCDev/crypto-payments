use crate::services::watcher::CryptoWatcher;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use tokio::time::sleep;

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