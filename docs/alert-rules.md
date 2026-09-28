# Alert Rules Reference

Rules are evaluated per-transaction for each watched contract.
Multiple rules can match the same transaction — each fires an independent webhook call.
A rule evaluation error is logged as a warning and skipped; it never stops the engine.

## Rule types

### `AnyTransaction`
Matches every transaction that appears in the contract's Horizon history.

**Use case:** full audit trail, low-volume contracts.

```toml
[[contracts.rules]]
type = "AnyTransaction"
```

### `TransactionFailed`
Matches transactions where `successful = false`.

**Use case:** detect reverted Soroban invocations or fee-bump failures.

```toml
[[contracts.rules]]
type = "TransactionFailed"
```

### `LargeTransfer`

| Field           | Type | Required | Description                        |
|-----------------|------|----------|------------------------------------|
| `threshold_xlm` | u64  | yes      | Minimum transfer amount in XLM (> 0) |

Matches when the payment amount (extracted from Horizon operations) is ≥ `threshold_xlm` XLM.
The `amount_xlm` field in the webhook payload contains the actual transferred amount.

**Note:** Amount is extracted from `payment` operation records. Soroban token transfers
that do not produce a native `payment` operation will not populate `amount_xlm`.

```toml
[[contracts.rules]]
type          = "LargeTransfer"
threshold_xlm = 10000
```

### `FunctionCalled`

| Field           | Type   | Required | Description                          |
|-----------------|--------|----------|--------------------------------------|
| `function_name` | string | yes      | Exact function name (case-sensitive) |

Matches when the Soroban `invoke_host_function` operation calls exactly `function_name`.

```toml
[[contracts.rules]]
type          = "FunctionCalled"
function_name = "withdraw"
```

### `AdminFunctionCalled`

| Field            | Type     | Required | Description                              |
|------------------|----------|----------|------------------------------------------|
| `function_names` | [string] | yes      | Non-empty list of function names to watch |

Matches when the invoked function is any entry in `function_names`.
Equivalent to multiple `FunctionCalled` rules but produces a single
`AdminFunctionCalled([...])` label in the alert.

```toml
[[contracts.rules]]
type           = "AdminFunctionCalled"
function_names = ["set_admin", "upgrade", "initialize"]
```

### `HighFee`

| Field                | Type | Required        | Description                                   |
|----------------------|------|-----------------|-----------------------------------------------|
| `threshold_stroops`  | u64  | one of the two  | Fee threshold in stroops (> 0)                |
| `threshold_xlm`      | u64  | one of the two  | Fee threshold in whole XLM (> 0)              |

Matches when the transaction's total fee is greater than or equal to the threshold.
The `fee_charged_stroops` field in the webhook payload contains the actual fee paid in stroops.

Set exactly one of `threshold_stroops` or `threshold_xlm`; they are mutually exclusive and
setting both is a validation error. `threshold_xlm` is converted to stroops during validation.

**Note:** Stroops are the smallest unit of XLM (1 XLM = 10,000,000 stroops).

```toml
[[contracts.rules]]
type               = "HighFee"
threshold_stroops  = 100000

# or, equivalently for a 1 XLM threshold:
[[contracts.rules]]
type          = "HighFee"
threshold_xlm = 1
```

### `EventEmitted`

| Field    | Type     | Required | Description                                                         |
|----------|----------|----------|---------------------------------------------------------------------|
| `topic`  | string   | yes      | Symbol that event topic 0 must equal exactly (valid Soroban symbol) |
| `topics` | [string] | no       | Patterns for topics 1, 2, … in order; `"*"` matches any value       |

Matches when the transaction emitted a contract event whose first topic is the symbol `topic`
and whose following topics match `topics` positionally. A topic value matches a pattern when:

- it is a single-key scalar `ScVal` (`{"symbol": "x"}`, `{"address": "G…"}`, `{"u32": 5}`,
  `{"i128": "-1"}`, …) and the inner value's text equals the pattern, or
- otherwise, its compact JSON equals the pattern.

The matching events (topics and data, as decoded `ScVal` JSON) are included in the
`matched_events` payload field.

**Use case:** react to what a contract reports it did — `transfer`, `mint`, `admin_changed`, …

**Note:** Events come from Soroban RPC `getEvents` (Horizon does not expose them), so the
contract needs a Soroban RPC endpoint: `soroban_rpc_url` on the contract, the custom network's
`rpc_url`, or the testnet/futurenet default. Mainnet has no default endpoint. Events older than
the RPC retention window (about 7 days on public endpoints) cannot be fetched and will not match.
Events are only fetched for contracts that have at least one `EventEmitted` rule.

