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

| Field           | Type   | Required | Default   | Description                              |
|-----------------|--------|----------|-----------|------------------------------------------|
| `function_name` | string | yes      | —         | Pattern to match against the invoked function name |
| `match`         | string | no       | `"exact"` | Matching mode: `"exact"`, `"prefix"`, or `"glob"` |

Matches when the Soroban `invoke_host_function` operation calls a function that satisfies the
match condition. `"exact"` (the default) requires an identical name; `"prefix"` requires the
invoked name to start with `function_name`; `"glob"` matches using `*` (any sequence) and `?`
(exactly one character).

```toml
# Exact match (default — backward compatible)
[[contracts.rules]]
type          = "FunctionCalled"
function_name = "withdraw"

# Prefix match: fires on admin_set_fee, admin_pause, admin_upgrade, …
[[contracts.rules]]
type          = "FunctionCalled"
function_name = "admin_"
match         = "prefix"

# Glob match: fires on set_fee, set_admin, set_pause, …
[[contracts.rules]]
type          = "FunctionCalled"
function_name = "set_*"
match         = "glob"
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

| Field                | Type | Required | Description                           |
|----------------------|------|----------|---------------------------------------|
| `threshold_stroops`  | u64  | yes      | Fee threshold in stroops (> 0)        |

Matches when the transaction's total fee exceeds `threshold_stroops`.
The `fee_charged` field in the webhook payload contains the actual fee paid in stroops.

**Note:** Stroops are the smallest unit of XLM (1 XLM = 10,000,000 stroops).

```toml
[[contracts.rules]]
type               = "HighFee"
threshold_stroops  = 100000
```

## Evaluation order

Rules are evaluated in the order they appear in the config file.
All matching rules fire; there is no short-circuit.

## Webhook payload fields

| Field                | Type        | Always present | Description                              |
|----------------------|-------------|----------------|------------------------------------------|
| `schema_version`     | u32         | yes            | Payload shape version (currently `1`); bump on breaking changes |
| `alert_id`           | string      | yes            | Deterministic 32-hex-char ID for deduplication (see below) |
| `label`              | string      | yes            | Contract label from config               |
| `contract_id`        | string      | yes            | Stellar C-address                        |
| `network`            | string      | yes            | `mainnet` / `testnet` / `futurenet`      |
| `rule_type`          | string      | yes            | Stable machine-readable rule variant     |
| `rule_triggered`     | string      | yes            | Human-readable rule description          |
| `transaction_hash`   | string      | yes            | Stellar transaction hash                 |
| `function_name`      | string/null | no             | First Soroban function name; `null` for non-Soroban transactions |
| `function_names`     | [string]    | yes            | All Soroban function names in the transaction |
| `amount_xlm`         | u64/null    | no             | Transfer amount in XLM if available      |
| `fee_charged_stroops`| u64/null    | no             | Transaction fee in stroops               |
| `timestamp`          | i64         | yes            | Unix timestamp (seconds) of transaction  |
| `timestamp_iso`      | string      | yes            | ISO 8601 timestamp string                |
| `horizon_link`       | string      | yes            | Direct link to transaction on Horizon    |
| `explorer_link`      | string      | yes            | Stellar Expert explorer link             |
| `ledger`             | u32/null    | no             | Ledger sequence number (when available)  |
| `source_account`     | string/null | no             | Source account G-address (when available)|
| `memo`               | string/null | no             | Memo content (absent for `MemoNone`)     |
| `memo_type`          | string/null | no             | Memo type: `"none"`, `"text"`, `"id"`, `"hash"`, `"return"` |
| `operation_count`    | u32/null    | no             | Total operations in the transaction      |

> `horizon_link` and `explorer_link` are always present in every alert payload, even when `function_name` is `null` for a non-Soroban transaction.

### `alert_id` and deduplication

`alert_id` is derived deterministically from `(network, contract_id, tx_hash, rule_type, rule_triggered)`
via SHA-256 (first 16 bytes → 32 hex chars). The same alert always produces the same `alert_id`,
so receivers can safely deduplicate retries and cursor replays by storing and checking this value.
It is also sent as the `X-TxWatch-Alert-Id` request header, enabling deduplication without parsing
the JSON body.

### Schema compatibility policy

`schema_version` is currently `1`. The versioning policy:
- **Additive changes** (new optional fields added to the payload) keep the same version.
- **Breaking changes** (field removals, renames, or type changes) bump the version.

Receivers should read `schema_version` before processing other fields to detect incompatible
format changes in stored or queued payloads.

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

## Adding a new rule type

1. Add a variant to `AlertRule` in `crates/config/src/lib.rs`
2. Add field validation in `AlertRule::validate()` in the same file
3. Add the match arm in `eval_rule()` in `crates/rules/src/lib.rs`
4. Add the label string in `rule_label()` in the same file
5. Add a stable `rule_type` string in `rule_type()` in the same file
6. Add unit tests in `crates/rules/src/lib.rs`
7. Update the rule type table in this section
8. Update the webhook payload example in README.md (if adding a new example)

No other crates need changes.
