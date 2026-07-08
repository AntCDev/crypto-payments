use async_trait::async_trait;

#[async_trait]
pub trait CryptoWatcher: Send + Sync {
    // Fetches the current balance (automatically scales based on asset decimals)
    async fn get_balance(&self, address: &str) -> Result<f64, String>;

    // Fetches the current network block height
    async fn get_current_block(&self, rpc_url: &str) -> Result<u64, String>;

    // Actively polls to detect an invoice payment and waits for confirmations
    async fn watch_payment(
        &self,
        address: &str,
        target_amount: f64,
        required_confirmations: u64
    ) -> Result<(), String>;
}