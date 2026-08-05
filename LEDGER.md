# Ledger, Movements & Fees

Companion to `README.md` and `NETWORKS.md`. Where `NETWORKS.md` describes how money is
*detected*, this document describes what happens to it afterwards: how on-chain activity is
recorded, how merchant balances are derived, and how operator fees accrue and settle.

> **Status: sketch.** This is a design document written before implementation, not a
> description of shipped code. It is a series of choices, and several of them will not
> survive contact with the implementation. Expect event names, journal kinds, account names,
> column names and status vocabularies to change; expect some of the logic — especially
> around sweep ordering, fee settlement policy and conversions — to change more
> substantially than that. What is intended to be stable is the *shape*: three layers,
> double-entry, append-only, recognition gated on finality. Treat everything below the
> section headings as provisional.

---

## 1. Design principle: three layers, one direction of truth

There are three distinct questions to answer, and conflating any two of them produces a
table that answers neither well:

| Layer                                | Question it answers                                     | Natural key                          | Mutability                                                               |
|--------------------------------------|---------------------------------------------------------|--------------------------------------|--------------------------------------------------------------------------|
| `chain_transactions`                 | What did we broadcast or observe, and what did it cost? | `(network_type, chain_ref, tx_hash)` | `status`, `block_number`, `block_hash` mutable; the rest fixed at insert |
| `chain_movements`                    | Which value moved from where to where?                  | `(tx_id, event_index)`               | append-only                                                              |
| `ledger_journals` + `ledger_entries` | Who owns what, and who owes whom?                       | journal `dedupe_key`                 | strictly append-only; corrections are reversals, never edits             |

Information flows in one direction only:

```
   chain  ──▶  chain_transactions  ──▶  chain_movements  ──▶  ledger
observed        (what happened)         (what moved)       (what it means)
```

The ledger never writes back down. Nothing in the chain layer is ever adjusted to make the
ledger balance.

### Why transactions and movements are separate tables

One transaction can produce many value transfers. A batched sweep is the obvious case: a
single vault call sweeps twelve deposit addresses, pays gas exactly once, and emits twelve
`Transfer` events. If gas lived on the movement row it would have to be either duplicated
across all twelve — making any `SUM(gas_paid)` wrong — or split by an invented pro-rata rule.

Gas is a property of the transaction. Value transfers are properties of the movements. The
same split covers UTXO transactions with multiple outputs and Solana transactions with
multiple instructions, and it is what makes a failed-but-gas-burning transaction
representable at all (§8).

### Why the ledger is double-entry

A custodial processor is, in accounting terms, an entity that **holds assets and owes
liabilities against them**. The same 50 USDC is simultaneously:

- an asset the operator physically controls, sitting at some address, and
- a debt owed to a specific merchant.

Single-entry running balances cannot express that one unit of value is both of these at
once, which means they cannot answer the two questions this system exists to answer
correctly at the same time: *"is the operator solvent?"* and *"what can this merchant
withdraw?"* Double-entry answers both from one book, and makes the answer self-checking — if
debits and credits do not sum to zero, something is wrong and it is loud.

The familiar "running balance per account" statement is still available; it is a window
function over `ledger_entries`, not a different data model (§10.1).

---

## 2. Schema

### 2.1 Chain layer

``` sql
CREATE TABLE chain_transactions (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),

    network_type  VARCHAR(20)  NOT NULL,        -- 'evm' | 'solana' | 'esplora'
    chain_ref     VARCHAR(50)  NOT NULL,        -- 'base' | 'polygon' | 'devnet' | cluster | …
    tx_hash       VARCHAR(255) NOT NULL,

    intent        VARCHAR(30)  NOT NULL,
        -- inbound | sweep | withdrawal | gas_refill | fee_settlement | conversion
    merchant_id   UUID REFERENCES merchants(id),   -- NULL only for operator-internal txs

    -- Gas / network fee. Its own token, because on most chains it is not the token moved.
    fee_token_id  VARCHAR(100),
    fee_paid      NUMERIC(78,0),
    fee_payer     VARCHAR(255),

    block_number  BIGINT,
    block_hash    VARCHAR(255),
    status        VARCHAR(20) NOT NULL,
        -- submitted | detected | confirmed | final | orphaned | failed

    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at    TIMESTAMPTZ NOT NULL DEFAULT now(),

    UNIQUE (network_type, chain_ref, tx_hash)
);
```

