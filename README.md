# tx-watch-core

> Real-time Soroban smart contract monitoring and webhook alert engine for the Stellar network.

Part of the [TxWatch](https://github.com/Tx-wats) ecosystem.

---

## What is this?

**TxWatch** sits between the [Stellar Horizon REST API](https://developers.stellar.org/api/horizon)
and your infrastructure. It polls every contract you configure, evaluates alert rules against
each new transaction, and fires a JSON webhook the moment a condition is met — no SDK, no
subscriptions, no infrastructure beyond a single Rust binary.

```
  Stellar Network
       │
       ▼
  Horizon REST API          ← TxWatch polls this
  (testnet / mainnet /
   futurenet)
       │
       ▼
  txwatch-poller            ← fetches /accounts/{contract}/transactions
       │                       fetches /transactions/{hash}/operations
       ▼
  txwatch-rules             ← evaluates AlertRules against each transaction
       │
       ▼
  txwatch-notifier          ← POSTs AlertPayload JSON to your webhook URL
       │
       ▼
  Your webhook receiver     ← Slack, PagerDuty, custom API, etc.
```

---

## Stellar / Soroban primer

| Concept | What it means here |
|---|---|
| **Stellar** | Layer-1 blockchain with fast finality (~5 s) and low fees |
| **Soroban** | Stellar's smart contract platform (WebAssembly-based) |
| **Horizon** | The REST API gateway to the Stellar network — TxWatch's data source |
| **Contract address** | A 56-character string starting with `C` (e.g. `CABC...`) |
| **XLM** | Stellar's native asset; 1 XLM = 10,000,000 stroops |
| **Stroop** | Smallest unit of XLM (like satoshi for Bitcoin) |
| **Paging token** | Horizon cursor used to fetch only new transactions since last poll |
| **invoke_host_function** | The Horizon operation type for a Soroban contract call |

### Horizon endpoints used

| Endpoint | Purpose |
|---|---|
| `GET /accounts/{contract_id}/transactions?cursor=…&order=asc` | Fetch new transactions for a contract |
| `GET /transactions/{hash}/operations` | Fetch operations to extract function name and payment amount |

### Network base URLs

| Network | Horizon base URL |
|---|---|
| Mainnet | `https://horizon.stellar.org` |
| Testnet | `https://horizon-testnet.stellar.org` |
| Futurenet | `https://horizon-futurenet.stellar.org` |
| Custom / local | Your own `horizon_url`, e.g. `http://localhost:8000` for `stellar/quickstart --local` |

---

## Quickstart

```bash
# 1. Clone
git clone https://github.com/Tx-wats/core
cd tx-watch-core

# 2. Copy and edit the example config (./txwatch.toml is the default path)
cp config/example.toml txwatch.toml
$EDITOR txwatch.toml

# 3. Validate your config
cargo run -p txwatch -- validate

# 4. Send a test webhook to confirm your receiver works
cargo run -p txwatch -- test-webhook --url https://hooks.example.com/my-webhook

# 5. Start watching
cargo run -p txwatch -- watch
```

To pick up config changes without restarting (and without losing cursors),
send `SIGHUP`: `kill -HUP <pid>`. An invalid file is logged and ignored.

Set `RUST_LOG=debug` for verbose output. This also enables per-contract idle
poll logs — the poller emits `"no new transactions"` debug messages with the
contract label and current cursor when a poll finds nothing new.

---

## Docker

Build the image:

```bash
docker build -t txwatch .
```

Run with a config file (the image reads `TXWATCH_CONFIG=/config/txwatch.toml`):

```bash
docker run -v $(pwd)/txwatch.toml:/config/txwatch.toml txwatch watch
```

Or validate your config:

```bash
docker run -v $(pwd)/txwatch.toml:/config/txwatch.toml txwatch validate
```

To watch contracts on a local standalone network, `docker compose -f docker-compose.local.yml up`
starts `stellar/quickstart --local` together with TxWatch using [`config/local.toml`](config/local.toml).

For local development with webhook testing, see [Local Development with Docker Compose](#local-development-with-docker-compose) in [CONTRIBUTING.md](CONTRIBUTING.md).

---

## CLI

```
txwatch [--config <path>] <command>

Commands:
  watch                        Start the polling engine
  validate                     Validate the config file and print a summary
  test-webhook --url <URL>     Send a test payload to a webhook URL and exit
  init                         Write a starter config (prompts for anything not passed)
  completions <shell>          Print a completion script (bash, zsh, fish, powershell, elvish)
  man                          Print the txwatch(1) man page
```

Install completions with e.g. `txwatch completions bash > ~/.local/share/bash-completion/completions/txwatch`
or `txwatch completions zsh > "${fpath[1]}/_txwatch"`, and the man page with
`txwatch man > ~/.local/share/man/man1/txwatch.1`. Release archives also ship a
`completions-and-man` bundle with both pre-generated.

Start a new config with `txwatch init --contract-id C... --network testnet --webhook-url https://...`
(add `--output <path>`, default `txwatch.toml`). Any value not passed as a flag is prompted for.
The generated file is validated before it's written, and an existing file is only replaced with
`--force`.

`--config` defaults to `config/example.toml`.
  watch [--once] [--dry-run]     Start the polling engine
  validate [--format text|json]  Validate the config file and print a summary
  test-webhook --url <URL>       Send a test payload to a webhook URL and exit
  replay --contract <label> --tx <hash> [--send]
                                 Evaluate a contract's rules against one transaction
```

`replay` fetches a historical transaction and its operations from Horizon, runs the named
contract's rules against it and prints every matched rule with its webhook payload, so rule authors
can check "would my rules have fired for transaction X?". Nothing is sent unless `--send` is given.

`validate --format json` prints the parsed config as a single JSON object (webhook secrets are
redacted to `webhook_secret_set`), or `{"valid": false, "error": "..."}` with exit code 1.

`--config` defaults to `config/example.toml`. `--horizon-url <URL>` overrides the Horizon base URL
for every contract (for example a private Horizon instance).

`watch --once` runs a single poll cycle, delivers any alerts, saves cursors to `cursor_file` (if
configured) and exits, for use from cron, CI jobs or serverless schedulers. It exits `1` if any
contract poll or webhook delivery failed, `0` otherwise. Set `cursor_file` so each run picks up
where the previous one stopped; without it every run starts from `now`.
The config path comes from `--config`, else the `TXWATCH_CONFIG` environment variable, else
`./txwatch.toml`. TxWatch exits with an error if that file does not exist.

---

## Config

See [docs/configuration.md](docs/configuration.md) for the full reference.

```toml
poll_interval_seconds = 10

[[contracts]]
label       = "My Escrow Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4"
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

---

## Alert rules

| Rule | Triggers when… |
|---|---|
| `AnyTransaction` | Any transaction touches the contract |
| `TransactionFailed` | A transaction fails (`successful = false`) |
| `LargeTransfer` | Payment amount ≥ `threshold_xlm` XLM |
| `FunctionCalled` | A specific Soroban function is invoked |
| `AdminFunctionCalled` | Any function in a named list is invoked |
| `HighFee` | Transaction fee exceeds configured threshold |

See [docs/alert-rules.md](docs/alert-rules.md) for full details.

---

## Webhook payload

```json
{
  "label":            "My Escrow Contract",
  "contract_id":      "CAAA...",
  "network":          "testnet",
  "rule_type":        "LargeTransfer",
  "rule_triggered":   "LargeTransfer(>=10000XLM)",
  "transaction_hash": "abc123...",
  "function_name":    "transfer",
  "function_names":   ["transfer"],
  "amount_xlm":       15000,
  "fee_charged_stroops": 50000,
  "timestamp":        1705316096,
  "timestamp_iso":    "2024-01-15T12:00:00Z",
  "horizon_link":     "https://horizon-testnet.stellar.org/transactions/abc123...",
  "explorer_link":    "https://stellar.expert/explorer/testnet/tx/abc123..."
}
```

**Webhook headers:**
- `Content-Type: application/json`
- `Content-Length: <length of JSON body in bytes>`
- `X-TxWatch-Version: <package version>`
- `X-TxWatch-Signature: sha256=<hmac>` (optional, only when `webhook_secret` is configured — HMAC-SHA256 of the request body)
- `X-TxWatch-Secret: <webhook_secret>` (optional, only when `webhook_secret` is configured — the raw secret; verify the signature instead where possible)

**Fields:**
- `rule_type` — stable machine-readable rule variant (e.g. `"LargeTransfer"`, `"HighFee"`); use this for programmatic routing
- `rule_triggered` — human-readable rule description with parameters (e.g. `"LargeTransfer(>=10000XLM)"`); use this for display
- `function_name` — the first invoked Soroban function name, or `null` for non-Soroban transactions.
- `function_names` — all invoked Soroban function names in the transaction (may contain multiple entries for multi-op transactions).
- `horizon_link` — direct Horizon REST API URL for the transaction (e.g. `https://horizon-testnet.stellar.org/transactions/<hash>`); useful for fetching raw XDR or operation details programmatically.
- `explorer_link` — Stellar Expert web explorer URL for the transaction (e.g. `https://stellar.expert/explorer/testnet/tx/<hash>`); useful for human-readable inspection in a browser.

> **`horizon_link` vs `explorer_link`**: `horizon_link` points to the Horizon REST API endpoint and returns raw JSON — useful for programmatic access. `explorer_link` points to the [Stellar Expert](https://stellar.expert) web UI — useful for human inspection. Both are always present for every alert regardless of rule type.

---

## Architecture

```
txwatch (cli binary)
  │
  ├── txwatch-config      TOML parsing · contract ID validation · rule validation
  │
  └── txwatch-poller      Horizon polling loop · cursor tracking · op enrichment
        │
        ├── txwatch-rules     AlertRule evaluation · AlertPayload construction
        │
        └── txwatch-notifier  Webhook POST · 3-attempt exponential backoff · tracing logs
```

### Crate responsibilities

| Crate | Responsibility |
|---|---|
| `txwatch-config` | Parse `config.toml` into typed structs; validate all fields |
| `txwatch-rules` | Pure rule evaluation — no I/O, fully unit-testable |
| `txwatch-notifier` | HTTP webhook delivery with retry; timestamped structured logs |
| `txwatch-poller` | Horizon REST client; cursor map; per-transaction error isolation |
| `txwatch` (cli) | `clap` binary; tracing init; subcommand dispatch |

---

## Tracing and observability

TxWatch uses `tracing` spans to correlate work across each poll cycle and webhook delivery.
- `txwatch-poller::poll_contract` is instrumented with `contract`, `contract_id`, and `network` fields.
- `txwatch-poller::fetch_soroban_details` is instrumented with the transaction hash.
- `txwatch-notifier::send_webhook` creates a span with `contract` and `rule` fields before sending the webhook request.

Set `RUST_LOG=info` or a more specific filter to view structured tracing output in the CLI.

Logs are human-readable text by default. For log pipelines (Loki, Datadog, CloudWatch), pass
`--log-format json` or set `TXWATCH_LOG_FORMAT=json` to emit one JSON object per line. Event
fields (`contract`, `tx`, `rule`, `attempt`, ...) stay as JSON fields, and each line includes the
current span and the full span list:

```sh
txwatch --log-format json watch
TXWATCH_LOG_FORMAT=json txwatch watch
```

### Prometheus metrics (optional)

Build the `txwatch` binary with the `metrics` feature and pass `--metrics-addr` to `watch`:

```bash
cargo build --release -p txwatch --features metrics
./target/release/txwatch --config config.toml watch --metrics-addr 127.0.0.1:9090
```

This serves `GET /metrics` (Prometheus text format), `GET /healthz` (process alive) and
`GET /readyz` (200 once a poll has succeeded recently, else 503). Exported series include
`txwatch_transactions_total`, `txwatch_alerts_total` and `txwatch_webhook_failures_total`
(labelled by `contract` and `network`), Horizon request and webhook delivery latency histograms,
per-contract freshness gauges and `txwatch_build_info`. Without the feature (the default build),
the `--metrics-addr` flag doesn't exist.

Library users can call `txwatch_poller::serve_metrics(addr, shutdown)` themselves before
`run_with_shutdown`, with `txwatch-poller`'s `metrics` feature enabled.

The endpoint can be scraped by any Prometheus-compatible monitoring stack (Prometheus, Grafana
Agent, VictoriaMetrics, etc.).

Example scrape config:

```yaml
scrape_configs:
  - job_name: txwatch
    static_configs:
      - targets: ['localhost:9090']
```

---

## How a transaction flows through TxWatch

```
1. Poller wakes up (every poll_interval_seconds)
2. For each contract:
   a. GET /accounts/{contract_id}/transactions?cursor={last_seen}&order=asc
   b. For each new transaction:
      i.  Advance cursor (even if enrichment fails)
      ii. GET /transactions/{hash}/operations
          → extract function_name from invoke_host_function ops
          → extract amount_stroops from payment ops
      iii. Build EnrichedTransaction
      iv.  Evaluate all AlertRules → Vec<AlertPayload>
      v.   POST each AlertPayload to webhook_url (retry up to 3×)
```

---

## Stellar testnet resources

| Resource | URL |
|---|---|
| Testnet Horizon | https://horizon-testnet.stellar.org |
| Stellar Expert (testnet) | https://stellar.expert/explorer/testnet |
| Stellar Laboratory | https://laboratory.stellar.org |
| Friendbot (fund testnet accounts) | https://friendbot.stellar.org |
| Soroban docs | https://developers.stellar.org/docs/smart-contracts |

---

## Sister repos

| Repo | Description |
|---|---|
| [tx-watch-web](https://github.com/Tx-wats/web) | Web dashboard for alert history and contract management |
| [tx-watch-contracts](https://github.com/Tx-wats/contracts) | Example Soroban contracts to monitor with TxWatch |

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT
