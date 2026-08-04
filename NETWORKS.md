# Network Implementations & Payment Detection Flow

> Companion to [`README.md`](./README.md). This document describes **how payments are
> detected**, per network. It is implementation-level: it names the actual functions,
> tables, constants and events involved, and calls out where behaviour is deliberate
> versus where it is a known gap.
>
> **Status:** EVM is implemented and documented in full. Solana and Esplora are
> specified here but **not yet implemented** — their sections are TODOs describing the
> intended shape, not the current code.

---

## 1. Design principle: agnostic orchestrator, optimized paths

The orchestrator that creates an invoice does not know what a chain is. It resolves a
token ID to a `TokenHandler`, the handler resolves to a `NetworkClient`, and the network
client is free to detect payment however that chain makes sense.

That freedom is the point. Every chain exposes a different "best" way to be told that
money arrived, and forcing them all through one abstraction (e.g. "poll balances")
throws away everything each chain is actually good at.

```
                        ┌──────────────────────┐
   POST /invoices  ──▶  │     Orchestrator     │   network-agnostic
                        │  (creates invoice)   │   knows: token_id, amount, merchant
                        └──────────┬───────────┘
                                   │ resolve token_id
                        ┌──────────▼───────────┐
                        │     TokenHandler     │   token-specific params
                        │  (USDC_ETH, SOL, …)  │   address, decimals, confirmations
                        └──────────┬───────────┘
                                   │
                        ┌──────────▼───────────┐
                        │    NetworkClient     │   chain-specific detection
                        │  EVM / SOL / Esplora │   watch_* services
                        └──────────┬───────────┘
                                   │ writes rows
                        ┌──────────▼───────────┐
                        │   payments/invoices  │  ──▶  webhook queue  ──▶  merchant
                        └──────────────────────┘
```

Everything below the orchestrator is allowed to be chain-shaped. Everything above it is
not allowed to care.

---

## 2. The two payment paths (and why both are always live)

Each invoice is presented to the payer as **one page with two options side by side**:

| | Naive path | Smart path |
|---|---|---|
| **UI** | QR code of a plain address | "Connect wallet" button |
| **Correlation** | by *address* (one derived address per invoice) | by *identifier carried in the transaction* |
| **User error surface** | none — they scan and send | none — the app builds the tx |
| **Wallet compatibility** | maximal (any wallet can send to an address) | requires WalletConnect-capable wallet |
| **Sweep required** | yes (deposit address → treasury) | no (funds land at destination directly) |
| **Detection mechanism** | scan blocks for transfers to watched addresses | scan for the contract event / memo |

**Both are displayed at once and both must be watched at once.** The backend cannot know
which one the payer will use — they might connect a wallet, change their mind, and scan
the QR instead. So on any network that supports both, *two independent watcher services*
run concurrently against the same invoice set, and both write into the same `payments`
table. Idempotency (§6) is what makes that safe.

The naive path exists because embedding token/chain/memo metadata in a QR code is
unreliable in practice — a meaningful fraction of wallets misparse it or silently drop
the memo. A bare address QR is the one thing that works everywhere.

The smart path exists because it removes the two worst failure modes of the naive path:
the sweep step (gas cost + an extra on-chain hop + a window where funds sit at a
throwaway address) and manual identifier entry (a user typing a UUID into a memo field
is a support ticket generator).

### Per-network path availability

| Network | Naive path | Smart path | Smart path mechanism |
|---|---|---|---|
| **EVM** | HD-derived deposit address | ✅ | `CustodialPaymentVault` contract call → `Payment` event carrying the invoice UUID |
| **Solana** | HD-derived deposit address | ✅ *(planned)* | direct transfer to treasury + memo instruction, locked behind wallet-connect |
| **Esplora (UTXO)** | HD-derived deposit address | ❌ | no equivalent primitive — naive only |
| **TRON** *(future)* | HD-derived deposit address | TBD | whatever TRON gives us for fee/energy optimization |

On Solana the memo is **only** ever produced by the wallet-connect path. We never ask a
human to type a memo — that is precisely the class of error the smart path exists to
delete.

---

## 3. Shared vocabulary

### Payment lifecycle

```
                 ┌──────────────────────────────────────────┐
                 │                                          │
  (first sight)  ▼                                          │  re-mined after reorg
   ────────▶ detected ──▶ merchant_confirmed ──▶ system_confirmed
                 │                │
                 │ tx gone        │ tx gone
                 ▼                ▼
              orphaned ◀──────────┘
```

| Status | Meaning | Transition rule |
|---|---|---|
| `detected` | seen on chain, 0+ confirmations | insert on first sighting |
| `merchant_confirmed` | reached the invoice's `required_confirmations` | promoted in `refresh_confirmations` |
| `system_confirmed` | reached `FINAL_CONFIRMATIONS`; considered irreversible, stops being polled | promoted in `refresh_confirmations` |
| `orphaned` | the tx is no longer mined anywhere | set in `handle_reorg` |

### Invoice status

Derived, never incremented. `recompute_invoice_totals` recomputes from
`SUM(payments.amount) WHERE status <> 'orphaned'`:

| Received vs requested | Status |
|---|---|
| `= 0` | `pending` |
| `> 0` and `< requested` | `underpaid` |
| `= requested` | `paid` |
| `> requested` | `overpaid` |
| — | `expired` (terminal; never overwritten by the recompute) |

### Amount representation

All amounts in the DB are **base units** (wei, lamports, satoshis, smallest token unit).
Decimals are applied at the presentation layer only. Nothing in the detection pipeline
ever handles a human-readable amount.

### Webhooks emitted by the EVM implementation

| Event | Fired when | Once-only latch |
|---|---|---|
| `payment.detected` | first insert of a `(invoice_id, tx_hash)` row | unique index on `payments` |
| `payment.confirmed` | payment crosses `required_confirmations` | guarded `UPDATE … WHERE status = 'detected'` |
| `payment.finalized` | payment crosses `FINAL_CONFIRMATIONS` | guarded `UPDATE … WHERE status <> 'system_confirmed'` |
| `payment.orphaned` | tx disappeared from the chain | status transition to `orphaned` |
| `payment.finished` | invoice's received total first reaches the requested amount | guarded `UPDATE` on invoice status |

Every `enqueue_webhook` call shares the same DB transaction as the state change that
justifies it, so the state change and the notification commit or roll back together.

### Shared tables

| Table | Purpose |
|---|---|
| `invoices` | one row per invoice; `network_type`, `chain_ref`, `wallet_address`, `token_address`, `created_block`, `amount_requested`, `amount_received`, `required_confirmations`, `expires_at` |
| `payments` | one row per `(invoice_id, tx_hash)`; the unique index is the idempotency backbone |
| `merchant_wallets` | merchant destination wallet per `network_type` |
| `network_scan_state` | scan cursor per `(network_type, chain_ref, scope)` — `last_block` + `last_block_hash` |
| `network_seen_blocks` | remembered block headers per scope, for reorg detection |
| webhook queue | written via `enqueue_webhook`, drained by a separate dispatcher with at-least-once semantics |

`scope` is what lets two independent scanners share one chain: the address scanner uses
`SCAN_SCOPE_ADDRESSES`, the logs scanner uses `SCAN_SCOPE_LOGS`, and neither can move the
other's cursor.

---

## 4. EVM — implemented

`EVMNetwork` is constructed with a `chain_id`, one or more RPC URLs, and an optional
vault contract address. `network_name` is `EVM_{chain_id}`, `chain_ref` is the chain ID
as a string. One instance per chain; Ethereum, Base and Polygon are three instances of
the same struct.

``` rust
EVMNetwork::new(chain_id, rpc_urls, contract_address)
```

`contract_address` being `None`/empty means the vault is not deployed on this chain
(yet). Native/ERC-20 address watching still runs; `watch_logs` logs a line and returns
instead of spinning.

### 4.1 Two services, one chain

| Service | Path served | Loop | Correlates by |
|---|---|---|---|
| `watch_addresses` | naive QR (native + ERC-20 to a deposit address) | `tick_addresses` | `to` address |
| `watch_logs` | WalletConnect (vault contract call) | `tick_logs` | invoice UUID in the event topic |

Both loops are structurally identical at the top level:

``` rust
loop {
    if let Err(e) = self.tick_*(pool).await {
        eprintln!(...);          // transient by assumption — cursor did not advance
    }
    sleep(POLL_INTERVAL_SECS).await;
}
```

The error posture is deliberate: **the cursor is only advanced on success**, so a failed
tick costs nothing but a poll interval. RPC hiccups, quorum splits mid-reorg and lagging
providers all resolve themselves by simply redoing the work next tick. Nothing in a tick
needs to be "rolled back" because nothing irreversible happens before the cursor moves.

### 4.2 RPC layer: quorum

Every RPC call goes through one of three quorum wrappers:

| Function | Result shape | Used by |
|---|---|---|
| `call_rpc` | plain string | `eth_blockNumber` |
| `call_rpc_json` | `serde_json::Value` | `eth_getBlockByNumber`, `eth_getTransactionByHash` |
| `get_logs` | `Vec<Log>` | `eth_getLogs` |

Rules:

1. **One URL configured → skip quorum entirely.** Local dev and testnets where you only
   have one provider are not required to invent a second one.
2. **Multiple URLs → fan out to all of them concurrently** (`join_all`), then:
    - fewer than 2 successful responses → `Quorum failed` error (transient, retried);
    - otherwise return the first value that **at least 2 endpoints agree on**.
3. **All responded, none matched** → `Quorum disagreement`. This is *not* something a
   tiebreaker can fix — there is no majority to break the tie toward — so it is treated
   as transient and retried. In practice this is what a reorg looks like from the
   outside while providers are still converging.

Comparison is structural, not textual: `call_rpc_json` compares `serde_json::Value`s so
key ordering differences between providers don't cause false disagreements, and
`get_logs` sorts each provider's log set by `(block_number, log_index)` before comparing.

### 4.3 Who we watch

`load_watched_invoices` is the single source of truth for both services. It returns
everything with an **open interest** on this chain:

```sql
(i.status = 'pending' AND i.expires_at > now())
OR EXISTS (SELECT 1 FROM payments p
            WHERE p.invoice_id = i.id
              AND p.status IN ('detected','merchant_confirmed'))
```

The second clause is the restart-safety bit, and it's the non-obvious one: an invoice
that already went `paid` still needs its confirmation counter driven all the way to
`FINAL_CONFIRMATIONS`, and that has to survive a process restart with an empty in-memory
`pending` map. Rebuilding the watch set from the DB every tick means there is no
in-memory state whose loss can strand a payment.

The join against `merchant_wallets` pulls the expected destination wallet, used as a
defense-in-depth check in the logs path (§4.6).

### 4.4 `tick_addresses` — the naive path

```
1. load_watched_invoices          → Vec<WatchedInvoice>
2. eth_blockNumber                → tip
3. build by_address multimap      → HashMap<address_lc, Vec<WatchedInvoice>>
4. load cursor (or cold start)
5. detect_fork_point → handle_reorg → rewind cursor      [if reorg]
6. for n in cursor+1 ..= target:
     a. eth_getBlockByNumber(n, full = true)
     b. parent-hash continuity check
     c. apply_block                → native transfers
     d. get_erc20_transfers(n)     → eth_getLogs, Transfer topic
     e. apply_erc20_transfers
     f. remember_block + save_cursor
7. refresh_confirmations(tip)
8. prune_seen_blocks
```

**Address → invoices is a multimap.** The same deposit address *can* legitimately appear
on two invoices (address reuse within a merchant), and a matching tx credits every
invoice on it.
**TODO:** once addresses are strictly single-use, collapse to a `HashMap` and hard-error
on duplicates instead of silently double-crediting.

**Cold start.** With no cursor, the floor is `MIN(created_block)` across all open
invoices, falling back to the current tip if there are none. Then `start - 1`, so the
first block actually processed is `start`. This makes a restart after downtime backfill
automatically rather than silently skipping the gap.

**Scan ceiling is `tip - 1`, not `tip`.** The newest block is the one most likely to have
reached some providers and not others, so scanning right up to the tip invites avoidable
quorum failures. This costs nothing in confirmation accuracy because
`refresh_confirmations` counts against the *live* tip independently (§4.7) — it is purely
about not fighting propagation lag inside the scan loop.

**Per-tick budget.** `target = min(tip - 1, last_block + MAX_BLOCKS_PER_TICK)`. A long
outage is caught up over many ticks instead of melting the RPC provider in one burst,
and the tip is re-read each tick so catch-up converges.

**Parent-hash continuity.** Before applying a block we check `block.parent_hash ==
last_hash`. A mismatch means a reorg landed mid-scan; we `break` out of the loop and let
the next tick's reorg detection handle it properly rather than trying to patch it up
inline.

**`get_block(n, full = true)`** pulls the transaction bodies, so native-value matching is
one round trip instead of N `eth_getTransactionByHash` calls. `parse_block` filters as it
goes: contract creations (`to: null`) are skipped, and zero-value txs are skipped because
ERC-20 transfers carry no native value and are handled by the log filter instead.

#### `apply_block` — native transfers

For each `(tx_hash, to, value)` landing on a watched address:

- **skip if `inv.token_lc.is_some()`** — a token invoice at this address must never be
  credited by a plain ETH transfer;
- skip if `block.number < inv.created_block` (predates the invoice, not our money);
- `INSERT … ON CONFLICT (invoice_id, tx_hash) DO NOTHING`:
    - **inserted** → this is a first sighting → enqueue `payment.detected` in the *same*
      DB transaction;
    - **conflict** → already known (rescan, or re-mined post-reorg) → `UPDATE` the location
      only, and un-orphan it if it was orphaned. **The amount is never rewritten.**
- commit, then `recompute_invoice_totals`.

#### ERC-20 to a deposit address

`get_erc20_transfers(block, to_addresses)` builds one `eth_getLogs` filter per block:

``` json
{
  "fromBlock": "0x…", "toBlock": "0x…",
  "topics": [ERC20_TRANSFER_TOPIC0, null, [<to-topics>]]
}
```

`address_to_topic` left-pads the address to a 32-byte word (indexed `address` topics are
right-aligned). The third topic position is an OR-set, so all watched addresses are
covered in one call.

Defensive filtering on each returned log:

| Check | Why |
|---|---|
| `!log.removed` | reorg-removed entry from a lagging provider; the reorg path owns it |
| `topics.len() == 3` | a standard `Transfer` has exactly 3; anything else either shares topic0 by coincidence or is a non-standard token, and is not safe to interpret |
| `data.len() >= 64` | malformed / non-`uint256` payload |
| `amount != 0` | some tokens emit zero-value `Transfer`s |

`apply_erc20_transfers` then credits **only** invoices whose `token_lc` matches the
emitting contract exactly. `token_lc == None` means a native invoice and is skipped
outright. Insert/conflict/webhook handling is identical to `apply_block`.

### 4.5 `tick_logs` — the WalletConnect path

```
1. load_watched_invoices
2. build by_id map                → HashMap<Uuid, WatchedInvoice>   (no multimap needed)
3. eth_blockNumber                → tip
4. load cursor (or cold start)
5. detect_fork_point_sparse → handle_reorg → rewind cursor          [if reorg]
6. chunked scan, MAX_LOG_BLOCK_RANGE blocks at a time:
     a. eth_getLogs(address = vault, topics = [payment_topic0()])
     b. fetch the chunk's LAST block header as an anchor  (after the logs)
     c. apply_payment_log for each log
     d. remember anchor + save_cursor
7. refresh_confirmations(tip)
8. prune_seen_blocks
```

Payment logs carry the invoice UUID natively, so this map is keyed by ID — there is no
address-collision problem to solve here.

`payment_topic0()` is a `OnceLock` reading `TOPIC_0` from the environment, falling back
to `DEFAULT_PAYMENT_TOPIC0`, lowercased. That makes redeploying the vault with a changed
event signature a config change.

**Anchors.** Unlike the address scanner, this path does not visit every block — it
queries ranges. So it can only remember **one header per chunk**: the header of the
chunk's last block. That header is fetched *after* the logs on purpose. If a reorg lands
between the two calls, the anchor won't match canonical on the next tick, and we rewind
and rescan — the ordering turns a race into a self-healing rescan rather than a missed
payment.

`MAX_LOG_BLOCK_RANGE` keeps each `eth_getLogs` inside provider limits; the outer
`MAX_BLOCKS_PER_TICK` budget still applies, so a large backfill is chunked twice.

### 4.6 Decoding the `Payment` event

```solidity
event Payment(
    address indexed merchant,
    address indexed token,
    bytes16 indexed identifier,   // UUIDv4, dashes stripped
    address payer,
    uint256 amountRequested,
    uint256 amountReceived,
    uint256 timestamp
);
```

| Slot | Content | Decoder |
|---|---|---|
| `topics[0]` | event signature | matched by the filter |
| `topics[1]` | merchant | `topic_to_address` |
| `topics[2]` | token | `topic_to_address` |
| `topics[3]` | invoice UUID | `topic_to_uuid` |
| `data[0]` | payer | right-aligned address word |
| `data[1]` | `amountRequested` | `hex_to_u128` |
| `data[2]` | `amountReceived` | `hex_to_u128` |
| `data[3]` | `timestamp` | unused |

**Alignment is the one thing that bites here.** An indexed `address` or `uint` is
**right**-aligned in its 32-byte word (`topic_to_address` takes `h[24..]`). An indexed
fixed-size `bytesN` is **left**-aligned / right-padded (`topic_to_uuid` takes
`h[..32]`, i.e. the first 16 bytes). They are opposites. Since the contract strips the
dashes from the UUIDv4, the first 16 bytes of the topic *are* the UUID.

A log with anything other than 4 topics, or short data, is rejected — someone else's
event that shares topic0, not ours.

#### `apply_payment_log` — validation before credit

| Check | Behaviour on failure |
|---|---|
| `log.removed` | return early; the reorg path owns removed entries |
| decodable | log and skip — foreign event sharing topic0 |
| `by_id` contains the UUID | silently skip — settled, expired, or another environment sharing the vault |
| `inv.merchant_wallet_lc == ev.merchant_lc` | log and ignore — wrong merchant credited |
| expected token matches `ev.token_lc` | log and ignore — paid in the wrong token |
| `block_number >= inv.created_block` | skip — predates the invoice |

For a native invoice (`token_lc == None`) the expected token is `NATIVE_TOKEN_SENTINEL`
(`address(0)`) — the same value `payNative()` emits.

**We credit `amountReceived`, not `amountRequested`.** This mirrors the vault's own
fee-on-transfer-safe accounting: for a fee-on-transfer token the vault records what
actually landed, and so do we. A delta between the two is logged as a note, and
`recompute_invoice_totals` lands the invoice on `paid` / `underpaid` / `overpaid`
accordingly.

**TODO:** `(invoice_id, tx_hash)` uniqueness collapses two `Payment` events for the
*same* invoice inside the *same* tx (e.g. a batching router) into a single credit.
Supporting that needs `log_index` in the unique key.

### 4.7 Confirmations

`refresh_confirmations` runs at the end of **both** ticks, against the current tip:

```
confirmations = GREATEST(0, tip - block_number + 1)
```

The including block itself counts as 1, so a payment in the tip block has 1 confirmation.

This is computed **off the DB**, not off the blocks just scanned. That is what lets an
invoice re-registered after a crash immediately pick up its real confirmation count
instead of restarting from zero.

Two thresholds:

| Threshold | Source | Promotes to | Webhook |
|---|---|---|---|
| `required_confirmations` | per invoice (defaults to `FINAL_CONFIRMATIONS`) | `merchant_confirmed` | `payment.confirmed` |
| `FINAL_CONFIRMATIONS` | global constant | `system_confirmed` | `payment.finalized` |

Both promotions are guarded `UPDATE`s whose `WHERE` clause includes the prior status.
That guard *is* the exactly-once latch: two workers racing on the same payment cannot
both see `rows_affected() == 1`, so only one enqueues the webhook.

Once a payment is `system_confirmed` it is no longer polled, and once an invoice has no
non-terminal payments left it is dropped from the in-memory `pending` map — the DB query
at the top of the tick already excludes it, this just stops the map growing forever.

**TODO:** `FINAL_CONFIRMATIONS` is global today. It should be per-chain — 48 blocks is
~10 min on Ethereum and ~2 min on Polygon, and you probably want considerably more on the
latter.

### 4.8 Reorg handling

Two detectors, because the two scanners have different block-history density:

| Detector | Used by | Assumption |
|---|---|---|
| `detect_fork_point` | address scanner | **dense** — we remembered every block, so we can demand a remembered hash at every height |
| `detect_fork_point_sparse` | logs scanner | **sparse** — only one anchor per getLogs chunk, so we walk back through the anchors we actually have |

Both start with the same fast path: re-fetch the cursor block; if its hash still matches
what we stored, there is no reorg and we return `None`.

Otherwise they walk backwards to `last_block - MAX_REORG_DEPTH` looking for the highest
height where our remembered hash still matches canonical. The dense detector treats a
missing remembered block as the fork point (pruned, or we cold-started above it) and
rescans forward from there. The sparse detector just skips to the next anchor.

If nothing survives inside the window, both clamp to the floor and rescan.
**TODO:** clamping is the wrong response here. A reorg deeper than `MAX_REORG_DEPTH`
means our assumptions about the chain are wrong — or the RPC set is serving a different
chain entirely. This should raise an operational alert, freeze the chain's payouts, and
surface on a health endpoint rather than silently rewinding.

#### `handle_reorg` — the two outcomes

Everything strictly above the fork point is untrustworthy. For each affected non-orphaned
payment, `locate_tx` asks the node where the tx is now:

| `locate_tx` result | Meaning | Action | Webhook? |
|---|---|---|---|
| `Some((Some(block), Some(hash)))` | still mined | update location, reset `confirmations = 0`, status back to `detected` | **no** |
| `Some((Some(_), None))` | mined-but-no-hash | orphan | **yes** |
| `Some((None, _))` | back in the mempool | orphan | **yes** |
| `None` | node has never heard of it — dropped | orphan | **yes** |

**Re-mining is not a merchant-visible event.** The money never went away, it just moved
blocks; `amount_received` is unchanged and only the confirmation countdown restarts.
`refresh_confirmations` recomputes from the tip on that very same tick, so nothing is
lost. If the payment had already been reported as `merchant_confirmed`, crossing the
threshold again re-emits `payment.confirmed`.

**Orphaning is the only reorg case that notifies the merchant**, because it is the only
one where funds they were told about are actually gone — anything they shipped on the
back of `payment.detected` / `payment.confirmed` needs walking back on their side.
**TODO:** skip this call once merchant webhook settings exist and the merchant has opted
out of orphaned notifications.

After each payment, `recompute_invoice_totals` rebuilds the invoice from its surviving
payments. It is a **full recompute, never a decrement** — that is what makes reorg replay,
duplicate ticks and rescans all converge on the same number no matter how many times they
run.

If an invoice falls back below the requested amount after a reorg, we log it but do not
emit a second event; `payment.orphaned` already told the merchant why.
**TODO:** add `payment.reverted` if merchants ask for it.

### 4.9 Address derivation (naive path)

`derive_address(mnemonic, index)`:

```
BIP-39 mnemonic ──▶ seed ──▶ BIP-32 derive(get_derivation_path(index))
                                  ──▶ secp256k1 secret key
                                  ──▶ uncompressed public key (65 bytes)
                                  ──▶ keccak256(pubkey[1..])          // drop 0x04 prefix
                                  ──▶ last 20 bytes ──▶ 0x… address
```

