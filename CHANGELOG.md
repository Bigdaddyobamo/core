# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- HMAC-SHA256 webhook signatures: `X-TxWatch-Signature: sha256=<hex>` over the request body when `webhook_secret` is set
- `X-TxWatch-Version` header on every webhook request
- `${ENV_VAR}` interpolation for `webhook_secret`
- Graceful shutdown on Ctrl-C: the in-flight poll cycle finishes before exit
- Optional Prometheus metrics (`metrics` feature of `txwatch-poller`) with a `/metrics` endpoint
- `cursor_file` setting to persist per-contract cursors across restarts
- HTTP pool settings: `http_pool_max_idle_per_host`, `http_tcp_keepalive_secs`, `http_connection_verbose`
- `HighFee` accepts `threshold_xlm` as an alternative to `threshold_stroops`
- Alert payload fields `rule_type`, `fee_charged_stroops`, `timestamp_iso` and `explorer_link`
- `txwatch schema` command and a committed JSON Schema for editor validation
- `txwatch validate --check-webhooks` and `--check-horizon` pre-flight checks
- Horizon operations fetched inline with `join=operations`
- Per-contract `poll_interval_seconds` override; each contract is polled on its own schedule and `txwatch validate` shows the effective interval

### Changed

- `poll_interval_seconds` is bounded to 5–3600 seconds
- `txwatch test-webhook` exits with code 1 when delivery fails
- Config parse errors name the offending field path
- `poll_interval_seconds` defaults to 10 and is no longer required
- `http_pool_max_idle_per_host` and `http_tcp_keepalive_secs` have concrete defaults (10 and 30) and are range-checked (1–100 and 0–7200); `http_tcp_keepalive_secs = 0` now disables keepalive as documented
- Contract labels are trimmed, must not contain control characters, are limited to 128 characters, and are compared case-insensitively for duplicates
- `FunctionCalled` / `AdminFunctionCalled` function names must be valid Soroban symbols (at most 32 characters from `[a-zA-Z0-9_]`)

## [0.1.0] - 2025-01-01

### Added

- Real-time Soroban smart contract monitoring and webhook alert engine
- Six alert rule types: `AnyTransaction`, `TransactionFailed`, `LargeTransfer`, `FunctionCalled`, `AdminFunctionCalled`, `HighFee`
- Horizon REST API integration with cursor-based pagination for efficient polling
- TOML-based configuration with contract, rule, and webhook setup
- Support for multiple Stellar networks: mainnet, testnet, futurenet
- Webhook notification delivery with exponential backoff retry logic (up to 3 attempts)
- Webhook secret sent as a plain `X-TxWatch-Secret` header (superseded by HMAC signing; see Unreleased)
- Structured JSON alert payloads with transaction details and Horizon links
- Transaction enrichment with operation-level details (function names, transfer amounts)
- Fee extraction and analysis for cost monitoring
- Configurable polling intervals
- CLI commands: `watch`, `validate`, `test-webhook`
- Comprehensive configuration reference and alert rules documentation
- Integration test suite using wiremock for HTTP mocking

---

[Keep a Changelog]: https://keepachangelog.com/en/1.0.0/