`tx_hash` alone is not unique — the same hash can legitimately exist on mainnet and on a
testnet, and `chain_ref` is what separates them. The unique constraint is the idempotency
latch for the chain layer, the same role the `(invoice_id, tx_hash)` index plays for
`payments`.

``` sql
CREATE TABLE chain_movements (
    id            UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tx_id         UUID NOT NULL REFERENCES chain_transactions(id),
    event_index   INT  NOT NULL,   -- log_index / vout / instruction index; 0 for a native transfer

    merchant_id   UUID REFERENCES merchants(id),
    invoice_id    UUID REFERENCES invoices(id),
    payment_id    UUID REFERENCES payments(id),   -- nullable: only inbound movements have one

    token_id      VARCHAR(100)  NOT NULL,
    amount        NUMERIC(78,0) NOT NULL CHECK (amount > 0),

    from_address  VARCHAR(255),
    from_kind     VARCHAR(20),
    to_address    VARCHAR(255),
    to_kind       VARCHAR(20),
        -- external | deposit_address | vault | merchant_main | gas | operator

    created_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tx_id, event_index)
);
```

`amount` is always positive; direction is carried by `from_*` / `to_*`. The `*_kind` columns
are denormalised classifications of the addresses — derivable by joining against `invoices`,
`merchant_wallets` and config, but that join is expensive and appears in almost every
dashboard query, so they are stored. They are also what makes "where are my unswept funds"
answerable without knowing anything about chains (§10.2).

Amounts are base units, consistent with the rest of the system.

### 2.2 Ledger layer

``` sql
CREATE TABLE ledger_accounts (
    id          UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    merchant_id UUID REFERENCES merchants(id),   -- NULL for operator / system accounts
    kind        VARCHAR(40)  NOT NULL,
    token_id    VARCHAR(100) NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (merchant_id, kind, token_id)
);
```

Accounts are `(merchant, kind, token)`. One per token, never a mixed-token account — summing
across tokens requires a price, and prices do not belong in the ledger.

``` sql
CREATE TABLE ledger_journals (
    id           UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    kind         VARCHAR(40)  NOT NULL,
    dedupe_key   VARCHAR(255) NOT NULL UNIQUE,

    merchant_id  UUID REFERENCES merchants(id),
    tx_id        UUID REFERENCES chain_transactions(id),
    payment_id   UUID REFERENCES payments(id),

    reverses     UUID REFERENCES ledger_journals(id),
    metadata     JSONB,        -- fee rate snapshot, policy version, oracle quote, …

    occurred_at  TIMESTAMPTZ NOT NULL,                 -- when the underlying event happened
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now()    -- when we recorded it
);

CREATE TABLE ledger_entries (
    entry_no    BIGSERIAL PRIMARY KEY,
    journal_id  UUID NOT NULL REFERENCES ledger_journals(id),
    account_id  UUID NOT NULL REFERENCES ledger_accounts(id),
    token_id    VARCHAR(100) NOT NULL,
    amount      NUMERIC(78,0) NOT NULL   -- signed: positive = debit, negative = credit
);
```

Two constraints carry most of the correctness weight:

**`dedupe_key` uniqueness.** Same latch pattern as `webhook_events`. Keys look like
`payment_recognized:<payment_id>`, `sweep:<tx_id>`, `fee_settlement:<tx_id>`,
`reversal:<original_journal_id>`. A worker that runs twice writes one journal. This is what
makes the recognition pass safely retryable.

**Balance per token, not per journal.** A deferred constraint trigger asserting
`SUM(amount) = 0` **grouped by `(journal_id, token_id)`**. Per-token is essential: a sweep
journal carries USDC legs and ETH gas legs together, and those two are not commensurable.
Requiring the whole journal to sum to zero would force a conversion that never happened.

