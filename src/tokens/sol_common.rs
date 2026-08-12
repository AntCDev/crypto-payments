use rust_decimal::Decimal;
use serde_json::{json, Value};

use crate::networks::sol::SolanaNetwork;
use crate::tokens::CheckoutContext;

/// Shared checkout payload for every Solana handler (mainnet/testnet/devnet,
/// any mint). mint/token_program are read off `ctx`, not off the handler's
/// config — `ctx` reflects exactly what create_invoice_payment committed to
/// the row and what get_derive_address used to compute wallet_address, so
/// it's the authoritative source for what this specific invoice actually is.
///
/// PUBLIC OUTPUT — anyone with the invoice link can read this. No RPC URLs.
pub fn sol_checkout_data(
    network: &SolanaNetwork,
    symbol: &str,
    decimals: u8,
    ctx: &CheckoutContext,
) -> Result<Value, String> {
    let owner_address = ctx
        .payment_reference
        .as_deref()
        .ok_or("solana invoice has no payment_reference (owner_address)")?;

    // What create_invoice_payment actually wrote to wallet_address: the ATA
    // for a mint, or owner_address itself for native SOL. Both paths transfer
    // here — the smart path just additionally names `reference`.
    let deposit_address = ctx.wallet_address.as_str();

    let mint = ctx.token_address.as_deref();
    let token_program = ctx.token_program.as_deref();
    let is_native = mint.is_none();

    let amount_base = ctx.amount_requested.normalize().to_string();
    let amount_ui = to_display_units(ctx.amount_requested, decimals);

    // NAIVE PATH — always owner_address, never the raw ATA. See module note
    // above: normal wallets resolve/create the ATA themselves from the owner
    // pubkey. For native SOL these two addresses are identical anyway.
    let naive_path = json!({
        "deposit_address": owner_address,
    });

    // SMART PATH — Solana Pay transfer request. Same recipient the naive
    // path resolves to; `reference` is the extra account key that lets the
    // watcher (and Solana-Pay-aware wallets) tag this specific invoice.
    let mut solana_pay_url = format!(
        "solana:{}?amount={}&reference={}",
        deposit_address, amount_ui, owner_address
    );
    if let Some(mint) = mint {
        solana_pay_url.push_str(&format!("&spl-token={}", mint));
    }

    let smart_path = json!({
        "kind": "solana_pay_transfer",
        "recipient": deposit_address,
        "reference": owner_address,
        "mint": mint,
        "token_program": token_program,
        "solana_pay_url": solana_pay_url,
        // Extension point for Token-2022 specifics (transfer_fee_bps,
        // requires_ata_creation, etc) once a handler actually needs them —
        // deliberately not fabricated here since nothing populates them yet.
    });

    Ok(json!({
        "cluster": network.cluster_label(),
        "token": {
            "symbol": symbol,
            "mint": mint,
            "decimals": decimals,
            "is_native": is_native,
        },
        "amount": {
            "base_units": amount_base,
            "display": amount_ui,
        },
        "naive_path": naive_path,
        "smart_path": smart_path,
    }))
}

// Identical to evm_common's helper — worth factoring into a shared
// tokens/format.rs if a third network needs it too.
fn to_display_units(base: Decimal, decimals: u8) -> String {
    let mut d = base;
    if d.set_scale(decimals as u32).is_err() {
        return base.to_string();
    }
    d.normalize().to_string()
}