```toml
[[contracts.rules]]
type   = "EventEmitted"
topic  = "transfer"

[[contracts.rules]]
type   = "EventEmitted"
topic  = "transfer"
topics = ["*", "GDESTINATION..."]   # any sender, to this address
```

## Cooldowns

Every rule accepts an optional `cooldown_seconds` (0–604800):

```toml
[[contracts.rules]]
type             = "TransactionFailed"
cooldown_seconds = 300
```

After the rule fires for a contract, further matches of the same (contract, rule) within
`cooldown_seconds` are suppressed and counted. The next alert that is sent for that rule carries
the number of suppressed matches in `suppressed_count`. Unset or `0` disables the cooldown.

Cooldown state is kept in memory for the life of `txwatch watch`; it resets on restart and does
not carry over between `txwatch watch --once` runs.

## Evaluation order

Rules are evaluated in the order they appear in the config file.
All matching rules fire; there is no short-circuit.

## Webhook payload fields

| Field              | Type        | Always present | Description                              |
|--------------------|-------------|----------------|------------------------------------------|
| `label`            | string      | yes            | Contract label from config               |
| `contract_id`      | string      | yes            | Stellar C-address                        |
| `network`          | string      | yes            | `mainnet` / `testnet` / `futurenet`      |
| `rule_triggered`   | string      | yes            | Human-readable rule description          |
| `transaction_hash` | string      | yes            | Stellar transaction hash                 |
| `function_name`    | string/null | no             | Soroban function name if available; `null` indicates a non-Soroban transaction |
| `amount_xlm`       | u64/null    | no             | Transfer amount in whole XLM (truncated). Kept for backward compatibility — use `amount_xlm_decimal` for precise accounting |
| `amount_stroops`   | u64/null    | no             | Raw transfer amount in stroops (1 XLM = 10,000,000 stroops), or `null` |
| `amount_xlm_decimal` | string/null | no           | Transfer amount as a decimal string with 7 fractional digits (e.g. `"9999.9900000"`), or `null` |
| `timestamp`        | i64         | yes            | Unix timestamp (seconds) of transaction  |
| `horizon_link`     | string      | yes            | Direct link to transaction on Horizon    |
| `explorer_link`    | string      | yes            | Stellar Expert explorer link for the transaction |
| `fee_charged_stroops` | u64/null | no             | Fee charged for the transaction in stroops |
| `matched_events`   | array       | yes            | Events that matched an `EventEmitted` rule (`contract_id`, `topics`, `data`); empty for other rules |
| `suppressed_count` | u64         | yes            | Matches suppressed by this rule's `cooldown_seconds` since the previous alert; `0` otherwise |

> `horizon_link` and `explorer_link` are always present in every alert payload, even when `function_name` is `null` for a non-Soroban transaction.

## Stable rule_type values

The webhook payload includes two rule-related fields:

| Field | Purpose | Example |
|-------|---------|---------|
| `rule_type` | Machine-readable, stable rule variant name; use for programmatic routing | `"LargeTransfer"` |
| `rule_triggered` | Human-readable description with parameters; use for display | `"LargeTransfer(>=10000XLM)"` |

### Rule type table

| Rule | `rule_type` value |
|------|-------------------|
| `AnyTransaction` | `"AnyTransaction"` |
| `TransactionFailed` | `"TransactionFailed"` |
| `LargeTransfer` | `"LargeTransfer"` |
| `FunctionCalled` | `"FunctionCalled"` |
| `AdminFunctionCalled` | `"AdminFunctionCalled"` |
| `HighFee` | `"HighFee"` |
| `EventEmitted` | `"EventEmitted"` |

## Adding a new rule type

1. Add a variant to `AlertRule` in `crates/config/src/lib.rs`
2. Add field validation in `AlertRule::validate()` in the same file
3. Add the match arm in `AlertRule::label()` in the same file
4. Add the match arm in `AlertRule::rule_type()` in the same file
5. Add the match arm in `eval_rule()` in `crates/rules/src/lib.rs`
6. Add unit tests in `crates/rules/src/lib.rs`
7. Update the rule type table in this section
8. Update the webhook payload example in README.md (if adding a new example)

**Note:** `rule_triggered` and `rule_type` in webhook payloads are now produced by `AlertRule::label()` and `AlertRule::rule_type()` from `txwatch-config`. There are no duplicate implementations in `txwatch-rules`. A single change to `AlertRule::label()` is reflected consistently in both CLI `validate` output and webhook payloads.