`occurred_at` and `created_at` are deliberately distinct. Recognition happens when the worker
gets to it; the economic event happened at the block. Reports use `occurred_at`, operational
debugging `created_at`.

---

## 3. Chart of accounts

| Kind                  | Type      | Scope                   | Meaning                                                     |
|-----------------------|-----------|-------------------------|-------------------------------------------------------------|
| `custody_unswept`     | asset     | merchant + token        | Recognized funds still at a deposit address or in the vault |
| `custody_treasury`    | asset     | merchant + token        | Funds in the merchant's main wallet                         |
| `custody_gas`         | asset     | merchant + native token | The merchant's gas account for that network/chain           |
| `custody_operator`    | asset     | operator + token        | Where settled fees land                                     |
| `payable_to_merchant` | liability | merchant + token        | What the merchant can withdraw                              |
| `fees_receivable`     | asset     | merchant + token        | Fees accrued and not yet settled                            |
| `fee_revenue`         | revenue   | operator + token        | Recognized operator revenue                                 |

Sign convention: **positive = debit, negative = credit.** Assets carry a positive balance,
liabilities and revenue a negative one. `payable_to_merchant` reading `-150` means the
merchant is owed 150.

The custody accounts mirror `chain_movements.to_kind` deliberately — `custody_unswept` tracks
value at `deposit_address`/`vault`, `custody_treasury` at `merchant_main`. That
correspondence is what makes the reconciliation in §10.3 meaningful. It is a correspondence,
not an equality; the gap between them is itself a useful number.

There is no `custody_segregated` account. Segregation is a property of the address, and
therefore lives in the chain layer, not in a parallel set of ledger accounts.

---

## 4. Journal kinds

| Kind                 | Trigger                                      | `dedupe_key`                      |
|----------------------|----------------------------------------------|-----------------------------------|
| `payment_recognized` | payment reaches `system_confirmed`           | `payment_recognized:<payment_id>` |
| `sweep`              | sweep tx reaches `final`                     | `sweep:<tx_id>`                   |
| `withdrawal`         | withdrawal tx reaches `final`                | `withdrawal:<tx_id>`              |
| `gas_refill`         | refill tx reaches `final`                    | `gas_refill:<tx_id>`              |
| `gas_burn_failed`    | tx reaches `failed` with non-zero `fee_paid` | `gas_burn:<tx_id>`                |
| `conversion`         | conversion tx reaches `final`                | `conversion:<tx_id>`              |
| `fee_settlement`     | settlement tx reaches `final`                | `fee_settlement:<tx_id>`          |
| `reversal`           | a recognized tx is orphaned                  | `reversal:<original_journal_id>`  |

Every one of these is written in the same DB transaction as the state change that justifies
it, behind a latch that can only succeed once — the same discipline `NETWORKS.md` requires of
webhooks.

---

## 5. Recognition: when a payment enters the ledger

**A payment enters the ledger when it reaches `system_confirmed`, not before.**

