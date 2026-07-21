# Multichain Payment Processor (POC)

> ⚠️ **Status: Early Proof of Concept.** This project is under active, early-stage development. Core architecture is in place, but large parts of the implementation are stubbed out, unaudited, and subject to change without notice. **Do not use this in production or with real funds.** It is published for the purpose of showing work-in-progress design and code, not as a ready-to-deploy tool.

> ⚠️ **This project is custodial by design.** The operator's server generates and holds the private keys / signing authority for all merchant-facing wallets (naive-QR deposit addresses and, on EVM, the batching smart contract's admin keys). Merchants do not hold their own keys. If you run this software, you are the custodian of any funds it receives, regardless of whether the wallet is labeled as "belonging" to a merchant. See [`COMPLIANCE.md`](./COMPLIANCE.md) before deploying anywhere beyond your own local testing.

## What this is

A self-hostable, backend-first payment processor for accepting cryptocurrency payments across multiple networks, without depending on a hosted third-party gateway. You run it, you hold the keys, you own the data — and, as above, you also hold *other people's* keys if you enable multi-tenant mode.

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

## Deployment modes

The software runs in one of two modes, set at deployment/config time. This is not just a UI toggle — it changes what obligations fall on whoever operates the instance. See [`COMPLIANCE.md`](./COMPLIANCE.md) for why this distinction matters.

### 1. Solo mode

The operator is the only merchant. There is no merchant signup flow, no per-merchant onboarding, and no third party ever has funds passing through the instance other than the operator's own. This is the mode intended for "I'm running this for my own site(s)."

- No KYB flow is required or presented.
- Still fully custodial (see above) — the operator holds the keys for their own funds, same as any self-custody setup, just mediated by this software instead of a personal wallet.

### 2. Multi-tenant mode

Signups are open, and unrelated third parties can register as merchants and receive funds through the operator's instance. This is a materially different situation: the operator becomes the custodian of value on behalf of people they don't control, and in most jurisdictions is considered a service provider to them.

- **KYB (Know Your Business) on the merchant is a hard requirement to enable this mode.** The software will not allow multi-tenant signups to go live with KYB disabled. The operator selects and configures their own KYB provider/keys; this project does not ship a bundled KYB vendor.
- Multi-tenant mode is the mode that triggers most real-world compliance obligations (AML/CTF registration and reporting, recordkeeping, beneficial-owner checks, etc., depending on jurisdiction). Read `COMPLIANCE.md` in full before flipping this switch.

## Explicit non-goals: no KYC, no on/off-ramp

- **No end-customer KYC.** This project does not identify, verify, or collect data on the *end customer* paying an invoice — the person sending funds from their own wallet. Payments are accepted directly from customer-controlled wallets (naive QR or WalletConnect); the software has no visibility into who that customer is beyond an on-chain address, and it is not designed to gain that visibility. If a merchant's own regulatory situation requires end-customer KYC, that is out of scope for this project and is the merchant's (or the merchant's own tooling's) responsibility, not something this processor performs.
- **No fiat on-ramp or off-ramp, at all.** This project never converts crypto to fiat or fiat to crypto, never touches a bank account or card rail, and never integrates a fiat payment processor. Funds go in as crypto (from the customer's wallet) and stay as crypto for as long as they are within this system's custody. What a merchant does with funds *after* withdrawing them from the platform (e.g. sending them to their own exchange account) is entirely outside this project.

## Payment flows

Two ways to pay an invoice, by design:

- **Naive QR** — a plain address QR code. Maximally compatible (many wallets fail to parse QR codes that embed token/network/memo metadata reliably), at the cost of requiring a sweep step from the deposit address to treasury.
- **WalletConnect** — a direct connection to the user's wallet, allowing a single atomic transaction (e.g. a smart contract call on EVM, or a treasury transfer + memo on Solana) with no separate sweep required and no risk of user error on manual memo entry.

The system is built to soft-prefer WalletConnect where available, while keeping the naive QR path fully functional as a fallback.

## Architecture

The system is split into three layers:

1. **`NetworkClient` (trait)** — one implementation per network (`EVMNetwork`, `SolanaNetwork`, `EsploraNetwork`). Each network exposes a common set of required capabilities (e.g. `watch_payments`), but is free to implement them however makes sense for that chain — an EVM network might run `watch_blocks` + `watch_logs`, Esplora might only need `watch_blocks`, Solana might watch for memos.

2. **`TokenHandler` (trait)** — tokens are registered against a handler (e.g. `USDC_ETH` → `EVMHandler`, `USDC_BASE` → `BaseHandler`). Handlers contain the token-specific logic and call down into their network client with the right addresses/parameters.

3. **Orchestrator** — the entry point for creating an invoice. It is deliberately network-agnostic: it doesn't know or care whether a token ID resolves to EVM, Solana, or Esplora. It generates the invoice record, hands off to the relevant token handler to start watching for payment, and separately runs a service that scans for completed payments and dispatches webhooks (with retry and at-least-once delivery semantics).

This separation means adding a new chain means implementing one trait, and adding a new token on an existing chain means (in most cases) registering a handler with different parameters — not writing new payment-detection logic from scratch.

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
- [ ] KYB provider integration hooks (required before multi-tenant mode can ship)
- [ ] Security review pass
- [ ] Deployment docs
- [ ] Compliance/legal review pass (see `COMPLIANCE.md`)

## License

TBD.

## Before you deploy this anywhere real

Read [`COMPLIANCE.md`](./COMPLIANCE.md). It is not legal advice, but it explains why this software is custodial, what that tends to mean legally in different places, and what changes the moment you flip on multi-tenant mode.

---

This is a solo, spare-time project in active development. Feedback, issues, and PRs are welcome, but expect breaking changes for now.
