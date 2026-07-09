pub mod evm;
pub mod sol;
pub mod esplora;

use crate::services::watcher::CryptoWatcher;
use crate::services::wallet::CryptoWalletManager;

use evm::{EvmWatcher, EvmWalletManager};
use sol::{SolWatcher, SolWalletManager};
use esplora::{EsploraWatcher, EsploraWalletManager};

pub fn watch_factory(
    network: &str,
    rpc_url: &str,
    token_address: Option<&str>,
    decimals: u8,
) -> Result<Box<dyn CryptoWatcher>, String> {
    match network.to_lowercase().as_str() {
        "ethereum" | "eth" => Ok(Box::new(EvmWatcher::new(rpc_url.to_string(), token_address.map(|s| s.to_string()), decimals))),
        "solana" | "sol" => Ok(Box::new(SolWatcher::new(rpc_url.to_string(), token_address.map(|s| s.to_string()), decimals))),
        "bitcoin" | "btc" => Ok(Box::new(EsploraWatcher::new(rpc_url.to_string(), token_address.map(|s| s.to_string()), decimals))),
        _ => Err(format!("Unsupported network for watcher: {}", network)),
    }
}

pub fn wallet_factory(
    network: &str,
    is_testnet: bool,
) -> Result<Box<dyn CryptoWalletManager>, String> {
    match network.to_lowercase().as_str() {
        "ethereum" | "eth" => {
            Ok(Box::new(EvmWalletManager::new(is_testnet)))
        }
        "solana" | "sol" => {
            Ok(Box::new(SolWalletManager::new(is_testnet)))
        }
        "bitcoin" | "btc" => {
            Ok(Box::new(EsploraWalletManager::new(is_testnet)))
        }
        _ => Err(format!("Unsupported network for wallet management: {}", network)),
    }
}