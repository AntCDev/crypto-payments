# Multichain Payment Processor (POC)

> ⚠️ **Status: Early Proof of Concept.** This project is under active, early-stage development. Core architecture is in place, but large parts of the implementation are stubbed out, unaudited, and subject to change without notice. **Do not use this in production or with real funds.** It is published for the purpose of showing work-in-progress design and code, not as a ready-to-deploy tool.

## What this is

A self-hostable, backend-first payment processor for accepting cryptocurrency payments across multiple networks, without depending on a hosted third-party gateway. You run it, you hold the keys, you own the data.

The goal is a single, coherent invoicing/payment-detection layer that can sit behind any merchant backend and tell it, reliably, "this invoice was paid" — regardless of which chain or token the customer used.

## Why

Most existing self-hosted options are either narrowly scoped to a single chain, or only partially open source. This project exists to explore what a genuinely chain-agnostic, fully open implementation looks like, and to have a concrete, inspectable piece of infrastructure rather than a black box.

It is also a learning project — a way to get hands-on with HD wallet derivation, chain-specific watching/confirmation logic, and the operational edge cases (reorgs, underpayment, idempotent webhook delivery) that any real payment system eventually has to deal with.

### Compared to existing tools

| Project | Scope | Notes |
|---|---|---|
| **BTCPay Server** | Bitcoin-focused | Mature, but not designed as a multichain/EVM/SOL-agnostic layer |
| **SHKeeper** | Handful of tokens | Minimal, limited network/token coverage |
| **PayRam** | Multichain, partially closed | Core logic is not fully open; ran into stability issues in local testing |
| **This project** | EVM (+ L2s), Solana, Esplora (Bitcoin-style UTXO) | Fully open source, unified architecture across networks |

## Supported networks (in progress)

- **EVM** — Ethereum mainnet + L2s (Base, etc.)
- **Solana**
- **Esplora-compatible** (Bitcoin and similar UTXO chains)

Token support is designed to be cheap to extend: most EVM tokens reuse the same handler logic with different addresses/decimals, so adding a new ERC-20/BEP-20-style token is close to a config change rather than new code.

## Architecture

The system is split into three layers:

1. **`NetworkClient` (trait)** — one implementation per network (`EVMNetwork`, `SolanaNetwork`, `EsploraNetwork`). Each network exposes a common set of required capabilities (e.g. `watch_payments`), but is free to implement them however makes sense for that chain — an EVM network might run `watch_blocks` + `watch_logs`, Esplora might only need `watch_blocks`, Solana might watch for memos.

2. **`TokenHandler` (trait)** — tokens are registered against a handler (e.g. `USDC_ETH` → `EVMHandler`, `USDC_BASE` → `BaseHandler`). Handlers contain the token-specific logic and call down into their network client with the right addresses/parameters.

3. **Orchestrator** — the entry point for creating an invoice. It is deliberately network-agnostic: it doesn't know or care whether a token ID resolves to EVM, Solana, or Esplora. It generates the invoice record, hands off to the relevant token handler to start watching for payment, and separately runs a service that scans for completed payments and dispatches webhooks (with retry and at-least-once delivery semantics).

This separation means adding a new chain means implementing one trait, and adding a new token on an existing chain means (in most cases) registering a handler with different parameters — not writing new payment-detection logic from scratch.

## Payment flows

Two ways to pay an invoice, by design:

- **Naive QR** — a plain address QR code. Maximally compatible (many wallets fail to parse QR codes that embed token/network/memo metadata reliably), at the cost of requiring a sweep step from the deposit address to treasury.
- **WalletConnect** — a direct connection to the user's wallet, allowing a single atomic transaction (e.g. a smart contract call on EVM, or a treasury transfer + memo on Solana) with no separate sweep required and no risk of user error on manual memo entry.

The system is built to soft-prefer WalletConnect where available, while keeping the naive QR path fully functional as a fallback.

## Webhooks

Payments can be underpaid, overpaid, or corrected across multiple transactions, so the webhook model reports state rather than a single "paid" boolean:

- `payment.received` — fired on detected funds, reporting received/total/expected amounts (so partial payments are visible)
- `payment.confirmed` — fired once configured confirmation depth is reached
- `payment.orphaned` — fired if a previously confirmed block is reorganized out; the merchant decides how to react

Confirmation depth is configurable per token/network. Monitoring continues after initial confirmation (at a reduced frequency) to allow reorg detection.

## Data / correctness principles

- PostgreSQL, designed around ACID guarantees and idempotent operations — invoice creation, sweeps, and webhook dispatch are all built to be safely retryable without double-processing.
- No NFT, trading, or speculative-market functionality. This is infrastructure for accepting payment, not a wallet, exchange, or trading tool.

## Tech stack

- **Backend:** Rust
- **Database:** PostgreSQL
- **Frontend (planned):** TypeScript + Tailwind

## Roadmap

- [ ] Finish token handler coverage beyond the initial set
- [ ] Harden sweep/gas-feeding logic and add test coverage
- [ ] Admin frontend (wallet/token/confirmation configuration)
- [ ] Security review pass
- [ ] Deployment docs

## License

TBD.

---

This is a solo, spare-time project in active development. Feedback, issues, and PRs are welcome, but expect breaking changes for now.
