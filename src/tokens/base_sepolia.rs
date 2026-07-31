use crate::networks::evm::EVMNetwork;
use crate::networks::{NetworkClient, NetworkRegistry};
use crate::tokens::{decrypt_data, PaymentDetails, TokenHandler, TokenRegistry};
use async_trait::async_trait;
use std::sync::Arc;
use sqlx::PgPool;
use uuid::Uuid;
use chrono::{Utc, Duration};

pub fn register(registry: &mut TokenRegistry, networks: Arc<NetworkRegistry>) {
    let network = match networks.evm_chain(84532) {
        Some(net) => net,
        None => {
            println!("  ❌ USDC_BASE_SEPOLIA - USD Coin (Base) - Skipped (Base network / chain_id 84532 not configured)");
            return;
        }
    };

    let handler = BaseSepoliaHandler { network };

    registry.register_token(
        "USDC_BASE_SEPOLIA",
        "USD Coin (Base) (Testnet)",
        "USDC stablecoin hosted natively on the Base Layer-2 and testing network.",
        "Requires 5 network confirmations.",
        handler,
    );
}

pub struct BaseSepoliaHandler {
    network: Arc<EVMNetwork>,
}

#[async_trait]
impl TokenHandler for BaseSepoliaHandler {
    fn token_id(&self) -> &str { "USDC_BASE" }

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
        let (deposit_address, derived_wallet_index, payment_reference) = self
            .network
            .get_derive_address(pool, merchant_id, invoice_id, &merchant_mnemonic)
            .await
            .map_err(|e| format!("Address derivation failed: {e}"))?;

        let expires_at = Utc::now() + Duration::minutes(30);

        // 5. Update invoice record with derived address details
        sqlx::query!(
            r#"
            UPDATE invoices
            SET wallet_address = $1,
                wallet_index = $2,
                expires_at = $3,
                payment_reference = $4,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = $5
            "#,
            deposit_address,
            derived_wallet_index as i32,
            expires_at,
            payment_reference,
            invoice_id
        )
            .execute(pool)
            .await
            .map_err(|e| format!("DB update failed: {e}"))?;

        // 6. Return payment response
        Ok(PaymentDetails {
            invoice_id,
            network: "base_sepolia".to_string(),
            deposit_address,
            token_address: Some("0x036CbD53842c5426634e7929541eC2318f3dCF7e".to_string()),
            decimals: 6,
            required_confirmations: 5,
            wallet_index: derived_wallet_index,
            expires_at,
        })
    }

    async fn cancel_payment(&self, _pool: &PgPool, invoice_id: Uuid) -> Result<(), String> {
        println!("BaseHandler::cancel_payment({invoice_id})");
        Ok(())
    }
}