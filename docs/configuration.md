# Configuration Reference

Config is a TOML file passed via `--config`, or the `TXWATCH_CONFIG` environment variable, defaulting to
`./txwatch.toml`. TxWatch exits with an error if the file does not exist.

`txwatch validate` (and startup) reports every validation error at once, one per line, rather than stopping
at the first.

While `txwatch watch` is running, `SIGHUP` re-reads and validates the file. A valid config is applied in place:
contracts that remain keep their cursors, and new contracts start from their `cursor_file` entry or `now`. An
invalid config is logged and the previous one keeps running. HTTP pool settings only change on restart.

## Editor validation

The committed [`txwatch-config.schema.json`](txwatch-config.schema.json) describes the supported configuration shape. In Taplo or Even Better TOML, add this directive at the top of a TOML file to enable completion and inline validation:

```toml
#:schema ../docs/txwatch-config.schema.json
```

The schema is also available from the CLI with `txwatch schema`. CI verifies that the committed schema remains synchronized with the derived Rust model.

## Top-level fields

| Field                         | Type            | Required | Default | Description |
|-------------------------------|-----------------|----------|---------|-------------|
| `poll_interval_seconds`       | u64             | no       | `10`    | How often to poll Horizon (seconds). Must be ≥ 5 and ≤ 3600. Each contract can override it (see below). |
| `contracts`                   | array of tables | yes      | —       | The `[[contracts]]` entries (see below). At least one is required; labels must be unique (case-insensitive). |
| `cursor_file`                 | string (path)   | no       | unset   | JSON file used to persist the per-contract cursor map. Loaded on startup and rewritten after each poll cycle. When unset, cursors start at Horizon's `now` and are not persisted. A missing or unparsable file falls back to `now`. |
| `http_pool_max_idle_per_host` | usize           | no       | `10`    | Maximum idle connections kept per host in the HTTP pool. Must be 1–100. Lower values use less memory; higher values help with many contracts. |
| `http_tcp_keepalive_secs`     | u64             | no       | `30`    | TCP keepalive interval (seconds) for pooled HTTP connections. Must be ≤ 7200; `0` disables keepalive. |
| `http_connection_verbose`     | bool            | no       | `false` | Reserved for HTTP connection-pool debug output. Accepted by the parser but currently has no effect. |

Unknown top-level keys are rejected.

> **Horizon rate limits:** Polling too frequently across many contracts can exhaust Horizon's per-IP request quota,
> resulting in `429 Too Many Requests` responses. Sustained polling across six or more contracts at intervals below
> 10 seconds is known to trigger rate limiting in production. The recommended minimum is
> `poll_interval_seconds = 10`; for high-volume deployments with many contracts, `poll_interval_seconds = 30` or
> higher is advised. TxWatch logs a startup warning when more than 5 contracts are polled at an effective
> interval below 10 seconds.

> **Contract limit:** `txwatch-config` declares `MAX_CONTRACTS = 100` as the supported upper bound for `[[contracts]]` entries. It is not yet enforced during validation, so keep configurations at or below 100 contracts to avoid exhausting memory or file descriptors with too many concurrent Horizon polling tasks.

## `[[contracts]]`

Each entry defines one watched Soroban contract. At least one entry is required.

| Field            | Type            | Required | Description |
|------------------|-----------------|----------|-------------|
| `label`          | string          | yes      | Human-readable name shown in logs and alert payloads. Surrounding whitespace is trimmed. Must not be blank, contain control characters (newlines, ANSI escapes, …) or exceed 128 characters; must be unique across contracts, ignoring case. |
| `contract_id`    | string          | yes      | Stellar C-address (56 chars, starts with `C`). |
| `network`        | string or table | yes      | `mainnet`, `testnet`, `futurenet`, or a custom network table (see below). |
| `rules`          | array of tables | yes      | The `[[contracts.rules]]` entries (see below). At least one is required. |
| `webhook_url`    | string          | yes      | `http://` or `https://` URL with a host that receives the alert JSON. |
| `poll_interval_seconds` | u64      | no       | Polls this contract at its own interval instead of the top-level `poll_interval_seconds`. Same bounds (5–3600). Contracts are scheduled independently; `txwatch validate` prints each contract's effective interval. |
| `webhook_secret` | string          | no       | When set, every webhook POST carries `X-TxWatch-Signature: sha256=<hex HMAC-SHA256 of the body>` **and** the raw secret in `X-TxWatch-Secret`. Supports `${ENV_VAR}` interpolation (e.g. `webhook_secret = "${MY_SECRET}"`); an unset variable is a startup error. |

Unknown keys inside a `[[contracts]]` entry are rejected.

### Network field values

Valid `network` values and their corresponding Horizon endpoints:

| Value | Horizon URL |
|---|---|
| `mainnet` | https://horizon.stellar.org |
| `testnet` | https://horizon-testnet.stellar.org |
| `futurenet` | https://horizon-futurenet.stellar.org |

Any value outside this list will cause a TOML parse error. For example:

```
Error: unknown variant `main`, expected one of `mainnet`, `testnet`, `futurenet`
```

To fix: replace your `network` value with one of the valid values listed above.

### Custom / local networks

For `stellar/quickstart --local` or a private network, give `network` an inline table instead of a name:

```toml
network = { horizon_url = "http://localhost:8000", passphrase = "Standalone Network ; February 2017" }
```

