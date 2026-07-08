pub mod eth;
pub mod sol;
pub mod btc;

use crate::services::watcher::CryptoWatcher;
use eth::EthWatcher;
use sol::SolWatcher;
use btc::BtcWatcher;

// This is your Factory pattern to generate the correct watcher at runtime
// Updated factory to accept the rpc_url
pub fn watch_factory(
    network: &str,
    rpc_url: &str,
    token_address: Option<&str>,
    decimals: u8,
) -> Result<Box<dyn CryptoWatcher>, String> {
    match network.to_lowercase().as_str() {
        "ethereum" | "eth" => {
            Ok(Box::new(EthWatcher::new(
                rpc_url.to_string(),
                token_address.map(|s| s.to_string()),
                decimals,
            )))
        }
        "solana" | "sol" => {
            Ok(Box::new(SolWatcher::new(
                rpc_url.to_string(),
                token_address.map(|s| s.to_string()),
                decimals,
            )))
        }
        "bitcoin" | "btc" => {
            // Passes the Esplora/Mempool API URL instead of a traditional node RPC URL
            Ok(Box::new(BtcWatcher::new(
                rpc_url.to_string(),
                token_address.map(|s| s.to_string()),
                decimals,
            )))
        }
        _ => Err(format!("Unsupported network: {}", network)),
    }
}