use crate::networks::esplora::EsploraNetwork;
use crate::networks::{BitcoinNetwork, NetworkClient, NetworkRegistry};
use crate::tokens::{
    decrypt_data, CheckoutContext, CheckoutView, PaymentDetails, TokenHandler, TokenRegistry,
};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use rust_decimal::Decimal;
use serde_json::{json, Value};
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

/// Bitcoin has no contract tokens in this design — one native asset per network.
/// Unlike the EVM configs there is no `token_address`: BTC is always native,
/// so the field is omitted entirely rather than carried as a permanent `None`.
#[derive(Debug, Clone)]
pub struct TokenConfig {
    pub id: &'static str,
    pub name: &'static str,
    pub detail: &'static str,
    pub info: &'static str,
    /// Which entry in the Esplora side of the NetworkRegistry this handler binds to.
    pub network: BitcoinNetwork,
    /// Value written to `PaymentDetails.network` and `invoices.chain_ref`.
    pub network_label: &'static str,
    /// BIP21 URI scheme. `bitcoin:` on all three networks — testnet/signet
    /// wallets key off the address HRP (tb1/bcrt1), not the scheme.
    pub bip21_scheme: &'static str,
    pub block_explorer: &'static str,
    pub decimals: u8,
    pub required_confirmations: i32,
    /// Bitcoin blocks are ~10 min, so a 30 min window (the EVM default) is too
    /// tight — a customer can pay on time and still have the invoice expire
    /// before the transaction is mined.
    pub invoice_ttl_minutes: i64,
}

/// Token configuration list for Bitcoin (Esplora-backed).
///
/// All three are declared unconditionally; `register` only registers the ones
/// whose network is actually present in the NetworkRegistry, so an operator who
/// only sets `ESPLORA_MAINNET_URLS` gets exactly one BTC token advertised.
pub const BITCOIN_TOKENS: &[TokenConfig] = &[
    TokenConfig {
        id: "BTC",
        name: "BTC",
        detail: "(Bitcoin)",
        info: "Native Bitcoin on mainnet.",
        network: BitcoinNetwork::Mainnet,
        network_label: "bitcoin",
        bip21_scheme: "bitcoin",
        block_explorer: "https://mempool.space",
        decimals: 8,
        required_confirmations: 2,
        invoice_ttl_minutes: 60,
    },
    TokenConfig {
        id: "BTC_TESTNET4",
        name: "BTC",
        detail: "(Testnet4)",
        info: "Native Bitcoin on the testnet4 network.",
        network: BitcoinNetwork::Testnet4,
        network_label: "bitcoin_testnet4",
        bip21_scheme: "bitcoin",
        block_explorer: "https://mempool.space/testnet4",
        decimals: 8,
        required_confirmations: 1,
        invoice_ttl_minutes: 60,
    },
    TokenConfig {
        id: "BTC_SIGNET",
        name: "BTC",
        detail: "(Signet)",
        info: "Native Bitcoin on the signet test network.",
        network: BitcoinNetwork::Signet,
        network_label: "bitcoin_signet",
        bip21_scheme: "bitcoin",
        block_explorer: "https://mempool.space/signet",
        decimals: 8,
        required_confirmations: 1,
        invoice_ttl_minutes: 60,
    },
];

/// One shared view for all three networks. There is no WalletConnect path on a
/// UTXO chain, so this view is QR-only — it must not render an "approve" or
/// "connect wallet" affordance the way the EVM view does.
pub const CHECKOUT_VIEW: CheckoutView = CheckoutView {
    id: "esplora",
    path: "/checkout/esplora.html",
    description: "Bitcoin checkout: BIP21 deposit QR only, no wallet-connection path.",
};

pub fn register(registry: &mut TokenRegistry, networks: Arc<NetworkRegistry>) {
    for config in BITCOIN_TOKENS {
        // Advertise a token only if its network was configured at boot.
        let network = match networks.esplora_network(config.network) {
            Some(net) => net,
            None => {
                println!(
                    "  ⏭️  {:?} not configured — skipping {}",
                    config.network, config.id
                );
                continue;
            }
        };

        let handler = BitcoinHandler {
            network,
            config: config.clone(),
        };

        registry.register_token(
            config.id,
            config.name,
            config.detail,
            config.info,
            handler,
        );
    }
}

pub struct BitcoinHandler {
    network: Arc<EsploraNetwork>,
    config: TokenConfig,
}

#[async_trait]
impl TokenHandler for BitcoinHandler {
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