| Field          | Required | Description |
|----------------|----------|-------------|
| `horizon_url`  | yes      | `http://` or `https://` Horizon base URL. |
| `explorer_url` | no       | Explorer base URL; alert `explorer_link` becomes `<explorer_url>/tx/<hash>`. Without it, `explorer_link` is the transaction's Horizon URL. |
| `passphrase`   | no       | Network passphrase, for reference. |

Alert payloads and logs report such contracts with `network = "custom"`. See `docker-compose.local.yml` and
`config/local.toml` for a ready-made quickstart + TxWatch setup.

## `[[contracts.rules]]`

At least one rule is required per contract. All matching rules fire independently.

### `AnyTransaction`
Fires on every transaction that appears in the contract's Horizon history.

```toml
[[contracts.rules]]
type = "AnyTransaction"
```

### `TransactionFailed`
Fires when `successful = false`.

```toml
[[contracts.rules]]
type = "TransactionFailed"
```

### `LargeTransfer`
Fires when the payment amount ≥ `threshold_xlm` XLM.

```toml
[[contracts.rules]]
type          = "LargeTransfer"
threshold_xlm = 10000          # must be > 0
```

### `FunctionCalled`
Fires when the Soroban invocation calls exactly `function_name` (case-sensitive).
Function names must be valid Soroban symbols: at most 32 characters from `[a-zA-Z0-9_]`
(no spaces, hyphens or surrounding whitespace). The same applies to `AdminFunctionCalled`.

```toml
[[contracts.rules]]
type          = "FunctionCalled"
function_name = "withdraw"
```

### `AdminFunctionCalled`
Fires when the invoked function is any entry in `function_names`.

```toml
[[contracts.rules]]
type           = "AdminFunctionCalled"
function_names = ["set_admin", "upgrade", "initialize"]
```

### `HighFee`
Fires when the transaction's charged fee is at least the threshold. Set exactly one of
`threshold_stroops` (raw stroops, must be > 0) or `threshold_xlm` (whole XLM, must be > 0;
converted to stroops during validation). Setting both is rejected.

```toml
[[contracts.rules]]
type              = "HighFee"
threshold_stroops = 1000000
```

## Webhook payload

```json
{
  "label":               "My Escrow Contract",
  "contract_id":         "CAAA...",
  "network":             "testnet",
  "rule_type":           "LargeTransfer",
  "rule_triggered":      "LargeTransfer(>=10000XLM)",
  "transaction_hash":    "abc123...",
  "function_name":       "transfer",
  "function_names":      ["transfer"],
  "amount_xlm":          15000,
  "fee_charged_stroops": 50000,
  "timestamp":           1705316096,
  "timestamp_iso":       "2024-01-15T12:00:00Z",
  "horizon_link":        "https://horizon-testnet.stellar.org/transactions/abc123...",
  "explorer_link":       "https://stellar.expert/explorer/testnet/tx/abc123..."
}
```

This example and the one in the README are checked against `AlertPayload` by
`crates/rules/tests/docs_payload.rs`, so they cannot drift from the code.

- `rule_type` — stable machine-readable rule variant (e.g. `"LargeTransfer"`); use it for routing.
- `rule_triggered` — human-readable rule description including parameters.
- `amount_xlm` — whole-XLM transfer amount, or `null` when the transaction has none.
- `fee_charged_stroops` — fee charged for the transaction in stroops, or `null` if unknown.
- `timestamp` / `timestamp_iso` — ledger close time as Unix seconds and as an ISO 8601 string.
- `function_name` — the first invoked Soroban function name (present for backward compatibility).
- `function_names` — all Soroban function names invoked in the transaction (one per `invoke_host_function` operation). Most transactions have zero or one entry.

## Environment variables

| Variable   | Default | Description                                      |
|------------|---------|--------------------------------------------------|
| `RUST_LOG` | `info`  | Log level: `error`, `warn`, `info`, `debug`, `trace` |

## Pre-flight checks

`txwatch validate --check-webhooks` checks all endpoints concurrently. It tries `HEAD` first; when a receiver returns `405 Method Not Allowed` or `501 Not Implemented`, TxWatch retries with `OPTIONS`. A per-URL table reports `reachable`, `reachable (OPTIONS)`, `method not allowed`, or `unreachable`, and any unreachable endpoint makes validation exit non-zero. Some serverless receivers reject both probe methods; use a real test webhook for those endpoints.

`txwatch validate --check-horizon` checks Horizon reachability, prints the latest ledger reported by each network, and verifies every configured contract exists on that network. A missing contract is reported as `not found on <network>` and exits non-zero.

Note: setting `RUST_LOG=debug` will show per-contract idle poll cycles — the
poller emits `"no new transactions"` debug logs with the contract `label` and
current `cursor` when a poll returns an empty page.

## Full example

```toml
poll_interval_seconds = 10

[[contracts]]
label       = "My Escrow Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
network     = "testnet"
webhook_url = "https://hooks.example.com/my-webhook"

  [[contracts.rules]]
  type          = "LargeTransfer"
  threshold_xlm = 10000

  [[contracts.rules]]
  type           = "AdminFunctionCalled"
  function_names = ["set_admin", "upgrade", "initialize"]

  [[contracts.rules]]
  type = "TransactionFailed"
```
