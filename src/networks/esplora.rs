use crate::services::watcher::CryptoWatcher;
use async_trait::async_trait;
use serde::Deserialize;
use bip39::Mnemonic;
use bip32::{XPrv, DerivationPath, PrivateKey};
use sha2::{Sha256, Digest};
use ripemd::Ripemd160;
use bech32::{ToBase32, Variant, u5};
use crate::services::wallet::CryptoWalletManager;

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

#[derive(serde::Deserialize, Debug, Clone)]
pub struct EsploraTxStatus {
    pub confirmed: bool,
    #[serde(rename = "block_height")]
    pub block_height: Option<u64>,
}

#[derive(serde::Deserialize, Debug, Clone)]
pub struct EsploraTx {
    pub txid: String,
    pub vout: Vec<EsploraVout>,
    pub status: EsploraTxStatus,
}

#[derive(serde::Deserialize, Debug, Clone)]
pub struct EsploraVout {
    pub value: u64, // Satoshis
    pub scriptpubkey_address: Option<String>,
}

// ### WATCHER ###

pub struct EsploraWatcher {
    api_url: String, // e.g., "https://mempool.space/api" or your local Esplora instance
    client: reqwest::Client,
    token_address: Option<String>, // Kept for interface symmetry (warns if used)
    decimals: u8,                  // Defaults to 8 for BTC (Satoshis)
}

impl EsploraWatcher {
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
impl CryptoWatcher for EsploraWatcher {
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
        required_confirmations: u64,
    ) -> Result<(), String> {
        // Convert target BTC to Satoshis to avoid floating-point errors during validation
        let target_satoshis = (target_amount * 10f64.powi(self.decimals as i32)) as u64;
        println!("Watching address {} for a payment of {} satoshis...", address, target_satoshis);

        loop {
            // 1. Fetch recent transactions for this specific address
            let txs_url = format!("{}/address/{}/txs", self.api_url, address);
            let res = self.client.get(&txs_url).send().await;

            if let Ok(response) = res {
                if let Ok(txs) = response.json::<Vec<EsploraTx>>().await {

                    // 2. Look for a transaction output matching our address and amount
                    let matching_tx = txs.iter().find(|tx| {
                        tx.vout.iter().any(|out| {
                            out.scriptpubkey_address.as_deref() == Some(address)
                                && out.value >= target_satoshis
                        })
                    });

                    if let Some(tx) = matching_tx {
                        // 3. Evaluate confirmation depth based on actual block inclusion
                        if !tx.status.confirmed {
                            println!("Payment found in mempool! Status: 0/{} confirmations.", required_confirmations);
                        } else if let Some(tx_height) = tx.status.block_height {
                            // Get the latest block tip
                            if let Ok(current_tip) = self.get_current_block(&self.api_url).await {
                                let confirmations = if current_tip >= tx_height {
                                    (current_tip - tx_height) + 1
                                } else {
                                    0
                                };

                                println!("Payment Progress: {}/{} confirmations (Mined in block {})",
                                         confirmations, required_confirmations, tx_height);

                                if confirmations >= required_confirmations {
                                    println!("🎉 BTC Payment successfully secured and confirmed!");
                                    return Ok(());
                                }
                            }
                        }
                    } else {
                        println!("Waiting for payment matching target amount...");
                    }
                }
            }

            // Poll every 30 seconds (optimal for Bitcoin's 10-minute block interval)
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    }
}


// ### WALLET ###
pub struct EsploraWalletManager {
    is_testnet: bool,
}

impl EsploraWalletManager {
    pub fn new(is_testnet: bool) -> Self {
        Self { is_testnet }
    }
}

#[async_trait]
impl CryptoWalletManager for EsploraWalletManager {
    async fn derive_address(&self, mnemonic: &str, index: u32) -> Result<String, String> {
        // 1. Parse the mnemonic phrase
        let mnemonic_parsed = Mnemonic::parse(mnemonic)
            .map_err(|e| format!("Invalid mnemonic: {}", e))?;

        // 2. Generate the cryptographic seed directly
        let seed = mnemonic_parsed.to_seed("");

        // 3. Construct and parse the exact BIP84 derivation path
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

        // 6. Get the COMPRESSED SEC1 representation (33 bytes: 0x02 or 0x03 prefix + 32-byte X coordinate)
        // SegWit addresses strictly require compressed public keys
        let public_key_point = public_key.to_encoded_point(true);
        let point_bytes = public_key_point.as_bytes();

        // 7. Compute HASH160: RIPEMD160(SHA256(public_key_bytes))
        let mut sha256_hasher = Sha256::new();
        sha256_hasher.update(point_bytes);
        let sha256_result = sha256_hasher.finalize();

        let mut ripemd_hasher = Ripemd160::new();
        ripemd_hasher.update(&sha256_result);
        let hash160 = ripemd_hasher.finalize(); // This 20-byte output is our witness program

        // 8. Construct the Bech32 Payload
        let hrp = if self.is_testnet { "tb" } else { "bc" };

        // Native SegWit v0 requires prepending the witness version (0) as a 5-bit base32 element
        let mut base32_data = vec![
            u5::try_from_u8(0).map_err(|e| format!("Invalid witness version element: {:?}", e))?
        ];
        // Convert our 20-byte hash160 buffer into 5-bit chunks and append
        base32_data.extend_from_slice(&hash160.to_base32());

        // 9. Encode to Bech32 string format
        let address = bech32::encode(hrp, base32_data, Variant::Bech32)
            .map_err(|e| format!("Bech32 encoding failed: {}", e))?;

        Ok(address)
    }

    fn get_derivation_path(&self, index: u32) -> Result<String, String> {
        // BIP84 standard path: m / 84' / coin_type' / account' / change / address_index
        // Mainnet coin_type = 0', Testnet coin_type = 1'
        let coin_type = if self.is_testnet { 1 } else { 0 };
        Ok(format!("m/84'/{}'/0'/0/{}", coin_type, index))
    }

    fn validate_address(&self, address: &str) -> bool {
        match bech32::decode(address) {
            Ok((hrp, data, variant)) => {
                let expected_hrp = if self.is_testnet { "tb" } else { "bc" };

                // Structural rules check
                if hrp != expected_hrp || variant != Variant::Bech32 {
                    return false;
                }
                if data.is_empty() || data[0] != u5::try_from_u8(0).unwrap() {
                    return false;
                }

                // 1 witness version element + 32 base32 elements (derived from the 20-byte payload) = 33 elements total
                data.len() == 33
            }
            Err(_) => false,
        }
    }
}