This is the address that goes in the QR and into `invoices.wallet_address`, and the key
whose funds later get swept to treasury.

### 4.10 EVM known limitations

| Limitation | Impact |
|---|---|
| `hex_to_u128` ceiling | any value above `u128` errors out. Same ceiling as the rest of the money pipeline. **TODO:** move to `u256` (see the `wei_to_decimal` TODO). |
| `FINAL_CONFIRMATIONS` is global | wrong for fast chains; needs to be per-chain |
| Deep reorg → clamp | should alert and freeze instead |
| Duplicate `Payment` events per tx | collapsed into one credit; needs `log_index` in the unique key |
| Address multimap | double-credits on address reuse by design; tighten once addresses are single-use |
| `REORG_WINDOW` const | declared on the impl but currently unused; `MAX_REORG_DEPTH` is what actually bounds the walk-back. **TODO:** remove it or wire it up. |
| Settlement trigger policy | `payment.finished` fires on the *amount* threshold regardless of confirmations. **TODO:** make it a merchant setting: `on_detected` (current), `on_confirmed`, `on_finalized`. |
| Underpaid tolerance | strictly `>=` today; no dust/rounding allowance. **TODO:** make configurable. |

---

## 5. Solana — TODO

> **Not implemented.** This section records intended design so the shape is fixed before
> the code exists.

### 5.1 Paths

- **Naive:** HD-derived deposit address per invoice, QR encodes the bare address. Swept
  to treasury afterwards.
- **Smart (wallet-connect only):** direct transfer to the merchant treasury with a **memo
  instruction** carrying the invoice identifier. No contract, no vault — Solana's memo
  program does what the EVM vault event does, at a fraction of the cost.

The memo path is **locked behind wallet-connect**. A memo typed by a human is worse than
no memo at all: it fails silently, it fails often, and it fails in a way that produces an
unattributable payment sitting in the treasury. The connected wallet builds the
instruction, so the memo is either correct or the transaction doesn't exist.

### 5.2 To specify

- [ ] **Detection mechanism.** `logsSubscribe` / `blockSubscribe` websocket vs polling
  `getSignaturesForAddress`. Websocket is cheaper but needs a resync path on
  disconnect — decide how the cursor survives a dropped subscription.
- [ ] **Cursor semantics.** Slots are not blocks: there are skipped slots, and slot
  numbers are not a dense sequence. `network_scan_state` stores `last_block` +
  `last_block_hash`; decide whether that maps to slot + blockhash or to a signature
  cursor, and whether `network_seen_blocks` is meaningful at all here.
- [ ] **Finality model.** `processed` / `confirmed` / `finalized` do not map onto a
  confirmation *count*. Decide whether `payments.confirmations` becomes a synthetic
  number derived from commitment level, or whether the schema grows a commitment
  column. This is the biggest single mismatch with the EVM assumptions baked into
  `refresh_confirmations`.
- [ ] **Reorg posture.** Reorgs below `finalized` are real but shallow. Does
  `handle_reorg`'s re-mined-vs-dropped distinction still apply, or is
  "not yet finalized" simply a different state?
- [ ] **SPL tokens.** Associated token accounts mean the "address" being watched is not
  the wallet address. Decide what goes in `invoices.wallet_address`, and how ATA
  creation (and its rent) is handled on the naive path.
- [ ] **Memo parsing.** Exact encoding of the identifier, and the same
  defense-in-depth validations the EVM logs path does (right merchant, right token,
  not before `created_block`).
- [ ] **Amount ceiling.** Lamports fit in `u64`, so the `u128` pipeline is not a
  constraint here — but SPL token amounts still need the same audit.
- [ ] **Derivation.** ed25519 / SLIP-0010, not secp256k1. `derive_address` has no shared
  code with the EVM implementation.

---

## 6. Esplora (Bitcoin-style UTXO) — TODO

> **Not implemented.**

### 6.1 Paths

- **Naive only.** There is no smart path. Bitcoin has no equivalent primitive that a
  connected wallet can use to attach an invoice identifier to a payment in a way that is
  both universally supported and reliably retrievable. `OP_RETURN` is not it — support is
  inconsistent across wallets and it changes the fee profile.

So Esplora is the network that justifies the whole "network-optimized paths" framing in
reverse: the orchestrator must be able to produce an invoice with **one** payment option
without any of the shared machinery assuming a second one exists.

### 6.2 To specify

- [ ] **Detection mechanism.** Esplora HTTP API: `/address/:addr/txs` polling vs
  block-scanning via `/block/:hash/txs`. Address-endpoint polling is far cheaper and
  is the obvious first implementation.
- [ ] **UTXO accounting.** A payment is a set of outputs, not a single value. Decide how
  `payments.amount` is populated — per-output rows, or one row per tx summing all
  outputs to the watched address. This is where the EVM-shaped `(invoice_id,
      tx_hash)` unique key needs the most scrutiny.
- [ ] **Mempool / 0-conf.** Esplora exposes unconfirmed txs. Decide whether a mempool
  sighting creates a `detected` row (consistent with EVM, where `detected` can mean
  0 confirmations) or whether we wait for a block.
- [ ] **RBF and double-spends.** The UTXO equivalent of an orphan, and more common than a
  reorg. `handle_reorg`'s "is this tx still there" logic maps reasonably well, but
  the trigger is different — needs its own path.
- [ ] **Reorg detection.** Block hashes and heights map cleanly onto
  `network_seen_blocks`, so `detect_fork_point` is largely portable. Confirm the
  Esplora API gives enough header history to do the walk-back.
- [ ] **Change addresses & the gap limit.** HD derivation is BIP-44/49/84 with an
  account/change/index structure. Decide the derivation scheme and the gap-limit
  policy for rescans.
- [ ] **Sweep + fee estimation.** UTXO consolidation cost is materially different from
  EVM's flat transfer cost. The sweep is not a solved problem copied from EVM.
- [ ] **Confirmation counting.** This one is genuinely the same as EVM: height-based,
  `tip - block_height + 1`.

---

## 7. Adding a new network

The contract a new `NetworkClient` has to satisfy:

1. **Derive an address** from the operator mnemonic + an index (naive path).
2. **Run at least one watcher service** that, given the DB pool, writes `payments` rows
   for money arriving against watched invoices.
3. **Be idempotent.** Re-running any tick over the same range must produce no new
   effects. In practice this means: `INSERT … ON CONFLICT DO NOTHING` on
   `(invoice_id, tx_hash)`, never rewriting an amount on conflict, and recomputing
   invoice totals rather than incrementing them.
4. **Enqueue webhooks in the same DB transaction as the state change** that justifies
   them, behind a guard that can only succeed once.
5. **Advance its cursor only on success**, so a failed tick is a no-op.
6. **Scope its scan state** so multiple services on one chain don't fight over one cursor.

What it does *not* have to do: use the same number of services, use blocks, use a
confirmation count, or support two payment paths. TRON, when it lands, is expected to
look nothing like EVM's fee model even though it will reuse the general shape — the plan
is to lean on whatever TRON gives us for energy/bandwidth optimization rather than
pretending it's Ethereum with different constants.

---

## 8. Correctness invariants

These hold across every network implementation, present and future. If a change breaks
one of them, it is a bug regardless of what else it fixes.

1. **`payments` is append-mostly.** An amount is written once, at insert. Location
   (`block_number`, `block_hash`) and status may be updated; the amount may not.
2. **Invoice totals are always a full recompute** from non-orphaned payments. Never a
   delta.
3. **Every webhook has a once-only latch** that is a DB constraint or a guarded `UPDATE`,
   not application logic.
4. **Cursors advance only after the work is committed.**
5. **An expired invoice is never resurrected** by a late payment recompute.
6. **Nothing durable lives only in memory.** The watch set is rebuilt from the DB every
   tick; the in-memory `pending` map is a hint, not state.
7. **Amounts are base units everywhere below the presentation layer.**