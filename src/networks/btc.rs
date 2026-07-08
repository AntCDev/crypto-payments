use crate::services::watcher::CryptoWatcher;
use async_trait::async_trait;
use serde::Deserialize;
use std::time::Duration;
use tokio::time::sleep;

// Esplora address endpoint returns this clean structure
#[derive(Deserialize)]
struct EsploraAddressStats {
    funded_txo_sum: u64,
    spent_txo_sum: u64,
}

#[derive(Deserialize)]
struct EsploraAddressResponse {
    chain_stats: EsploraAddressStats,
    mempool_stats: EsploraAddressStats,
}

pub struct BtcWatcher {
    api_url: String, // e.g., "https://mempool.space/api" or your local Esplora instance
    client: reqwest::Client,
    token_address: Option<String>, // Kept for interface symmetry (warns if used)
    decimals: u8,                  // Defaults to 8 for BTC (Satoshis)
}

impl BtcWatcher {
    pub fn new(api_url: String, token_address: Option<String>, decimals: u8) -> Self {
        Self {
            api_url,
            client: reqwest::Client::new(),
            token_address,
            decimals,
        }
    }
}

#[async_trait]
impl CryptoWatcher for BtcWatcher {
    async fn get_balance(&self, address: &str) -> Result<f64, String> {
        if self.token_address.is_some() {
            println!("⚠ Warning: Token address was provided for BTC, but layer-1 Esplora only tracks native BTC.");
        }

        let url = format!("{}/address/{}", self.api_url, address);

        let res: EsploraAddressResponse = self.client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("Esplora request failed: {}", e))?
            .json()
            .await
            .map_err(|e| format!("Failed to parse Esplora response: {}", e))?;

        // Balance = (Confirmed Received + Unconfirmed Received) - Total Spent
        let total_satoshis = (res.chain_stats.funded_txo_sum + res.mempool_stats.funded_txo_sum)
            .saturating_sub(res.chain_stats.spent_txo_sum + res.mempool_stats.spent_txo_sum);

        // Dynamically scale using the configured asset decimals (10^8 for native BTC)
        let btc_balance = total_satoshis as f64 / 10f64.powi(self.decimals as i32);
        Ok(btc_balance)
    }

    async fn get_current_block(&self, _rpc_url: &str) -> Result<u64, String> {
        let url = format!("{}/blocks/tip/height", self.api_url);

        let text_height = self.client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("Esplora height request failed: {}", e))?
            .text()
            .await
            .map_err(|e| format!("Failed to read height text: {}", e))?;

        text_height.trim().parse::<u64>()
            .map_err(|_| "Failed to parse block height integer".to_string())
    }

    async fn watch_payment(
        &self,
        address: &str,
        target_amount: f64,
        required_confirmations: u64
    ) -> Result<(), String> {
        println!("Starting payment watch for {} for {} BTC...", address, target_amount);

        let initial_balance = self.get_balance(address).await?;
        let target_balance = initial_balance + target_amount;
        let mut detection_block: Option<u64> = None;

        loop {
            if detection_block.is_none() {
                let current_balance = self.get_balance(address).await?;
                if current_balance >= target_balance {
                    let current_block = self.get_current_block(&self.api_url).await?;
                    detection_block = Some(current_block);
                    println!("Payment detected at block {}! Waiting for confirmations...", current_block);
                }
            } else if let Some(detected_at) = detection_block {
                let current_block = self.get_current_block(&self.api_url).await?;

                let confirmations = if current_block >= detected_at {
                    (current_block - detected_at) + 1
                } else {
                    0
                };

                println!("Progress: {}/{} confirmations", confirmations, required_confirmations);

                if confirmations >= required_confirmations {
                    let final_balance = self.get_balance(address).await?;
                    if final_balance >= target_balance {
                        println!("BTC Payment fully confirmed!");
                        return Ok(());
                    } else {
                        println!("⚠ Re-org detected! Resetting watch state.");
                        detection_block = None;
                    }
                }
            }

            // Polling every 30 seconds is standard for BTC due to ~10 minute block times
            sleep(Duration::from_secs(30)).await;
        }
    }
}