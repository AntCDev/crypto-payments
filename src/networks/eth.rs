use crate::services::watcher::CryptoWatcher;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::time::Duration;
use tokio::time::sleep;

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

pub struct EthWatcher {
    rpc_url: String,
    client: reqwest::Client,
    token_address: Option<String>, // None = Native ETH, Some = ERC-20 Token (USDC, etc.)
    decimals: u8,                  // 18 for ETH, 6 for USDC
}

impl EthWatcher {
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
impl CryptoWatcher for EthWatcher {
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