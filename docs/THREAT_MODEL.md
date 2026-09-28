# TxWatch Threat Model

## 1. System Overview & Architecture

TxWatch is a security monitoring daemon for Soroban smart contracts on the Stellar network. It continuously polls the Stellar Horizon API for newly ingested transactions, enriches transaction data with Soroban invocation details and payments, evaluates user-defined alert rules against enriched transactions, and dispatches webhook payloads signed via HMAC-SHA256 to external monitoring endpoints (e.g. alerting pipelines, Slack, Discord, custom webhooks).

```
+-------------------------------------------------------------------------------+
|                                TxWatch Daemon                                 |
|                                                                               |
|  +--------------------+        +--------------------+                         |
|  |   txwatch-cli      | -----> |   txwatch-config   |                         |
|  +--------------------+        +--------------------+                         |
|            |                             |                                    |
|            v                             v                                    |
|  +--------------------+        +--------------------+                         |
|  |   txwatch-poller   | <----> |   txwatch-rules    |                         |
|  +--------------------+        +--------------------+                         |
|            |                                                                  |
|            v                                                                  |
|  +--------------------+                                                       |
|  |  txwatch-notifier  |                                                       |
|  +--------------------+                                                       |
+------------|------------------------------------------------------------------+
             |
             v (HTTP POST / HMAC-SHA256)
     [ External Webhook Receiver ]
```

---

## 2. Assets & Data Flow

| Asset | Sensitivity | Invariants & Security Objectives |
| :--- | :--- | :--- |
| **Webhook Secrets (`webhook_secret`)** | Critical / Confidential | Must never be logged in plaintext, must not be leaked via HTTP headers (`X-TxWatch-Secret` deprecated/isolated), must not be exposed over plain unencrypted HTTP. |
| **Webhook Destination URLs** | Medium / Confidential | Often embed sensitive tokens (e.g. Slack webhook URLs); must be redacted in public logs and stdout. |
| **Cursor State (`cursor_file`)** | High / Integrity | Must accurately track the latest processed transaction paging token; writes must be atomic to prevent rollback/replay or skipped transactions. |
| **Alert Rules Configuration** | High / Integrity | Must not allow integer overflow, arbitrary injection, or unvalidated host/path traversal. |
| **Alert Notification Payloads** | Medium / Authenticity | Must be verifiable by the receiver via HMAC-SHA256 signature to guarantee payload integrity and authenticity. |

---

## 3. Trust Boundaries & Threat Actors

### Trust Boundaries
1. **Daemon Boundary (Local Process vs External Network)**: Horizon API nodes (untrusted remote input) and Webhook receivers (untrusted remote endpoints).
2. **Configuration Boundary**: File system permissions on TOML configuration and cursor files; environment variables.
3. **Internal Crate Interfaces**: Separation of validation (`txwatch-config`), detection (`txwatch-rules`), poller coordination (`txwatch-poller`), and transport delivery (`txwatch-notifier`).

### Threat Actors
- **Malicious Horizon Node / MITM**: Supplies malformed JSON, forged transactions, recursive pagination loops, or excessive 429 rate limit responses.
- **Malicious Webhook Receiver / Adversary**: Attempts Server-Side Request Forgery (SSRF) against internal metadata or loopback services, slowloris DoS (hanging connections), or replay attacks using intercepted signatures.
- **Local Low-Privilege Attacker**: Attempts path traversal via `cursor_file`, reading plaintext secrets from log aggregators, or process termination mid-flight.

---

## 4. STRIDE Threat Analysis

| Threat Category | Potential Attack Vector | Component | Mitigation & Defense |
| :--- | :--- | :--- | :--- |
| **Spoofing** | Forged alerts sent to receiver; replaying old alert payloads | `txwatch-notifier` | HMAC-SHA256 signature over payload body; inclusion of timestamp in signed claims. |
| **Tampering** | Interrupted cursor writes resulting in corrupted JSON cursor state | `txwatch-poller` | Atomic file write via `.tmp` file swap with `fs::rename`. |
| **Repudiation** | Dropped webhook alerts without persistence or tracing | `txwatch-poller`, `txwatch-notifier` | Structured tracing, error logging, and retry backoff mechanics. |
| **Information Disclosure** | Webhook secrets leaked in logs or error messages; cleartext transmission | `txwatch-config`, `txwatch-notifier` | Redaction of tokens in logs; enforcement of HTTPS when secrets are configured. |
| **Denial of Service** | Slow or unreachable receiver blocking Horizon polling loop; integer overflow | `txwatch-poller`, `txwatch-rules` | Asynchronous decoupled webhook delivery; checked arithmetic on thresholds (`checked_mul`). |
| **Elevation of Privilege** | Arbitrary file overwrite via unsanitized cursor file paths | `txwatch-config` | Strict path handling, atomic file creation, and deny-unknown-fields deserialization. |

---

## 5. Crate-by-Crate Security Threats & Mitigations

### 1. `txwatch-config`
- **Threat**: Malformed contract addresses or absurd numeric thresholds (e.g. `threshold_xlm` > 1,000,000,000) leading to integer overflow or dead-looping rules.
- **Mitigation**: Bounds checking (`MAX_LARGE_TRANSFER_THRESHOLD_XLM`), Soroban symbol character set validation (`[a-zA-Z0-9_]`), and URL scheme validation.

### 2. `txwatch-rules`
- **Threat**: Integer overflow when converting whole XLM to stroops (`10_000_000` multiplier); case sensitivity mismatches when matching function names.
- **Mitigation**: Safe checked arithmetic (`checked_mul`), normalization of admin function symbols to lowercase, and rigorous payload type checking.

### 3. `txwatch-notifier`
- **Threat**: Exposure of shared secrets; slow receivers exhausting connection pools; infinite retries on unrecoverable HTTP 4xx client errors.
- **Mitigation**: Hard retry limits (`MAX_RETRIES`), exponential backoff with sleep interruption on shutdown, and immediate failure on non-retryable 4xx responses.

### 4. `txwatch-poller`
- **Threat**: Head-of-line blocking where inline await on webhook delivery stalls contract polling; cursor desynchronization causing missing alerts.
- **Mitigation**: Concurrent JoinSet polling per contract; decoupled webhook execution; atomic cursor updates before enrichment.

### 5. `txwatch-cli`
- **Threat**: Signal termination leaving in-flight deliveries broken; argument injection in subcommand handlers.
- **Mitigation**: Graceful shutdown token broadcast (`watch::Receiver<bool>`); clap-based strict type parsing.

---

## 6. Verification & Executable Attack Tests

Executable attack tests are located across all crate test suites:
- `crates/config/tests/attack_tests.rs`: Malicious config injection, threshold overflow, and invalid contract ID validation.
- `crates/rules/tests/attack_tests.rs`: Arithmetic overflow exploitation, case-spoofing in symbols, and malformed XDR transactions.
- `crates/notifier/tests/attack_tests.rs`: Replay attacks, retry exhaustion attacks, and secret leakage prevention.
- `crates/poller/tests/attack_tests.rs`: Malicious Horizon pagination DoS, 429 flood resilience, and atomic cursor recovery.
- `crates/cli/tests/attack_tests.rs`: Signal interruption resilience and config path validation.
