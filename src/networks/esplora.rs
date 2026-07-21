use super::{Amount, BitcoinNetwork, NetworkClient, PaymentWatch};
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
use bech32::{Hrp, segwit};
use sqlx::PgPool;

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
    api_urls: Vec<String>, // Upgraded to support redundant URLs
    network_name: String,   // Dynamically configured (e.g., "BTC_MAINNET")
    client: reqwest::Client,
    is_testnet: bool,
    pending: Mutex<HashMap<Uuid, PaymentWatch>>,
}

impl EsploraNetwork {
    /// Inherent constructor matching the initialization pattern in NetworkRegistry
    pub fn new(network: BitcoinNetwork, api_urls: Vec<String>) -> Self {
        assert!(!api_urls.is_empty(), "EsploraNetwork requires at least one API URL");

        // Dynamically compute the network name for DB tracking
        let network_name = format!("BTC_{:?}", network).to_uppercase();

        let is_testnet = match network {
            BitcoinNetwork::Mainnet => false,
            BitcoinNetwork::Testnet4 | BitcoinNetwork::Signet => true,
        };

        Self {
            api_urls,
            network_name,
            client: reqwest::Client::new(),
            is_testnet,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Internal helper that queries a single REST URL endpoint
    async fn request_api_single(&self, base_url: &str, path: &str) -> Result<String, String> {
        let full_url = format!("{}{}", base_url, path);
        let response = self.client
            .get(&full_url)
            .send()
            .await
            .map_err(|e| format!("HTTP request to {full_url} failed: {e}"))?;

        let text = response
            .text()
            .await
            .map_err(|e| format!("Failed to read body from {full_url}: {e}"))?;

        Ok(text)
    }

    /// Replicates your EVM quorum strategy: fans out to all Esplora REST endpoints
    /// and expects a minimum agreement threshold of 2 nodes.
    async fn request_api(&self, path: &str) -> Result<String, String> {
        if self.api_urls.len() == 1 {
            return self.request_api_single(&self.api_urls[0], path).await;
        }

        let futures = self.api_urls.iter().map(|url| self.request_api_single(url, path));
        let results: Vec<Result<String, String>> = futures::future::join_all(futures).await;
        let oks: Vec<&String> = results.iter().filter_map(|r| r.as_ref().ok()).collect();

        if oks.len() < 2 {
            let errs: Vec<&String> = results.iter().filter_map(|r| r.as_ref().err()).collect();
            return Err(format!(
                "Quorum failed for path {path} on network {}: only {}/{} endpoints responded. Errors: {:?}",
                self.network_name, oks.len(), self.api_urls.len(), errs
            ));
        }

        // Return the first value that at least 2 endpoints agree on
        for candidate in &oks {
            if oks.iter().filter(|v| *v == candidate).count() >= 2 {
                return Ok((*candidate).clone());
            }
        }

        Err(format!(
            "Quorum disagreement for path {path} on network {}: endpoints returned different values: {:?}",
            self.network_name, oks
        ))
    }

    /// Inherent helper method to derive a Native SegWit (Bech32) address for a given index.
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

        let public_key_point = public_key.to_encoded_point(true);
        let point_bytes = public_key_point.as_bytes();

        let mut sha256_hasher = Sha256::new();
        sha256_hasher.update(point_bytes);
        let sha256_result = sha256_hasher.finalize();

        let mut ripemd_hasher = Ripemd160::new();
        ripemd_hasher.update(&sha256_result);
        let hash160 = ripemd_hasher.finalize();

        let hrp_str = if self.is_testnet { "tb" } else { "bc" };
        let hrp = Hrp::parse(hrp_str)
            .map_err(|e| format!("Invalid HRP prefix: {}", e))?;

        let address = segwit::encode(hrp, segwit::VERSION_0, &hash160)
            .map_err(|e| format!("Bech32 encoding failed: {}", e))?;

        Ok(address)
    }
}
pub fn derive_reference_bytes(invoice_id: Uuid) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(invoice_id.as_bytes());
    hasher.finalize().into()
}
#[async_trait]
impl NetworkClient for EsploraNetwork {
    // --- WALLET METHODS ---
    async fn get_derive_address(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        mnemonic: &str,
    ) -> Result<(String, u32, Option<String>), String> {
        // Automatically tracks and increments the merchant's key index for account 0
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

        // Uses the newly generated index (row.next_index - 1 gives the zero-indexed key)
        let index = (row.next_index - 1) as u32;
        let address = self.derive_address(mnemonic, index)?;

        // Keeping tracking references uniform across your processing layer
        let reference_bytes = crate::networks::evm::derive_reference_bytes(invoice_id);
        let reference = format!("0x{}", hex::encode(reference_bytes));

        Ok((address, index, Some(reference)))
    }

    fn get_derivation_path(&self, index: u32) -> String {
        let coin_type = if self.is_testnet { 1 } else { 0 };
        format!("m/84'/{}'/0'/0/{}", coin_type, index)
    }

    fn validate_address(&self, address: &str) -> bool {
        match segwit::decode(address) {
            Ok((hrp, version, program)) => {
                let expected_hrp = if self.is_testnet { "tb" } else { "bc" };

                if hrp.as_str() != expected_hrp {
                    return false;
                }

                if version != segwit::VERSION_0 {
                    return false;
                }

                program.len() == 20
            }
            Err(_) => false,
        }
    }

    // --- CHAIN STATE METHODS ---
    async fn get_native_balance(&self, address: &str) -> Result<Amount, String> {
        let path = format!("/address/{}", address);
        let raw_json = self.request_api(&path).await?;

        let res: EsploraAddressResponse = serde_json::from_str(&raw_json)
            .map_err(|e| format!("Failed to parse Esplora balance payload: {}", e))?;

        let total_satoshis = (res.chain_stats.funded_txo_sum + res.mempool_stats.funded_txo_sum)
            .saturating_sub(res.chain_stats.spent_txo_sum + res.mempool_stats.spent_txo_sum);

        Ok(Amount(total_satoshis as u128))
    }

    async fn get_token_balance(
        &self,
        _token_address: &str,
        _address: &str,
        _decimals: u8,
    ) -> Result<Amount, String> {
        println!("⚠ Warning: Token balance query ignored. Esplora layer-1 architecture does not track custom tokens.");
        Ok(Amount(0))
    }

    async fn get_current_block(&self) -> Result<u64, String> {
        let raw_text = self.request_api("/blocks/tip/height").await?;

        raw_text.trim().parse::<u64>()
            .map_err(|_| "Failed to parse block height integer from quorum responses".to_string())
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
        println!("EsploraNetwork::watch_payments processing loop started with {} endpoints", self.api_urls.len());

        loop {
            let current_watches: Vec<PaymentWatch> = match self.pending.lock() {
                Ok(p) => p.values().cloned().collect(),
                Err(_) => return Err("Pending payments lock poisoned".to_string()),
            };

            if !current_watches.is_empty() {
                let mut completed_invoices = Vec::new();

                for watch in current_watches {
                    let target_satoshis = watch.target_amount;
                    let path = format!("/address/{}/txs", watch.address);

                    // Query the API using the engine's built-in multi-endpoint quorum structure
                    if let Ok(raw_json) = self.request_api(&path).await {
                        if let Ok(txs) = serde_json::from_str::<Vec<EsploraTx>>(&raw_json) {

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

                for id in completed_invoices {
                    self.unregister_payment(id);
                }
            }

            sleep(Duration::from_secs(30)).await;
        }
    }
}