        // 4. Derive deposit address. On Bitcoin the address IS the payment
        //    identifier — there is no memo/reference equivalent, so the third
        //    return value is expected to be None here.
        let (deposit_address, derived_wallet_index, payment_reference) = self
            .network
            .get_derive_address(pool, merchant_id, invoice_id, &merchant_mnemonic)
            .await
            .map_err(|e| format!("Address derivation failed: {e}"))?;

        // Cheap guard against a derivation path/HRP mismatch (e.g. a mainnet
        // address handed back by the signet client) reaching a customer.
        if !self.network.validate_address(&deposit_address) {
            return Err(format!(
                "Derived address {deposit_address} is not valid for {}",
                self.config.network_label
            ));
        }

        let expires_at = Utc::now() + Duration::minutes(self.config.invoice_ttl_minutes);

        // 5. Update invoice record with derived address details and configuration metadata.
        //    `token_address` is left NULL — BTC is native, there is no contract.
        sqlx::query!(
            r#"
            UPDATE invoices
            SET wallet_address = $1,
                wallet_index = $2,
                expires_at = $3,
                payment_reference = $4,
                token_address = NULL,
                token_decimals = $5,
                required_confirmations = $6,
                network_type = 'esplora',
                chain_ref = $7,
                updated_at = CURRENT_TIMESTAMP
            WHERE id = $8
            "#,
            deposit_address,
            derived_wallet_index as i32,
            expires_at,
            payment_reference,
            self.config.decimals as i16,
            self.config.required_confirmations as i16,
            self.config.network_label,
            invoice_id
        )
        .execute(pool)
        .await
        .map_err(|e| format!("DB update failed: {e}"))?;

        // 6. Return payment response matching token configuration
        Ok(PaymentDetails {
            invoice_id,
            network: self.config.network_label.to_string(),
            deposit_address,
            token_address: None,
            decimals: self.config.decimals,
            required_confirmations: self.config.required_confirmations,
            wallet_index: derived_wallet_index,
            expires_at,
        })
    }

    fn checkout_view(&self) -> CheckoutView {
        CHECKOUT_VIEW
    }

    async fn checkout_data(
        &self,
        _pool: &PgPool,
        ctx: &CheckoutContext,
    ) -> Result<Value, String> {
        // Amounts are carried in base units (satoshis) as NUMERIC. BIP21 wants
        // whole BTC, so convert here rather than in the view.
        let remaining_sats = (ctx.amount_requested - ctx.amount_received).max(Decimal::ZERO);
        let remaining_btc = sats_to_btc(remaining_sats);

        // Omit the amount parameter once the invoice is already covered, so a
        // re-opened page doesn't produce a QR that asks for another 0 BTC.
        let bip21_uri = if remaining_sats > Decimal::ZERO {
            format!(
                "{}:{}?amount={}",
                self.config.bip21_scheme, ctx.wallet_address, remaining_btc
            )
        } else {
            format!("{}:{}", self.config.bip21_scheme, ctx.wallet_address)
        };

        Ok(json!({
            "network": self.config.network_label,
            "chain": "bitcoin",
            "address": ctx.wallet_address,
            "bip21_uri": bip21_uri,
            "decimals": self.config.decimals,
            "amount_requested_sats": ctx.amount_requested.to_string(),
            "amount_received_sats": ctx.amount_received.to_string(),
            "amount_remaining_sats": remaining_sats.to_string(),
            "amount_requested_btc": sats_to_btc(ctx.amount_requested),
            "amount_received_btc": sats_to_btc(ctx.amount_received),
            "amount_remaining_btc": remaining_btc,
            "required_confirmations": self.config.required_confirmations,
            // Anything at or below this is unspendable-in-practice once fees are
            // considered; the view can warn instead of silently accepting it.
            "dust_threshold_sats": 546,
            "explorer_address_url": format!("{}/address/{}", self.config.block_explorer, ctx.wallet_address),
            "explorer_tx_base": format!("{}/tx", self.config.block_explorer),
            "expires_at": ctx.expires_at,
        }))
    }

    async fn cancel_payment(&self, _pool: &PgPool, invoice_id: Uuid) -> Result<(), String> {
        println!(
            "BitcoinHandler::cancel_payment({invoice_id}) for token: {}",
            self.config.id
        );
        Ok(())
    }
}

/// Satoshis -> BTC as a BIP21-safe decimal string.
/// Trailing zeros are trimmed; whole amounts render without a fractional part.
fn sats_to_btc(sats: Decimal) -> String {
    let btc = (sats / Decimal::from(100_000_000u64)).round_dp(8).normalize();
    btc.to_string()
}
