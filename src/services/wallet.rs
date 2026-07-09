use async_trait::async_trait;

#[async_trait]
pub trait CryptoWalletManager: Send + Sync {
    // Derives a public address from a mnemonic and a specific derivation index
    async fn derive_address(&self, mnemonic: &str, index: u32) -> Result<String, String>;

    // Returns the standard derivation path string used for a given index (e.g., "m/44'/60'/0'/0/0")
    fn get_derivation_path(&self, index: u32) -> Result<String, String>;

    // Validates whether a given address string is structurally valid for the target network
    fn validate_address(&self, address: &str) -> bool;
}