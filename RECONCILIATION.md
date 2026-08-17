# Reconciliation

Companion to [`README.md`](./README.md), [`NETWORKS.md`](./NETWORKS.md) and [`LEDGER.md`](./LEDGER.md).

Where `NETWORKS.md` describes how *expected* money is detected and `LEDGER.md` describes what happens to it afterwards, this document describes what the system does about *unexpected* money: value that arrives at, or leaves from, an address the processor controls without the processor having initiated or anticipated it.

> **Status: skeleton.** This is a design sketch written ahead of implementation, in the same spirit as `LEDGER.md`. It describes intended shape, not shipped code. Table names, account names, event names, status vocabularies and several of the broader concepts are provisional and expected to change on contact with the implementation — particularly the watch-set lifecycle, the probe cadence, and the treatment of unregistered assets. What is intended to be stable is the direction of the design: reconciliation is movement-driven rather than balance-driven, it produces ordinary attributed ledger entries rather than plugs, and it never writes back down into the chain layer.

---

## Index

1. [What reconciliation is for](#1-what-reconciliation-is-for)
2. [Design principle: movement-driven, not balance-driven](#2-design-principle-movement-driven-not-balance-driven)
3. [Scope: what is watched, and for how long](#3-scope-what-is-watched-and-for-how-long)
4. [Asset identity: the ledger is handler-agnostic](#4-asset-identity-the-ledger-is-handler-agnostic)
5. [External accounts: attributed and unexplained](#5-external-accounts-attributed-and-unexplained)
6. [Event taxonomy](#6-event-taxonomy)
7. [How a reconciliation event becomes ledger entries](#7-how-a-reconciliation-event-becomes-ledger-entries)
8. [Balance probing as an independent second check](#8-balance-probing-as-an-independent-second-check)
9. [Interaction with existing invariants](#9-interaction-with-existing-invariants)
10. [Merchant key export and shared custody](#10-merchant-key-export-and-shared-custody)
11. [Accepted risks](#11-accepted-risks)
12. [Open questions](#12-open-questions)

---

## 1. What reconciliation is for

The payment path is closed-loop: an invoice is created, an address or reference key is watched, a matching transfer is detected, and the resulting value is attributed to that invoice. Everything in `NETWORKS.md` describes that loop.

Real addresses do not respect that loop. An address the processor controls can gain or lose value for reasons that have nothing to do with any invoice:

- a payment arriving long after its invoice expired,
- a merchant moving their own funds using an exported key,
- someone sending to the wrong address,
- unsolicited dust and spam tokens, which on most chains carry advertising payloads,
- airdrops of assets the processor has never registered,
- operator-initiated movements performed manually during an incident.

Reconciliation is the mechanism by which those events become part of the accounting record instead of quietly invalidating it. The design intent is that the ledger tracks reality even when reality is inconvenient — an unexplained balance is recorded as an unexplained balance and raises an alarm, rather than being silently absorbed into a merchant's spendable balance or silently ignored.

Reconciliation is explicitly **not** a mechanism for correcting the ledger to match the chain. Corrections in this system are reversals, never edits, and nothing in the chain layer is ever adjusted to make the ledger balance (`LEDGER.md` §1).

---

## 2. Design principle: movement-driven, not balance-driven

The naive implementation of reconciliation polls an address balance, diffs it against the ledger-derived balance, and writes a plug entry for the difference. That approach is rejected here. A plug entry is a number with no transaction hash, no direction, no counterparty and no timestamp beyond the moment it was noticed. It cannot distinguish "the merchant swept with their own key" from "the sweeper has a bug," which are the two cases most worth telling apart.

The design instead requires a strictly larger ingest surface than the payment path currently uses. Where payment detection scans for transfers *to watched addresses that match a live invoice*, reconciliation ingest records **every movement touching a watched address, in either direction, regardless of whether it maps to an invoice**. This is the substantial work item hiding inside this phase — most of the reconciliation logic downstream of it is bookkeeping.

Once movements are ingested at that granularity, nearly every external event is fully attributable. The transaction hash, direction, amount, asset and counterparty are all known, so the resulting ledger entry is an ordinary attributed entry that happens to have been discovered rather than initiated. Balance probing (§8) remains in the design, but only as a secondary check on the small residue of things that movement ingest cannot see.

---

## 3. Scope: what is watched, and for how long

Reconciliation does not watch every address the processor has ever derived. That set grows without bound — one address per invoice, forever — and the overwhelming majority of those addresses are empty and will remain empty. The watch set is deliberately bounded.

### 3.1 The watch set

| Entity                                                               | Watched | Duration                                                             |
|----------------------------------------------------------------------|---------|----------------------------------------------------------------------|
| Merchant main address per `(network_type, chain_ref)` — HD index `0` | Yes     | Permanently, for the life of the merchant                            |
| Deposit address with a live invoice                                  | Yes     | Through the invoice lifecycle (already covered by payment detection) |
| Deposit address known to hold funds and not yet swept                | Yes     | Until the sweep confirms and the address is marked swept             |
| Deposit address marked swept                                         | **No**  | Dropped from the watch set                                           |
| Deposit address that never received value                            | **No**  | Eligible for the reuse pool where the network permits it (§3.3)      |
| Shared on-chain entities — e.g. the EVM `CustodialPaymentVault`      | Yes     | Permanently, while the entity is active                              |

Future networks add their own shared entities to the last row. The general rule is that anything holding value which is not a swept, empty deposit address is in the watch set.

### 3.2 Watch duration is a function of sweep policy

Because a deposit address leaves the watch set when it is marked swept, how long any given address is watched is determined entirely by the merchant's sweep policy rather than by reconciliation itself. A merchant configured to sweep on detection may have an address in the watch set for minutes. A merchant configured to sweep only when gas is below some percentile of its historic range, or only when a batch reaches a given size, may have the same address watched for days.

The consequence is stated plainly rather than engineered around: **value arriving at a deposit address after it has been swept and dropped from the watch set is not observed, and does not appear in the ledger.** This is an accepted risk (§11), inherited from the decision to bound the watch set by sweep state rather than to poll every address ever derived.

### 3.3 Address reuse and the reuse pool

Only addresses that have **never** received value — virgin addresses — are eligible for reuse. An address becomes eligible no sooner than its invoice's expiry plus a quarantine period of at least twenty-four hours, so that a late payment against the original invoice is overwhelmingly likely to land while the address is still uniquely attributable.

Reuse is currently planned for the UTXO model only. EVM and Solana do not reuse addresses, specifically because the misattribution risk that reuse introduces is not worth taking on chains where the gap-limit problem it solves is less acute for external wallet import.

A reused address re-enters the watch set with its new invoice, and its history remains attributable at the movement level because every movement carries its own transaction hash and block position.

### 3.4 What the watch set is not

The watch set is not a source of truth about balances. It is a subscription list. Balances are derived from the ledger, and the ledger is built from movements. An address being watched means movements touching it will be ingested; it does not mean the system holds a cached balance for it.

---

## 4. Asset identity: the ledger is handler-agnostic

Reconciliation depends on being able to name an asset without naming the code that manages it. A withdrawal or an unsolicited transfer arrives as a fact about a token contract, not as a fact about a `TokenHandler`.

The ledger therefore identifies assets by `(network_type, chain_ref, asset_kind, token_address)` and never by handler. `asset_kind` is an explicit discriminator — native versus contract-issued — rather than a sentinel address, because the zero-address convention is an EVM idiom that means nothing on Solana or on UTXO chains and would need special-casing in every implementation.

Three rules follow, and they are enforced at registration time rather than left to convention:

- **Addresses are canonicalized on the way in.** EVM checksummed-versus-lowercase, Solana base58 case sensitivity and bech32 lowercasing each provide a way for two handlers to register the same real asset as two distinct ledger assets. The registry stores one canonical form per network and enforces it with a constraint.
- **Decimals and symbol belong to the asset registry, not to the handler.** Two handlers disagreeing about decimals for the same address is a silent error of up to twelve orders of magnitude. Registration fails loudly on disagreement rather than resolving it.
- **`handler_id` lives on the chain layer.** It is recorded on `chain_transactions` and `chain_movements` — "which code moved this" is operational metadata worth having for audit — and never on a journal or entry. The ledger has no concept of a handler.

The practical payoff is that multiple handlers may manage the same asset (a raw-RPC `USDC` and a crate-backed `USDC_Crates`, say) without the ledger fragmenting, and a handler can be deleted from the codebase without orphaning historical entries.

Withdrawals resolve in the opposite direction: given an asset the merchant wants to move, the registry is queried for handlers advertising that `(network, chain_ref, token_address)`, and the operator's configuration determines whether one is chosen automatically or the choice is surfaced to the merchant.

---

## 5. External accounts: attributed and unexplained

The counterparty side of every reconciliation entry is one of two accounts, and the distinction between them is the main diagnostic output of the whole subsystem.

| Account                | Written when                                                                                        | Expected state                                                 |
|------------------------|-----------------------------------------------------------------------------------------------------|----------------------------------------------------------------|
| `external:attributed`  | A movement was ingested. Transaction hash, direction, amount, asset and counterparty are all known. | Grows and shrinks with normal external activity. Unremarkable. |
| `external:unexplained` | A balance probe found a delta with no movement to source it.                                        | **Zero, permanently.**                                         |

Both are written automatically — the ledger records what is true regardless of whether the truth is flattering. Only the second is an alarm. A non-zero `external:unexplained` balance means the system holds value it cannot explain, or is missing value it cannot account for, and that is a monitoring event rather than an accounting entry to be shrugged at.

The design intent is that `external:unexplained` is small, rare, and investigated by a human every time it moves. If it becomes routine, that is a signal that movement ingest is missing a case, not a signal to widen a tolerance.

Related accounts that reconciliation interacts with but does not own are defined in `LEDGER.md` — in particular the operator→merchant gas advance receivable, which reconciliation observes as a movement like any other.

---

## 6. Event taxonomy

Provisional. These are the categories the design expects to need; the implementation may merge or split them.

### 6.1 Inbound, not matching a live invoice

- **Late payment.** A payment against an expired invoice, arriving after the reuse quarantine has passed or against an address that was never eligible for reuse. The value is real and is credited to the merchant. The invoice is untouched (§9).
- **Mistaken send.** Value sent to a controlled address by someone with no relationship to any invoice. Credited to the merchant, flagged for operator review, since the merchant may face a return request they need to be aware of.
- **Dust and spam.** Small unsolicited transfers, frequently carrying advertising payloads in token metadata or in an accompanying memo. Recorded, credited at zero economic weight, and excluded from sweep candidacy.
- **Airdrop of an unregistered asset.** Recorded as observed but held in a non-spendable state until the asset is registered, since the system cannot value, sweep or withdraw an asset it has no handler for.
- **Merchant top-up.** A merchant sending their own funds in from an external wallet, typically to cover native-token gas at an address they intend to move funds from.

### 6.2 Outbound, not initiated by the processor

- **Merchant self-move.** A merchant using an exported key to move funds directly (§10).
- **Operator manual movement.** An operator moving funds outside the normal sweep path, typically during an incident.

Both are recorded as debits against the merchant's holding account with `external:attributed` on the other side. Neither is treated as an error; both are treated as facts.

### 6.3 Value that changes in transit

- **Fee-on-transfer assets**, where the amount received is less than the amount sent.
- **Rebasing assets**, where a balance changes with no transaction at all.

Both are visible only through balance probing, and both are candidates for the unregistered-asset treatment: recorded, not credited as spendable, and excluded from sweeps until explicitly supported.

---

## 7. How a reconciliation event becomes ledger entries

The pipeline is the same one `LEDGER.md` describes, entered from a different door:

```
   chain  ──▶  chain_transactions  ──▶  chain_movements  ──▶  ledger
observed        (what happened)         (what moved)       (what it means)
                                             ▲
                                             │
                               reconciliation ingest enters here,
                               not through the invoice path
```

Reconciliation writes no rows to `payments`. That table is invoice-scoped and stays that way; a movement with no invoice has nothing to say to it.

Idempotency comes from the same place it does everywhere else in the system: the journal `dedupe_key` is derived from `(network_type, chain_ref, tx_hash, event_index)`, so re-ingesting a range produces no new effects. Reconciliation ticks advance their cursor only after the work commits, and scope their scan state separately from the payment scanners so the two cannot move each other's cursor.

A reconciliation journal is an ordinary double-entry journal. Nothing about it is special-cased in balance derivation, and merchant balances remain derived rather than incremented.

---

## 8. Balance probing as an independent second check

Movement ingest cannot see everything. Rebasing assets change balances with no transaction. Fee-on-transfer assets make the sent amount an unreliable proxy for the received amount. Assets can arrive before an address enters the watch set, or before the processor started watching the chain at all.

Balance probing exists to catch that residue. It runs independently of movement ingest, at a lower frequency, over the same watch set defined in §3. For each `(address, asset)` pair it compares the on-chain balance to the ledger-derived balance for that address.

- A delta fully explained by movements that are ingested but not yet final is ignored.
- Any remaining delta is written against `external:unexplained` and raises an alarm.

The probe is deliberately not the primary mechanism. If probing is finding deltas regularly, the correct response is to fix movement ingest, not to tune the probe.

---

## 9. Interaction with existing invariants

`NETWORKS.md` §8 states that an expired invoice is never resurrected by a late payment recompute. That invariant is correct and unchanged. It is also, on its own, incomplete: a late payment is real value sitting at an address the processor controls, and something has to represent it.

Reconciliation is where that lands. The invoice remains expired and its totals are untouched; the value is credited to the merchant through a reconciliation journal. The two statements are consistent because they operate at different layers — invoice status is a claim about a commercial agreement, ledger balance is a claim about custody of value.

Two further interactions worth naming:

- **`payments` is append-mostly and an amount is written once, at insert.** Fee-on-transfer assets violate the assumption behind that invariant, since the amount observed leaving is not the amount arriving. Reconciliation records the received amount as its own movement rather than rewriting a payment row.
- **Nothing durable lives only in memory.** The watch set is rebuilt from the database every tick, like every other watch set in the system. It is a query, not a cached structure.

---

## 10. Merchant key export and shared custody

Each merchant is issued a wallet and mnemonic at signup. HD index `0` is reserved for that merchant's main address on the network; subsequent indices are derived per invoice. Merchants are able to view and save their own keys.

The purpose of this is narrow and should be stated as such wherever it is surfaced in the UI: it is a glass-break measure, so that a merchant retains control of their funds if the operator or the server disappears. It is not intended as a day-to-day access path.

**Sweeping and other configured tasks are not disabled by key export.** The system continues to operate its normal policies on those addresses. A merchant who moves funds themselves may therefore cause behaviour that is undesirable but not incorrect: a sweep that finds dust and spends more in gas than it recovers, a sweep transaction that fails or is replaced because the merchant's own transaction consumed the nonce or spent the UTXO first, a batch that is smaller than the policy intended. These are consequences of two parties acting on one address, not defects in the code.

Three practical problems come with export, and the design addresses each:

- **The gap limit.** BIP44 wallets stop scanning after roughly twenty consecutive empty addresses. Per-invoice deposit addresses are mostly empty forever, so a merchant importing their mnemonic into an ordinary wallet may see nothing at all — precisely at the moment the glass-break path matters. Virgin-address reuse keeps the used range dense on networks where it applies. Beyond that, export is accompanied by a manifest of used indices and addresses, and by a small standalone CLI tool that scans past the default gap. The tool exists because in the scenario it is built for, the operator's database is not available.
- **Derivation path divergence.** Wallets disagree about derivation paths, most visibly on Solana, and the failure mode is a merchant importing a valid mnemonic and seeing an empty wallet. Paths are pinned and documented per network in `NETWORKS.md`, and the documentation is intended to be detailed enough to be read directly or handed to a language model by a merchant working through a recovery.
- **Scope of what is exported.** A mnemonic exports the entire future derivation tree, not just the addresses issued so far. It cannot be revoked. Exporting individual derived keys is worse ergonomically but bounded, and the trade-off is worth surfacing to the merchant explicitly.

Regardless of which is exported, the operator remains the custodian in the sense that matters legally (`COMPLIANCE.md`) while no longer being the only party able to move funds. Reconciliation is what keeps the books honest in that state.

---

## 11. Accepted risks

Listed explicitly so that they are decisions rather than oversights.

1. **Value arriving at a swept, de-watched deposit address is not observed.** The watch set is bounded by sweep state; anything sent to an address after it leaves the set is invisible to the system until and unless someone investigates manually.
2. **Payment misattribution from address reuse.** Applies to the UTXO model only, since EVM and Solana do not reuse addresses. Mitigated by reusing only virgin addresses and only after expiry plus at least a day of quarantine, which makes a colliding late payment unlikely without making it impossible.
3. **Merchant-initiated movements can cause failed or uneconomic sweeps.** Accepted as the cost of giving merchants a glass-break path while leaving automated policies enabled by default.
4. **Unregistered assets are recorded but not valued.** The system will show that something arrived without being able to say what it is worth or move it.
5. **Reconciliation is only as complete as movement ingest.** Anything the ingest layer cannot see reaches the ledger only through balance probing, and only as an unexplained delta.

---

## 12. Open questions

- **The EVM vault model.** Two shapes are under consideration: a default contract acting only as a relay, and an optional per-merchant contract that a merchant funds in exchange for batched transactions and lower per-payment gas. The choice determines whether the vault is a shared entity in the watch set or a per-merchant one, and it is the main reason the fund-isolation work is not yet complete — the vault is currently the only place where the "no merchant's funds touch another's" property is not trivially true.
- **Probe cadence and cost.** Probing every watched `(address, asset)` pair has a real RPC cost that scales with merchant count and sweep policy laxity. The right frequency is probably per-network and possibly per-merchant.
- **Response policy for a persistent unexplained balance.** The alarm is defined; what happens if it stays lit is not.
- **Whether reconciliation tolerance should change after key export.** Widening it makes shared-custody noise quieter, at the cost of making a real discrepancy harder to see. The current inclination is not to widen it and to accept the noise.
- **Return-of-funds workflow.** Mistaken sends are recorded and credited, but the process by which a merchant or operator returns them is undefined.