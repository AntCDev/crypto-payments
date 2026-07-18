use crate::networks::evm::EVMNetwork;
use crate::tokens::{PaymentDetails, TokenHandler, TokenRegistry};
use async_trait::async_trait;
use std::sync::Arc;
use sqlx::PgPool;
use uuid::Uuid;
use chrono::{Utc, Duration};
use crate::networks::{NetworkClient, NetworkRegistry};

pub struct EthHandler {
    network: Arc<EVMNetwork>,
}

#[async_trait]
impl TokenHandler for EthHandler {
    fn token_id(&self) -> &str {
        "USDC_ETH"
    }

    async fn create_invoice_payment(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        amount: rust_decimal::Decimal,
    ) -> Result<PaymentDetails, String> {
        println!("EthHandler::create_invoice_payment(invoice: {invoice_id}, amount: {amount})");

        // -------------------------------------------------------------------------
        // WHAT HAPPENS INSIDE THIS HANDLER:
        // -------------------------------------------------------------------------
        // 1. QUERY & ACQUIRE INDEX:
        //    Fetch/reserve the next BIP-44 public derivation index for Mainnet Ethereum.
        let derived_wallet_index = 108;

        // 2. DERIVE DEPOSIT ADDRESS:
        //    Derive the public address on Ethereum path m/44'/60'/0'/0/108.
        let deposit_address = "0xa0b8...eth_derived_address".to_string();

        // 3. DEFINE EXPIRATION:
        //    Mainnet transactions can take longer; set a 60-minute payment window.
        let expires_at = Utc::now() + Duration::minutes(60);

        // 4. UPDATE THE DATABASE RECORD:
        //    Apply the `deposit_address`, `derived_wallet_index`, and `expires_at`
        //    directly into the DB row identified by `invoice_id`.
        //
        // 5. REGISTER CHAIN WATCHER:
        //    Register a watch request with the EVM background block listener.
        // -------------------------------------------------------------------------

        Ok(PaymentDetails {
            invoice_id,
            network: "ethereum".to_string(),
            deposit_address,
            token_address: Some("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48".to_string()), // Real Ethereum USDC contract
            decimals: 6,
            required_confirmations: 12, // Mainnet requires higher confirmations
            wallet_index: derived_wallet_index,
            expires_at,
        })
    }

    async fn cancel_payment(&self, _pool: &PgPool, invoice_id: Uuid) -> Result<(), String> {
        println!("EthHandler::cancel_payment({invoice_id})");
        // -------------------------------------------------------------------------
        // WHAT HAPPENS HERE:
        // 1. De-register the active EVM transaction watcher for the target address.
        // 2. Clear out any pending locks or state.
        // -------------------------------------------------------------------------
        Ok(())
    }
}

pub fn register(registry: &mut TokenRegistry, networks: Arc<NetworkRegistry>) {
    let handler = EthHandler {
        network: networks.evm.clone(),
    };

    registry.register_token(
        "USDC_ETH",
        "USDC",
        "Eth.",
        "USDC on the mainnet built using crates.",
        handler,
    );
}