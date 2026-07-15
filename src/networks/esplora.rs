use super::{Amount, NetworkClient, PaymentWatch};
use async_trait::async_trait;
use uuid::Uuid;

use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;
use tokio::time::sleep;

use bip32::{DerivationPath, PrivateKey, XPrv};
use bip39::Mnemonic;
use sha2::{Digest, Sha256};
use ripemd::Ripemd160;
use bech32::{u5, ToBase32, Variant};

// ==========================================
// ### PRIVATE ESPLORA DATA STRUCTURES ###
// ==========================================
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

#[derive(Deserialize, Debug, Clone)]
pub struct EsploraTxStatus {
    pub confirmed: bool,
    #[serde(rename = "block_height")]
    pub block_height: Option<u64>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct EsploraTx {
    pub txid: String,
    pub vout: Vec<EsploraVout>,
    pub status: EsploraTxStatus,
}

#[derive(Deserialize, Debug, Clone)]
pub struct EsploraVout {
    pub value: u64, // Satoshis
    pub scriptpubkey_address: Option<String>,
}

// ==========================================
// ### NETWORK IMPLEMENTATION ###
// ==========================================
pub struct EsploraNetwork {
    api_url: String,
    client: reqwest::Client,
    is_testnet: bool,
    pending: Mutex<HashMap<Uuid, PaymentWatch>>,
}

#[async_trait]
impl NetworkClient for EsploraNetwork {
    fn new(rpc_url: &str) -> Self {
        // Automatically deduce the network type context based on standard endpoint keywords
        let url_lower = rpc_url.to_lowercase();
        let is_testnet = url_lower.contains("testnet") || url_lower.contains("signet") || url_lower.contains("tb");

        Self {
            api_url: rpc_url.to_string(),
            client: reqwest::Client::new(),
            is_testnet,
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

        let public_key_point = public_key.to_encoded_point(true);
        let point_bytes = public_key_point.as_bytes();

        let mut sha256_hasher = Sha256::new();
        sha256_hasher.update(point_bytes);
        let sha256_result = sha256_hasher.finalize();

        let mut ripemd_hasher = Ripemd160::new();
        ripemd_hasher.update(&sha256_result);
        let hash160 = ripemd_hasher.finalize();

        let hrp = if self.is_testnet { "tb" } else { "bc" };

        let mut base32_data = vec![
            u5::try_from_u8(0).map_err(|e| format!("Invalid witness version element: {:?}", e))?
        ];
        base32_data.extend_from_slice(&hash160.to_base32());

        let address = bech32::encode(hrp, base32_data, Variant::Bech32)
            .map_err(|e| format!("Bech32 encoding failed: {}", e))?;

        Ok(address)
    }

    fn get_derivation_path(&self, index: u32) -> String {
        let coin_type = if self.is_testnet { 1 } else { 0 };
        format!("m/84'/{}'/0'/0/{}", coin_type, index)
    }

    fn validate_address(&self, address: &str) -> bool {
        match bech32::decode(address) {
            Ok((hrp, data, variant)) => {
                let expected_hrp = if self.is_testnet { "tb" } else { "bc" };

                if hrp != expected_hrp || variant != Variant::Bech32 {
                    return false;
                }
                if data.is_empty() || data[0] != u5::try_from_u8(0).unwrap() {
                    return false;
                }

                data.len() == 33
            }
            Err(_) => false,
        }
    }

    // --- CHAIN STATE METHODS ---

    async fn get_native_balance(&self, address: &str) -> Result<Amount, String> {
        let url = format!("{}/address/{}", self.api_url, address);

        let res: EsploraAddressResponse = self.client
            .get(&url)
            .send()
            .await
            .map_err(|e| format!("Esplora request failed: {}", e))?
            .json()
            .await
            .map_err(|e| format!("Failed to parse Esplora response: {}", e))?;

        let total_satoshis = (res.chain_stats.funded_txo_sum + res.mempool_stats.funded_txo_sum)
            .saturating_sub(res.chain_stats.spent_txo_sum + res.mempool_stats.spent_txo_sum);

        // Native Bitcoin uses 8 decimals; Esplora already returns values in atomic Satoshis.
        // We can cast directly to u128 without float conversions.
        Ok(Amount(total_satoshis as u128))
    }

    async fn get_token_balance(
        &self,
        _token_address: &str,
        _address: &str,
        _decimals: u8,
    ) -> Result<Amount, String> {
        // Esplora APIs focus exclusively on Layer-1 Bitcoin UTXOs
        println!("⚠ Warning: Token balance query ignored. Esplora layer-1 architecture does not track custom tokens.");
        Ok(Amount(0))
    }

    async fn get_current_block(&self) -> Result<u64, String> {
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

    // --- BATCHED WATCHING METHODS ---

    fn register_payment(&self, watch: PaymentWatch) {
        if let Ok(mut pending) = self.pending.lock() {
            println!("EsploraNetwork::register_payment for invoice: {}", watch.invoice_id);
            pending.insert(watch.invoice_id, watch);
        }
    }

    fn unregister_payment(&self, invoice_id: Uuid) {
        if let Ok(mut pending) = self.pending.lock() {
            println!("EsploraNetwork::unregister_payment for invoice: {}", invoice_id);
            pending.remove(&invoice_id);
        }
    }

    async fn watch_payments(&self) -> Result<(), String> {
        println!("EsploraNetwork::watch_payments processing loop started on {}", self.api_url);

        loop {
            // Take a short-lived lock snapshot to isolate thread access
            let current_watches: Vec<PaymentWatch> = match self.pending.lock() {
                Ok(p) => p.values().cloned().collect(),
                Err(_) => return Err("Pending payments lock poisoned".to_string()),
            };

            if !current_watches.is_empty() {
                let mut completed_invoices = Vec::new();

                for watch in current_watches {
                    // target_amount is already stored in absolute Satoshis (u128)
                    let target_satoshis = watch.target_amount;
                    let txs_url = format!("{}/address/{}/txs", self.api_url, watch.address);

                    let res = self.client.get(&txs_url).send().await;
                    if let Ok(response) = res {
                        if let Ok(txs) = response.json::<Vec<EsploraTx>>().await {

                            // Scan UTXO vector for matching destination criteria
                            let matching_tx = txs.iter().find(|tx| {
                                tx.vout.iter().any(|out| {
                                    out.scriptpubkey_address.as_deref() == Some(&watch.address)
                                        && (out.value as u128) >= target_satoshis
                                })
                            });

                            if let Some(tx) = matching_tx {
                                if !tx.status.confirmed {
                                    println!(
                                        "Invoice {}: Payment located in mempool! 0/{} confirmations.",
                                        watch.invoice_id, watch.required_confirmations
                                    );
                                } else if let Some(tx_height) = tx.status.block_height {
                                    if let Ok(current_tip) = self.get_current_block().await {
                                        let confirmations = if current_tip >= tx_height {
                                            (current_tip - tx_height) + 1
                                        } else {
                                            0
                                        };

                                        println!(
                                            "Invoice {}: Confirmation progress: {}/{} (Mined at block {})",
                                            watch.invoice_id, confirmations, watch.required_confirmations, tx_height
                                        );

                                        if confirmations >= watch.required_confirmations as u64 {
                                            println!("Invoice {}: BTC Payment successfully confirmed!", watch.invoice_id);
                                            completed_invoices.push(watch.invoice_id);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Clean up entries that achieved total settlement
                for id in completed_invoices {
                    self.unregister_payment(id);
                }
            }

            // Polling frequency adapted for Bitcoin block intervals
            sleep(Duration::from_secs(30)).await;
        }
    }
}