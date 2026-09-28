# Alert Rules Reference

Rules are evaluated per-transaction for each watched contract.
Multiple rules can match the same transaction — each fires an independent webhook call.
A rule evaluation error is logged as a warning and skipped; it never stops the engine.

## Common rule options

Every `[[contracts.rules]]` entry supports these optional fields regardless of rule type:

| Field          | Type    | Default | Description |
|----------------|---------|---------|-------------|
| `enabled`      | bool    | `true`  | Set `false` to silence a rule without removing it. Disabled rules are skipped in evaluation and shown as `(disabled)` in `txwatch validate`. |
| `webhook_url`  | string  | unset   | Override the contract-level `webhook_url` for this rule only. Same URL validation as the contract level. |
| `webhook_secret` | string | unset  | Override the contract-level `webhook_secret` for this rule only. |
| `severity`     | string  | unset   | One of `info`, `warning`, `critical`. Included as `severity` in the alert payload; not included when unset. |

Example — silence a noisy rule temporarily and route a critical one to PagerDuty:

```toml
[[contracts.rules]]
type    = "AnyTransaction"
enabled = false              # quiet for now; config history is preserved

[[contracts.rules]]
type         = "AdminFunctionCalled"
function_names = ["set_admin", "upgrade"]
webhook_url  = "https://pagerduty.example.com/alert"
severity     = "critical"
```

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

### `SourceAccount`

| Field   | Type       | Required | Description |
|---------|------------|----------|-------------|
| `allow` | [G-address] | no      | If set, only fire when the transaction source account is one of these addresses. |
| `deny`  | [G-address] | no      | If set, fire when the transaction source account is one of these addresses. |

At least one of `allow` or `deny` must be non-empty. Each address must be a valid 56-character Stellar G-address.

Matches when the transaction `source_account` satisfies both:
- It is in `allow` (if `allow` is non-empty), **and**
- It is not in `deny` (if `deny` is non-empty).

**Use case:** alert when an admin function is called by an unexpected account, or watch a specific counterparty.

```toml
# Fire only when called by the known multisig
[[contracts.rules]]
type  = "SourceAccount"
allow = ["GAAZI4TCR3TY5OJHCTJC2A4QSY6CJWJH5IAJTGKIN2ER7LBNVKOCCWN"]

# Fire when called by any account except the approved bot
[[contracts.rules]]
type = "SourceAccount"
deny = ["GBUKOFF2GVFNQRJFGHVGON2JKGB4VBUF2QQJFNJ6HQVTJRZTCGQX7ZR"]
```

The `source_account` field is included in the alert payload when this rule fires (and whenever `source_account` is present in the Horizon response).

### `All`

| Field   | Type              | Required | Description |
|---------|-------------------|----------|-------------|
| `rules` | [rule entry array] | yes      | Non-empty list of nested rule entries. All must match. |

Fires when **all** nested rules match the same transaction. Equivalent to a logical AND.
Each nested entry supports the same options as a top-level rule entry (`enabled`, `webhook_url`, `severity`, …).
Nesting is allowed up to depth 5.

The `rule_triggered` label in the payload uses the readable form, e.g.  
`All(FunctionCalled(withdraw), LargeTransfer(>=10000XLM))`.

```toml
[[contracts.rules]]
type = "All"
[[contracts.rules.rules]]
type          = "FunctionCalled"
function_name = "withdraw"
[[contracts.rules.rules]]
type          = "LargeTransfer"
threshold_xlm = 10000
```

### `Any`

| Field   | Type              | Required | Description |
|---------|-------------------|----------|-------------|
| `rules` | [rule entry array] | yes      | Non-empty list of nested rule entries. At least one must match. |

Fires when **any** nested rule matches. Equivalent to a logical OR.

```toml
[[contracts.rules]]
type = "Any"
[[contracts.rules.rules]]
type = "TransactionFailed"
[[contracts.rules.rules]]
type          = "HighFee"
threshold_xlm = 1
```

### `Not`

| Field  | Type       | Required | Description |
|--------|------------|----------|-------------|
| `rule` | rule entry | yes      | A single nested rule entry. Fires when it does NOT match. |

Fires when the nested rule does **not** match. Equivalent to a logical NOT.

```toml
[[contracts.rules]]
type = "Not"
[contracts.rules.rule]
type = "TransactionFailed"
```

## Evaluation order

Rules are evaluated in the order they appear in the config file.
Disabled rules (``enabled = false``) are skipped entirely.
All matching enabled rules fire; there is no short-circuit.

## Webhook payload fields

| Field              | Type        | Always present | Description                              |
|--------------------|-------------|----------------|------------------------------------------|
| `label`            | string      | yes            | Contract label from config               |
| `contract_id`      | string      | yes            | Stellar C-address                        |
| `network`          | string      | yes            | `mainnet` / `testnet` / `futurenet`      |
| `rule_triggered`   | string      | yes            | Human-readable rule description          |
| `transaction_hash` | string      | yes            | Stellar transaction hash                 |
| `function_name`    | string/null | no             | Soroban function name if available; `null` indicates a non-Soroban transaction |
| `function_names`   | [string]    | yes            | All invoked function names (may be empty) |
| `amount_xlm`       | u64/null    | no             | Transfer amount in XLM if available      |
| `fee_charged_stroops` | u64/null | no            | Fee charged in stroops                   |
| `source_account`   | string/null | no             | Transaction source account (G-address); omitted when not present |
| `severity`         | string/null | no             | `info`, `warning`, or `critical`; omitted when unset on the rule |
| `timestamp`        | i64         | yes            | Unix timestamp (seconds) of transaction  |
| `timestamp_iso`    | string      | yes            | ISO 8601 timestamp of the transaction    |
| `horizon_link`     | string      | yes            | Direct link to transaction on Horizon    |
| `explorer_link`    | string      | yes            | Stellar Expert explorer link for the transaction |

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
| `SourceAccount` | `"SourceAccount"` |
| `All` | `"All"` |
| `Any` | `"Any"` |
| `Not` | `"Not"` |

## Adding a new rule type

1. Add a variant to `AlertRule` in `crates/config/src/lib.rs`
2. Add field validation in `AlertRule::validate_at_depth()` in the same file
3. Add the match arm in `eval_rule()` in `crates/rules/src/lib.rs`
4. Add the label string in `rule_label()` in the same file
5. Add a stable `rule_type` string in `rule_type()` in the same file
6. Add unit tests in `crates/rules/src/lib.rs`
7. Update the rule type table in this section
8. Update the webhook payload example in README.md (if adding a new example)

No other crates need changes.