`required_confirmations` (the merchant's threshold) drives *webhooks*. `FINAL_CONFIRMATIONS`
(the system's threshold) drives *the ledger*. They are different numbers answering different
questions: "can I ship the product?" versus "may I treat this as irreversible money?"

This produces three states a merchant sees, and they should be labelled distinctly in the
dashboard:

| Dashboard label       | Source                                                         | Meaning                        |
|-----------------------|----------------------------------------------------------------|--------------------------------|
| In progress           | `payments` where `status IN ('detected','merchant_confirmed')` | Seen on chain, not yet final   |
| Confirmed / available | `payable_to_merchant` balance                                  | In the ledger, withdrawable    |
| Unswept               | `chain_movements` position at deposit addresses (§10.2)        | Physically not yet in treasury |

"Unswept" cuts across the other two: some unswept funds are recognized, some are still in
progress. That is correct, and worth showing as such rather than flattening.

---

## 6. Fees

### 6.1 Accrual, not deduction

Fees are **never** taken out of the swept amount. Sweeping 49.5 and leaving 0.5 behind as the
operator's cut strands dust at an address where the gas to collect it may exceed its value —
the fee is destroyed by the mechanism meant to collect it.

Instead, receiving 50 at a 1% fee produces a *receivable*: the merchant's balance is 50, and
separately they owe 0.5. Coins stay whole; the obligation is tracked in the ledger and
settled in bulk later.

This is not fully collateralised, and that is a deliberate, accepted trade-off. In operator
mode merchants can read their own keys, so a determined merchant can drain their wallets
through an external wallet app and skip settlement. The mitigations are commercial rather
than cryptographic: the operator holds the record of what is owed, sets the settlement
threshold per merchant, and can suspend the account. The model is closer to an ad platform's
billing threshold than to escrow — thresholds start low, rise with trust, and non-payment
stops service.

### 6.2 Rate configuration

Rates are **per `token_id`**, not per physical token. `USDC_BASE_HANDLER_A` and
`USDC_BASE_HANDLER_B` may point at the same contract and still carry different rates, because
the token ID is what the rest of the system already resolves against — it is the unit the
orchestrator, the handlers and the invoices all speak in.

``` sql
CREATE TABLE fee_rates (
    id             UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    merchant_id    UUID REFERENCES merchants(id),   -- NULL = operator default
    token_id       VARCHAR(100) NOT NULL,
    basis_points   INT NOT NULL CHECK (basis_points >= 0),
    effective_from TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (merchant_id, token_id, effective_from)
);
```

Resolution: merchant-specific override, else operator default, taking the latest row with
`effective_from <= payment.occurred_at`. Rates are versioned rather than mutated, so
recomputing a historical accrual gives the historical answer.

The resolved rate is **snapshotted into `ledger_journals.metadata` at accrual time**. The
journal is the record of what was charged; the config table only says what would be charged
now.

Rounding is fixed at accrual time and stored, since base-unit percentages rarely divide
evenly. Round half-up on the fee and let the merchant balance absorb the remainder.

### 6.3 Settlement trigger

Fees accrue continuously into `fees_receivable` and settle in batches. The trigger is
operator-configurable, and the natural expression is a USD threshold rather than a per-token
one:

> settle when a merchant's total outstanding fees exceed $20

Evaluating that means crossing token boundaries, which the reaper does through the token
handler:

``` rust
token_handler.get_usd_value(amount, token_id) -> Option<Decimal>
```

Real handlers consult whatever price source they are configured with; test handlers return
whatever the test demands. This keeps price lookup behind the same trait boundary as
everything else chain-shaped, and keeps prices out of the ledger — a USD figure is only ever
used to *decide whether to act*, never written as a ledger amount.

The USD threshold also solves fee-collection economics for free. Settling 400 units of a
memecoin whose total value is $0.30 costs more gas than it collects. The reaper therefore
evaluates in two stages:

1. **Trigger.** Sum `get_usd_value` across all of the merchant's `fees_receivable` balances.
   Below the operator's threshold, do nothing.
2. **Selection.** Once triggered, settle only those tokens whose individual USD value clears
   a dust floor. Everything below it stays accrued and rides along to the next settlement.

Fee settlement is itself an on-chain transaction: it produces a `chain_transaction`, its own
`chain_movements`, and a `fee_settlement` journal — and it burns gas from the merchant's gas
account like every other transaction touching their funds.

Other trigger modes (fixed schedule, at-withdrawal, deduct-on-every-payment until cleared)
are the same query with a different predicate. Which combination is offered, and whether
"whichever fires first" is the default, is operator config.

### 6.4 Who pays gas

The merchant does, in every transaction touching their funds: sweeps, conversions,
withdrawals, and fee settlements. The system is custodial, but funds and costs stay strictly
per-merchant — the operator does not front gas, and there is no shared gas pool whose costs
would have to be allocated across merchants. A merchant's accounts are only ever touched by
that merchant's activity, plus settlement of what they owe.

Each merchant has a gas account per network/chain, currently refilled manually. Gas spent is
recorded as a leg in the gas token, reducing `custody_gas` and reducing
`payable_to_merchant` **in that same token**. It is never converted into the payment token.

---

## 7. Worked examples

Following one sequence: three payments of 50 USDC on Base at a 1% fee, then a sweep, then a
fee settlement, then a withdrawal. `m` is the merchant, `op` the operator.

### 7.1 Inbound payment reaching `system_confirmed`

**`chain_transactions`**

| id     | network | chain_ref | tx_hash  | intent  | merchant | fee_token | fee_paid | status |
|--------|---------|-----------|----------|---------|----------|-----------|----------|--------|
| `tx-1` | evm     | base      | `0xaaa…` | inbound | m        | —         | —        | final  |

Gas is null: the payer paid it, not the merchant.

**`chain_movements`**

| tx_id  | idx | payment_id | token_id    | amount   | from_kind | to_kind         | to_address |
|--------|-----|------------|-------------|----------|-----------|-----------------|------------|
| `tx-1` | 12  | `pay-1`    | `USDC_BASE` | 50000000 | external  | deposit_address | `0xdep1…`  |

**`ledger_journals`** — kind `payment_recognized`, dedupe `payment_recognized:pay-1`,
metadata `{"fee_bps": 100, "rate_source": "merchant_override"}`

**`ledger_entries`**

| account                  | token     | amount      |
|--------------------------|-----------|-------------|
| `custody_unswept[m]`     | USDC_BASE | `+50000000` |
| `payable_to_merchant[m]` | USDC_BASE | `-50000000` |
| `fees_receivable[m]`     | USDC_BASE | `+500000`   |
| `fee_revenue[op]`        | USDC_BASE | `-500000`   |

Sums to zero within USDC_BASE. After three such payments the merchant is owed 150 USDC, owes
1.5 USDC in fees, and 150 USDC sits unswept on chain. Nothing has been shaved off anything.

### 7.2 Sweep, batching all three deposit addresses

Gas price crossed the merchant's configured threshold, so the sweeper fired.

**`chain_transactions`**

| id     | intent | merchant | fee_token  | fee_paid           | status |
|--------|--------|----------|------------|--------------------|--------|
| `tx-4` | sweep  | m        | `ETH_BASE` | `3000000000000000` | final  |

**`chain_movements`** — one row per address swept, one transaction

| tx_id  | idx | payment_id | token_id  | amount   | from_kind       | to_kind       |
|--------|-----|------------|-----------|----------|-----------------|---------------|
| `tx-4` | 0   | —          | USDC_BASE | 50000000 | deposit_address | merchant_main |
| `tx-4` | 1   | —          | USDC_BASE | 50000000 | deposit_address | merchant_main |
| `tx-4` | 2   | —          | USDC_BASE | 50000000 | deposit_address | merchant_main |

**`ledger_entries`** — kind `sweep`, dedupe `sweep:tx-4`

| account                  | token     | amount              |
|--------------------------|-----------|---------------------|
| `custody_unswept[m]`     | USDC_BASE | `-150000000`        |
| `custody_treasury[m]`    | USDC_BASE | `+150000000`        |
| `custody_gas[m]`         | ETH_BASE  | `-3000000000000000` |
| `payable_to_merchant[m]` | ETH_BASE  | `+3000000000000000` |

Two tokens, one journal, each summing to zero independently. The USDC legs relocate value
without changing what is owed; the ETH legs reduce both the gas asset and the ETH liability,
because gas is spent from the merchant's own funds.

Note what did *not* happen: the merchant's USDC balance is still exactly 150. Gas did not come
out of it. On a chain where the payment token and the gas token are the same (native BTC,
native ETH) the legs collapse into one token and the balance does drop — but that is a
property of that chain, not a general rule, and it must not be generalised into one.

### 7.3 Fee settlement

The reaper runs: `fees_receivable` totals 1.5 USDC ≈ $1.50, below the $20 threshold. Nothing
happens. Some weeks and many payments later the total reaches $21.40 — 20.80 in USDC_BASE and
0.60 in a low-value token below the dust floor. Only USDC settles.

**`chain_transactions`**

| id     | intent         | merchant | fee_token | fee_paid          | status |
|--------|----------------|----------|-----------|-------------------|--------|
| `tx-9` | fee_settlement | m        | ETH_BASE  | `900000000000000` | final  |

**`chain_movements`**

| tx_id  | idx | token_id  | amount   | from_kind     | to_kind  |
|--------|-----|-----------|----------|---------------|----------|
| `tx-9` | 0   | USDC_BASE | 20800000 | merchant_main | operator |

**`ledger_entries`** — kind `fee_settlement`, metadata carries the USD quotes used to decide

| account                  | token     | amount             |
|--------------------------|-----------|--------------------|
| `custody_treasury[m]`    | USDC_BASE | `-20800000`        |
| `custody_operator[op]`   | USDC_BASE | `+20800000`        |
| `payable_to_merchant[m]` | USDC_BASE | `+20800000`        |
| `fees_receivable[m]`     | USDC_BASE | `-20800000`        |
| `custody_gas[m]`         | ETH_BASE  | `-900000000000000` |
| `payable_to_merchant[m]` | ETH_BASE  | `+900000000000000` |

The receivable is extinguished, the liability drops by the same amount, and the value has
physically moved to the operator. The sub-dust-floor token keeps its accrued balance for next
time.

### 7.4 Withdrawal

The merchant withdraws 100 USDC to an address they control outside the system.

| account                  | token     | amount              |
|--------------------------|-----------|---------------------|
| `custody_treasury[m]`    | USDC_BASE | `-100000000`        |
| `payable_to_merchant[m]` | USDC_BASE | `+100000000`        |
| `custody_gas[m]`         | ETH_BASE  | `-1200000000000000` |
| `payable_to_merchant[m]` | ETH_BASE  | `+1200000000000000` |

The corresponding movement has `to_kind = 'external'`. This is the point at which funds leave
custody, and the ledger says so: the asset and the liability shrink together.

---

## 8. Failed transactions that still burn gas

A reverted EVM transaction still consumes gas. The chain layer records it as a normal
transaction with `status = 'failed'` and a populated `fee_paid`; movements are simply absent,
because no value moved.

The ledger records a `gas_burn_failed` journal with **gas legs only, no value legs**:

| account                  | token    | amount             |
|--------------------------|----------|--------------------|
| `custody_gas[m]`         | ETH_BASE | `-400000000000000` |
| `payable_to_merchant[m]` | ETH_BASE | `+400000000000000` |

The merchant is genuinely poorer by the gas, and no value legs are warranted. This is a second
reason the balance constraint is grouped per token: a journal touching only the gas token is
perfectly valid and must not be forced to reference the payment token it failed to move.

---

## 9. Ordering hazards and reversals

### 9.1 Sweeping funds that are not yet recognized

The realistic failure: a deposit address holds one finalized payment and one at three
confirmations, the gas-price trigger fires, and the sweeper moves the *address balance* —
both of them. The sweep journal then debits `custody_unswept` for value that was never
credited there, driving it negative.

The intended fix is a `swept_by_tx_id UUID NULL` column on `payments`, set when a sweep
includes that payment's funds. Recognition then branches:

- funds still at the deposit address → debit `custody_unswept`, as normal
- already moved by a sweep → debit `custody_treasury` directly, and write no sweep leg

The sweep journal only ever covers payments already recognized at sweep time; late-recognized
payments arrive straight into treasury. This keeps both invariants without making the sweeper
wait for finality — which matters, because the entire point of gas-price-triggered sweeping is
to move opportunistically when gas is cheap, not when confirmations happen to be ready.

### 9.2 Reorg after recognition

Rare at `FINAL_CONFIRMATIONS`, but "rare" combined with "append-only" means the path has to
exist before it is needed.

A `reversal` journal is written with `reverses` pointing at the original and every leg
negated. The original is never edited or deleted. If the merchant already withdrew against the
reversed funds, `payable_to_merchant` goes negative — they are overdrawn. That is the honest
representation, and it converts an accounting problem into a collections problem, which is the
correct place for it to live.

---

## 10. Deriving balances

### 10.1 Ledger balances (ownership)

``` sql
SELECT a.kind, a.token_id, SUM(e.amount) AS balance
FROM ledger_entries e
JOIN ledger_accounts a ON a.id = e.account_id
WHERE a.merchant_id = $1
GROUP BY a.kind, a.token_id;
```

Withdrawable in token *T* is `-balance(payable_to_merchant[T])`, optionally net of
`fees_receivable[T]` depending on operator policy. Both framings read the same two numbers, so
switching between "net fees off the displayed balance" and "show owed separately and block
withdrawal above a threshold" is a policy change, not a migration. Keeping it that way is
deliberate.

The per-account running statement, if wanted:

``` sql
SELECT e.entry_no, j.kind, e.amount,
       SUM(e.amount) OVER (ORDER BY e.entry_no) AS running_balance
FROM ledger_entries e
JOIN ledger_journals j ON j.id = e.journal_id
WHERE e.account_id = $1
ORDER BY e.entry_no;
```

### 10.2 Physical position (location)

"How much USDC is unswept, and across how many addresses" comes from `chain_movements`, not
the ledger. This is forced rather than preferred: the ledger only knows about finalized
payments, so a payment at 2/12 confirmations is physically sitting in an HD address and
entirely invisible to the ledger. A dashboard figure that excluded it would not match what a
block explorer shows for that address.

``` sql
WITH deltas AS (
    SELECT m.to_address AS address, m.to_kind AS kind, m.token_id, m.merchant_id,
           m.amount AS delta
    FROM chain_movements m JOIN chain_transactions t ON t.id = m.tx_id
    WHERE t.status NOT IN ('orphaned','failed')
    UNION ALL
    SELECT m.from_address, m.from_kind, m.token_id, m.merchant_id, -m.amount
    FROM chain_movements m JOIN chain_transactions t ON t.id = m.tx_id
    WHERE t.status NOT IN ('orphaned','failed')
)
SELECT token_id, kind,
       COUNT(*) FILTER (WHERE bal > 0) AS addresses,
       SUM(bal) AS total
FROM (
    SELECT address, kind, token_id, SUM(delta) AS bal
    FROM deltas WHERE merchant_id = $1
    GROUP BY address, kind, token_id
) x
WHERE bal <> 0
GROUP BY token_id, kind;
```

Filtering `kind IN ('deposit_address','vault')` gives the unswept figure directly, per token,
with an address count — which also feeds the sweep policy, since "spread thin across 40
addresses" and "concentrated in 2" are very different gas propositions for the same total.

### 10.3 Reconciliation

`custody_unswept` (ledger) and the deposit-address position (chain) are expected to *differ*,
and the difference is meaningful:

```
chain position at deposit addresses  −  custody_unswept balance
    =  value detected but not yet recognized
```

That number should always be ≥ 0, and should always be explainable by the set of payments
currently in `detected` / `merchant_confirmed`. If it is negative, or non-zero with no
in-flight payments, something is wrong — most likely §9.1. This is the cheapest and most
valuable monitoring check in the system, and it is worth having before the first sweep runs in
anger.

Balances are computed on demand for now. Materialised or snapshot balances are a performance
answer to be applied when the query is measurably slow, not before.

---

## 11. Invariants

Alongside those in `NETWORKS.md` §8. If a change breaks one of these it is a bug regardless of
what else it fixes.

1. `ledger_entries` are **never updated or deleted**. Corrections are reversal journals.
2. Every journal sums to zero **per `(journal_id, token_id)`**.
3. Every journal has a `dedupe_key` backed by a DB uniqueness constraint, not application
   logic.
4. A payment enters the ledger exactly once, at `system_confirmed`, never before.
5. Amounts are base units everywhere below the presentation layer. No mixed-token amounts, and
   no fiat amounts in the ledger at all — USD is used to *decide*, never to *record*.
6. Gas is recorded in the gas token. It is never netted against a payment token without an
   explicit `conversion` journal recording the rate.
7. `chain_movements.amount > 0` always; direction is carried by `from_*` / `to_*`.
8. The ledger never writes back into the chain layer.
9. Fees accrue as receivables and are settled by explicit transactions. Nothing is ever
   silently deducted from a swept or withdrawn amount.
10. Ledger history is never destroyed by any deletion path (§12).

---

## 12. Deletion and retention

Merchant deletion is **TBD in detail**, but one part is settled: it is a flag plus removal of
personally identifiable fields — name, contact details, credentials, webhook URLs and secrets
— and explicitly **not** a row deletion, and explicitly **not** any removal of ledger,
journal, movement or transaction history.

Financial records of value that actually moved through custody are exactly the records a
custodian is obliged to retain, and a deleted ledger cannot be reconstructed from the chain
alone: fee accruals, receivables, reversals and the recognition timeline have no on-chain
representation. The existing `ON DELETE CASCADE` foreign keys on `merchants` therefore need
revisiting before any deletion flow ships — appropriate for the current early-stage tables,
actively wrong for the ledger.

Which fields count as PII, retention periods, and how a deleted merchant's accounts appear in
operator reporting are open. `COMPLIANCE.md` is the place for the jurisdictional side.

---

## 13. TBD

Known-open, roughly ordered by how much each would change the shape above.

**Conversions and pricing.** Non-native-token sweeps may need converting some value to the
native token first to pay gas, when the gas account is empty. That is a `conversion` journal
with legs in two tokens and a rate snapshot in `metadata` — never an implicit haircut on a
balance. Slippage, failure mid-conversion, and which venue the conversion routes through are
unspecified. Related: once conversions exist, "how much is this merchant up in USD" becomes a
reporting question needing an oracle and a chosen valuation basis. That belongs in a reporting
layer, firmly outside the ledger.

**Automated gas refills.** Currently manual. The intended shape is a per-network/chain policy
that converts from allowed tokens when the gas balance drops below a threshold, but the policy
structure and its interaction with sweep triggers is not designed.

**Refunds.** Not modelled. The leaning is non-refundable at the platform level, leaving refunds
to the merchant's own process. If modelled later it needs a refund journal kind plus a
withdrawal-shaped outbound transaction, and a decision on what happens to fees already accrued
against the refunded amount — reverse them, keep them, or make it configurable.

**Fee settlement modes.** The USD-threshold reaper is specified above; fixed schedule,
at-withdrawal, and deduct-on-every-payment-until-cleared are named but not designed, nor is
"whichever fires first".

**Price source semantics.** `get_usd_value` is specified as a handler method but its staleness
tolerance, failure behaviour (does settlement halt if the price source is down?), and whether
quotes are cached per reaper run are all open.

**Withdrawal request lifecycle.** §7.4 covers a completed withdrawal. A requested,
pending-approval, or failed withdrawal has no representation yet — probably a
`withdrawal_requests` table upstream of the chain layer, with the journal written only on
finality, mirroring recognition.

**Multi-token invoices.** Out of scope. One invoice, one token, one amount. If that ever
changes, the per-token fee accrual model survives but invoice-level totals do not.

**Operator-level accounts.** `custody_operator` and `fee_revenue` are sketched as
merchant-null accounts in the same book. Whether operator P&L should be a separate book, and
how the operator's own withdrawals of settled revenue are recorded, is not worked out.

**Partial sweeps.** All examples assume a sweep moves the full address balance. Partial sweeps
(leaving a UTXO, or sweeping under a cap) are representable in the movement layer but interact
awkwardly with §9.1's recognition branching.

**Ordering across chains.** `entry_no` gives a total order within the ledger, but journals from
different chains arrive in wall-clock order, not block order. `occurred_at` exists for this and
is probably sufficient; it has not been stress-tested against a reorg on one chain concurrent
with normal activity on another.

**Table growth.** `chain_movements` grows without bound and is append-only. Partitioning by
`created_at` or by chain is the obvious answer; not needed yet, and cheaper to add before the
table is large.
