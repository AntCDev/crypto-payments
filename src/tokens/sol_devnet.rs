use crate::networks::sol::SolanaNetwork;
use crate::networks::{NetworkClient, NetworkRegistry, SolanaCluster};
use crate::tokens::{decrypt_data, PaymentDetails, TokenHandler, TokenRegistry};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct TokenConfig {
    pub id: &'static str,
    pub name: &'static str,
    pub detail: &'static str,
    pub info: &'static str,
    pub token_address: Option<&'static str>, // None for native SOL
    pub decimals: u8,
    pub required_confirmations: i32,
}

// Token configuration list for Solana Devnet
pub const DEVNET_TOKENS: &[TokenConfig] = &[
    TokenConfig {
        id: "USDC_DEVNET",
        name: "USDC",
        detail: "(SOL) (Devnet)",
        info: "USDC stablecoin on the Solana Devnet.",
        token_address: Some("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),
        decimals: 6,
        required_confirmations: 5,
    },
    TokenConfig {
        id: "USDT_DEVNET",
        name: "USDT",
        detail: "(SOL) (Devnet)",
        info: "Tether USD stablecoin on the Solana Devnet.",
        token_address: Some("Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"),
        decimals: 6,
        required_confirmations: 5,
    },
    TokenConfig {
        id: "USDS_DEVNET",
        name: "USDS",
        detail: "(SOL) (Devnet)",
        info: "USDS stablecoin on the Solana Devnet.",
        token_address: Some("FILL_ME_IN_USDS_MINT_ADDRESS"),
        decimals: 6,
        required_confirmations: 5,
    },
    TokenConfig {
        id: "SOL_DEVNET",
        name: "SOL",
        detail: "(SOL) (Devnet)",
        info: "Native Solana coin on the Devnet.",
        token_address: None,
        decimals: 9,
        required_confirmations: 5,
    },
];

pub fn register(registry: &mut TokenRegistry, networks: Arc<NetworkRegistry>) {
    let network = match networks.sol_cluster(SolanaCluster::Devnet) {
        Some(net) => net,
        None => {
            println!("   ❌ Solana Devnet not configured");
            return;
        }
    };

    for config in DEVNET_TOKENS {
        let handler = DevnetHandler {
            network: Arc::clone(&network),
            config: config.clone(),
        };

        // Confirmation detail is stored in config but excluded from the registration call
        registry.register_token(
            config.id,
            config.name,
            config.detail,
            config.info,
            handler,
        );
    }
}

pub struct DevnetHandler {
    network: Arc<SolanaNetwork>,
    config: TokenConfig,
}

#[async_trait]
impl TokenHandler for DevnetHandler {
    fn token_id(&self) -> &str {
        self.config.id
    }

    async fn create_invoice_payment(
        &self,
        pool: &PgPool,
        merchant_id: Uuid,
        invoice_id: Uuid,
        _amount: rust_decimal::Decimal,
        _token_id: &str,
    ) -> Result<PaymentDetails, String> {
        // 1. Retrieve and parse MASTER_KEY from environment
        let master_key_hex = std::env::var("MASTER_KEY")
            .map_err(|_| "MASTER_KEY environment variable not set".to_string())?;

        let master_key_vec = hex::decode(&master_key_hex)
            .map_err(|e| format!("Failed to decode MASTER_KEY hex: {e}"))?;

        let master_key: &[u8; 32] = master_key_vec
            .as_slice()
            .try_into()
            .map_err(|_| "MASTER_KEY must be exactly 32 bytes (64 hex characters)".to_string())?;

        // 2. Fetch encrypted key material from the DB for this merchant
        let key_material = sqlx::query!(
            r#"
            SELECT encrypted_secret, encryption_nonce
            FROM merchant_key_material
            WHERE merchant_id = $1 AND key_family = 'bip39'
            "#,
            merchant_id
        )
            .fetch_one(pool)
            .await
            .map_err(|e| format!("Failed to fetch key material for merchant {merchant_id}: {e}"))?;

        // 3. Decrypt the merchant BIP39 mnemonic
        let decrypted_bytes = decrypt_data(
            master_key,
            &key_material.encrypted_secret,
            &key_material.encryption_nonce,
        )?;

        let merchant_mnemonic = String::from_utf8(decrypted_bytes)
            .map_err(|e| format!("Invalid UTF-8 sequence in decrypted mnemonic: {e}"))?;

        // 4. Derive wallet address and payment reference using decrypted mnemonic
        // Extract token address for DB assignment
        let token_address = self.config.token_address.as_ref().map(ToString::to_string);

        let (deposit_address, derived_wallet_index, payment_reference) = self
            .network
            .get_derive_address(pool, merchant_id, invoice_id, &merchant_mnemonic, token_address.as_deref())
            .await
            .map_err(|e| format!("Address derivation failed: {e}"))?;

        let expires_at = Utc::now() + Duration::minutes(30);


        // NOTE: adjust this to whatever SolanaNetwork exposes for the current
        // slot height (e.g. get_current_slot()) if the method name differs.
        let current_slot = self
            .network
            .get_current_block()
            .await
            .map_err(|e| format!("Failed to fetch current slot: {e}"))? as i64; // adjust type to match column

        let network_type = "solana";
        let chain_ref = "devnet";

        // 5. Update invoice record with derived address details and configuration metadata
        sqlx::query!(
        r#"
            UPDATE invoices
            SET wallet_address = $1,
                wallet_index = $2,
                expires_at = $3,
                payment_reference = $4,
                token_address = $5,
                token_decimals = $6,
                required_confirmations = $7,
                network_type = $8,
                chain_ref = $9,
                created_block = $10,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = $11
            "#,
            deposit_address,
            derived_wallet_index as i32,
            expires_at,
            payment_reference,
            token_address,
            self.config.decimals as i16,
            self.config.required_confirmations as i16,
            network_type,
            chain_ref,
            current_slot,
            invoice_id
        )
            .execute(pool)
            .await
            .map_err(|e| format!("DB update failed: {e}"))?;

        Ok(PaymentDetails {
            invoice_id,
            network: "devnet".to_string(),
            deposit_address,
            token_address,
            decimals: self.config.decimals,
            required_confirmations: self.config.required_confirmations,
            wallet_index: derived_wallet_index,
            expires_at,
        })
    }

    async fn cancel_payment(&self, _pool: &PgPool, invoice_id: Uuid) -> Result<(), String> {
        println!("DevnetHandler::cancel_payment({invoice_id}) for token: {}", self.config.id);
        Ok(())
    }
}