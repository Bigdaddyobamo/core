#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]

//! txwatch-rules evaluates `AlertRule` conditions against enriched Stellar transactions
//! and constructs structured `AlertPayload` webhook bodies.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use txwatch_config::{AlertRule, RuleConfig, EVENT_TOPIC_WILDCARD};

// ── Constants ──────────────────────────────────────────────────────────────────

/// Maximum XLM supply in stroops: 50 billion XLM × 10^7 stroops/XLM
/// = 5 × 10^17 (500 quadrillion) stroops, well below u64::MAX (~1.8 × 10^19).
/// Parsed amounts and fees above this value cannot exist on the network, so
/// [`EnrichedTransaction::from_horizon`] discards them as malformed.
pub const MAX_XLM_SUPPLY_STROOPS: u64 = 500_000_000_000_000_000;

/// Returns `value` if it is a possible on-chain stroop amount, logging and
/// discarding anything above [`MAX_XLM_SUPPLY_STROOPS`].
fn sanitize_stroops(value: Option<u64>, field: &str, tx_hash: &str) -> Option<u64> {
    match value {
        Some(v) if v > MAX_XLM_SUPPLY_STROOPS => {
            tracing::warn!(
                tx = %tx_hash,
                field,
                value = v,
                max = MAX_XLM_SUPPLY_STROOPS,
                "parsed amount exceeds the total XLM supply — ignoring it"
            );
            None
        }
        other => other,
    }
}

// ── Horizon transaction shape ─────────────────────────────────────────────────

/// Raw Horizon transaction record as returned by the REST API.
#[derive(Debug, Clone, Deserialize)]
pub struct HorizonTransaction {
    pub hash: String,
    pub created_at: String, // RFC 3339
    pub successful: bool,
    pub paging_token: String,
    /// Fee charged in stroops (Horizon returns this as a string).
    pub fee_charged: Option<String>,
    /// Base64-encoded XDR transaction envelope.
    pub envelope_xdr: Option<String>,
    /// Base64-encoded XDR transaction result.
    pub result_xdr: Option<String>,
    /// Ledger sequence the transaction was included in; used to look up its
    /// contract events on Soroban RPC.
    #[serde(default)]
    pub ledger: Option<u32>,
}

// ── Contract events ───────────────────────────────────────────────────────────

/// A Soroban contract event emitted by a transaction, as returned by Soroban
/// RPC `getEvents` with `xdrFormat: "json"`. Topics and data are `ScVal`s in
/// their JSON form, e.g. `{"symbol": "transfer"}` or `{"address": "G…"}`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContractEvent {
    /// Contract that emitted the event.
    pub contract_id: String,
    pub topics: Vec<Value>,
    pub data: Value,
}

impl ContractEvent {
    /// The first topic as a symbol, if it is one.
    fn first_symbol(&self) -> Option<&str> {
        self.topics.first()?.get("symbol")?.as_str()
    }

    /// Does this event match an `EventEmitted { topic, topics }` rule?
    /// `topic` must equal topic 0 as a symbol exactly; each entry of `topics`
    /// is compared positionally against topics 1.. (`"*"` matches anything).
    pub fn matches(&self, topic: &str, topics: &[String]) -> bool {
        if self.first_symbol() != Some(topic) {
            return false;
        }
        topics.iter().enumerate().all(|(i, pattern)| {
            pattern == EVENT_TOPIC_WILDCARD
                || self
                    .topics
                    .get(i + 1)
                    .is_some_and(|value| topic_value_matches(value, pattern))
        })
    }
}

/// Compare one `ScVal` topic against a config pattern. Single-key scalar
/// values (`{"symbol": "x"}`, `{"address": "G…"}`, `{"u32": 5}`, `{"i128": "-1"}`)
/// match the inner value's text; anything else matches its compact JSON.
fn topic_value_matches(value: &Value, pattern: &str) -> bool {
    let scalar = |v: &Value| match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    let inner = match value {
        Value::Object(map) if map.len() == 1 => map.values().next().and_then(scalar),
        other => scalar(other),
    };
    match inner {
        Some(text) => text == pattern,
        None => value.to_string() == pattern,
    }
}

// ── Enriched transaction ──────────────────────────────────────────────────────

/// A transaction enriched with Soroban-specific fields extracted from the
/// Horizon `operations` sub-resource JSON (returned inline via `join=operations`
/// or fetched separately). We keep this as a plain struct so rule evaluation
/// stays pure and testable without network calls.
#[derive(Debug, Clone)]
pub struct EnrichedTransaction {
    pub hash: String,
    pub timestamp: DateTime<Utc>,
    pub successful: bool,
    pub paging_token: String,
    /// All Soroban contract functions invoked in this transaction (may be multiple).
    pub function_names: Vec<String>,
    /// Transfer amount in stroops (1 XLM = 10_000_000 stroops), if detected.
    /// Uses u64 because the total XLM supply is ~50 billion XLM = 5 × 10^17
    /// (500 quadrillion) stroops ([`MAX_XLM_SUPPLY_STROOPS`]), well within
    /// u64::MAX (~1.8 × 10^19).
    pub amount_stroops: Option<u64>,
    /// Fee charged for this transaction in stroops.
    pub fee_charged_stroops: Option<u64>,
    /// Contract events emitted by this transaction. Only populated when the
    /// contract has an `EventEmitted` rule (fetched from Soroban RPC).
    pub events: Vec<ContractEvent>,
}

impl EnrichedTransaction {
    /// Build from a raw Horizon record plus optional Soroban operation details.
    pub fn from_horizon(
        tx: HorizonTransaction,
        function_names: Vec<String>,
        amount_stroops: Option<u64>,
        fee_charged_stroops: Option<u64>,
    ) -> Result<Self> {
        let timestamp = tx.created_at.parse::<DateTime<Utc>>().with_context(|| {
            format!(
                "cannot parse timestamp '{}' for tx {}",
                tx.created_at, tx.hash
            )
        })?;

        let fee_charged_stroops = fee_charged_stroops.or_else(|| {
            tx.fee_charged
                .as_deref()
                .and_then(|s| s.parse::<u64>().ok())
        });

        Ok(Self {
            amount_stroops: sanitize_stroops(amount_stroops, "amount_stroops", &tx.hash),
            fee_charged_stroops: sanitize_stroops(
                fee_charged_stroops,
                "fee_charged_stroops",
                &tx.hash,
            ),
            hash: tx.hash,
            timestamp,
            successful: tx.successful,
            paging_token: tx.paging_token,
            function_names,
            events: Vec::new(),
        })
    }

    /// Attach the contract events emitted by this transaction.
    pub fn with_events(mut self, events: Vec<ContractEvent>) -> Self {
        self.events = events;
        self
    }
}

// ── AlertPayload ──────────────────────────────────────────────────────────────

/// The JSON body POSTed to the webhook URL when a rule fires.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AlertPayload {
    pub label: String,
    pub contract_id: String,
    pub network: String,
    /// Stable machine-readable rule variant (e.g. `"LargeTransfer"`).
    pub rule_type: String,
    pub rule_triggered: String,
    pub transaction_hash: String,
    /// First invoked function name (backward-compat singular field).
    pub function_name: Option<String>,
    /// All invoked function names in this transaction.
    pub function_names: Vec<String>,
    /// Amount in whole XLM (stroops / 10_000_000), present for LargeTransfer.
    #[serde(rename = "amount_xlm")]
    pub amount_xlm: Option<u64>,
    /// Fee charged in stroops.
    pub fee_charged_stroops: Option<u64>,
    /// Unix timestamp (seconds).
    pub timestamp: i64,
    /// ISO 8601 timestamp string.
    pub timestamp_iso: String,
    pub horizon_link: String,
    /// Stellar Expert explorer link for the transaction.
    pub explorer_link: String,
    /// Contract events that matched an `EventEmitted` rule (topics and data);
    /// empty for every other rule type.
    #[serde(default)]
    pub matched_events: Vec<ContractEvent>,
    /// Number of matches of this rule that were suppressed by its
    /// `cooldown_seconds` since the previous alert was sent. 0 when the rule
    /// has no cooldown or nothing was suppressed.
    #[serde(default)]
    pub suppressed_count: u64,
}

// ── Rule evaluation ───────────────────────────────────────────────────────────

/// Evaluate all rules for one contract against one transaction.
/// Returns one `AlertPayload` per matching rule.
/// Never panics — errors in individual rule evaluation are logged and skipped.
pub fn evaluate<R: AsRef<AlertRule>>(
    label: &str,
    contract_id: &str,
    network: &str,
    horizon_base: &str,
    explorer_base: &str,
    rules: &[R],
    tx: &EnrichedTransaction,
) -> Vec<AlertPayload> {
    let horizon_link = format!("{}/transactions/{}", horizon_base, tx.hash);
    let explorer_link = format!("{}/tx/{}", explorer_base, tx.hash);
    let timestamp = tx.timestamp.timestamp();
    let timestamp_iso = tx.timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    rules
        .iter()
        .map(AsRef::as_ref)
        .filter_map(|rule| match eval_rule(rule, tx) {
            Ok(true) => Some(AlertPayload {
                label: label.to_string(),
                contract_id: contract_id.to_string(),
                network: network.to_string(),
                rule_type: rule_type(rule),
                rule_triggered: rule_label(rule),
                transaction_hash: tx.hash.clone(),
                function_name: tx.function_names.first().cloned(),
                function_names: tx.function_names.clone(),
                amount_xlm: tx.amount_stroops.map(|s| s / 10_000_000),
                fee_charged_stroops: tx.fee_charged_stroops,
                timestamp,
                timestamp_iso: timestamp_iso.clone(),
                horizon_link: horizon_link.clone(),
                explorer_link: explorer_link.clone(),
                matched_events: matched_events(rule, tx),
                suppressed_count: 0,
            }),
            Ok(false) => None,
            Err(e) => {
                tracing::warn!(
                    tx = %tx.hash,
                    rule = %rule_label(rule),
                    error = %e,
                    "rule evaluation error — skipping"
                );
                None
            }
        })
        .collect()
}

// NOTE: When adding a new AlertRule variant, update both `eval_rule()` and
// `rule_label()` together. Rust's exhaustive matching catches missing arms,
// but this convention should be preserved for new rule variants.
fn eval_rule(rule: &AlertRule, tx: &EnrichedTransaction) -> Result<bool> {
    Ok(match rule {
        AlertRule::AnyTransaction => true,

        AlertRule::TransactionFailed => !tx.successful,

        AlertRule::LargeTransfer { threshold_xlm } => {
            let threshold_stroops = threshold_xlm
                .checked_mul(10_000_000)
                .context("threshold_xlm overflow when converting to stroops")?;
            tx.amount_stroops
                .map(|s| s >= threshold_stroops)
                .unwrap_or(false)
        }

        AlertRule::FunctionCalled { function_name } => tx
            .function_names
            .iter()
            .any(|f| f == function_name.as_str()),

        AlertRule::AdminFunctionCalled { function_names } => tx.function_names.iter().any(|f| {
            let f_lower = f.to_lowercase();
            function_names.iter().any(|n| n.to_lowercase() == f_lower)
        }),

        AlertRule::HighFee {
            threshold_stroops, ..
        } => tx
            .fee_charged_stroops
            .map(|f| f >= *threshold_stroops)
            .unwrap_or(false),

        AlertRule::EventEmitted { topic, topics } => {
            tx.events.iter().any(|e| e.matches(topic, topics))
        }
    })
}

/// The events an `EventEmitted` rule matched; empty for other rules.
fn matched_events(rule: &AlertRule, tx: &EnrichedTransaction) -> Vec<ContractEvent> {
    match rule {
        AlertRule::EventEmitted { topic, topics } => tx
            .events
            .iter()
            .filter(|e| e.matches(topic, topics))
            .cloned()
            .collect(),
        _ => Vec::new(),
    }
}

// NOTE: When adding a new AlertRule variant, update both `eval_rule()` and
// `rule_label()` together. Rust's exhaustive matching catches missing arms,
// but this convention should be preserved for new rule variants.
fn rule_label(rule: &AlertRule) -> String {
    match rule {
        AlertRule::AnyTransaction => "AnyTransaction".into(),
        AlertRule::TransactionFailed => "TransactionFailed".into(),
        AlertRule::LargeTransfer { threshold_xlm } => {
            format!("LargeTransfer(>={}XLM)", threshold_xlm)
        }
        AlertRule::FunctionCalled { function_name } => format!("FunctionCalled({})", function_name),
        AlertRule::AdminFunctionCalled { function_names } => {
            format!("AdminFunctionCalled([{}])", function_names.join(", "))
        }
        AlertRule::HighFee {
            threshold_stroops,
            threshold_xlm,
        } => {
            if let Some(xlm) = threshold_xlm {
                format!("HighFee(>={} XLM)", xlm)
            } else {
                format!("HighFee(>={} stroops)", threshold_stroops)
            }
        }
        AlertRule::EventEmitted { topic, topics } => {
            txwatch_config::event_emitted_label(topic, topics)
        }
    }
}

fn rule_type(rule: &AlertRule) -> String {
    match rule {
        AlertRule::AnyTransaction => "AnyTransaction".into(),
        AlertRule::TransactionFailed => "TransactionFailed".into(),
        AlertRule::LargeTransfer { .. } => "LargeTransfer".into(),
        AlertRule::FunctionCalled { .. } => "FunctionCalled".into(),
        AlertRule::AdminFunctionCalled { .. } => "AdminFunctionCalled".into(),
        AlertRule::HighFee { .. } => "HighFee".into(),
        AlertRule::EventEmitted { .. } => "EventEmitted".into(),
    }
}

// ── Cooldowns ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct CooldownState {
    last_fired: DateTime<Utc>,
    suppressed: u64,
}

/// Enforces per-rule `cooldown_seconds`: after a rule fires for a contract,
/// further matches of the same (contract, rule) within the window are dropped
/// and counted, and the next alert that goes out carries that count in
/// `suppressed_count`.
///
/// The current time is passed in by the caller, so tests control the clock.
#[derive(Debug, Default)]
pub struct CooldownTracker {
    state: HashMap<(String, String), CooldownState>,
}

impl CooldownTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Filter `payloads` (as returned by [`evaluate`] for `rules`) through each
    /// rule's cooldown at time `now`. Payloads whose rule has no cooldown pass
    /// through unchanged.
    pub fn apply(
        &mut self,
        rules: &[RuleConfig],
        payloads: Vec<AlertPayload>,
        now: DateTime<Utc>,
    ) -> Vec<AlertPayload> {
        payloads
            .into_iter()
            .filter_map(|mut payload| {
                let cooldown = rules
                    .iter()
                    .find(|r| rule_label(&r.rule) == payload.rule_triggered)
                    .and_then(|r| r.cooldown_seconds)
                    .unwrap_or(0);
                match self.check(
                    &payload.contract_id,
                    &payload.rule_triggered,
                    cooldown,
                    now,
                ) {
                    Some(suppressed) => {
                        payload.suppressed_count = suppressed;
                        Some(payload)
                    }
                    None => {
                        tracing::debug!(
                            contract = %payload.label,
                            rule = %payload.rule_triggered,
                            tx = %payload.transaction_hash,
                            cooldown_seconds = cooldown,
                            "rule matched within its cooldown — alert suppressed"
                        );
                        None
                    }
                }
            })
            .collect()
    }

    /// Record a match of `rule` on `contract_id` at `now`. Returns
    /// `Some(suppressed_count)` when the alert should fire (resetting the
    /// window and the count), or `None` when it falls inside the cooldown.
    pub fn check(
        &mut self,
        contract_id: &str,
        rule: &str,
        cooldown_seconds: u64,
        now: DateTime<Utc>,
    ) -> Option<u64> {
        if cooldown_seconds == 0 {
            return Some(0);
        }
        let key = (contract_id.to_owned(), rule.to_owned());
        let window = i64::try_from(cooldown_seconds)
            .ok()
            .and_then(chrono::TimeDelta::try_seconds)
            .unwrap_or(chrono::TimeDelta::MAX);
        match self.state.get_mut(&key) {
            Some(state) if now.signed_duration_since(state.last_fired) < window => {
                state.suppressed = state.suppressed.saturating_add(1);
                None
            }
            Some(state) => {
                let suppressed = state.suppressed;
                *state = CooldownState {
                    last_fired: now,
                    suppressed: 0,
                };
                Some(suppressed)
            }
            None => {
                self.state.insert(
                    key,
                    CooldownState {
                        last_fired: now,
                        suppressed: 0,
                    },
                );
                Some(0)
            }
        }
    }
}

impl AlertPayload {
    /// Builder helper to override the label (used by test-webhook).
    pub fn with_label(mut self, label: String) -> Self {
        self.label = label;
        self
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use chrono::Datelike;
    use txwatch_config::AlertRule;

    fn make_tx(
        successful: bool,
        function_names: &[&str],
        amount_stroops: Option<u64>,
    ) -> EnrichedTransaction {
        let function_names: Vec<String> = function_names.iter().map(|s| s.to_string()).collect();

        EnrichedTransaction {
            hash: "abc123".into(),
            timestamp: "2024-01-15T12:00:00Z".parse().unwrap(),
            successful,
            paging_token: "100".into(),
            function_names: function_names.iter().map(|s| s.to_string()).collect(),
            amount_stroops,
            fee_charged_stroops: None,
            events: vec![],
        }
    }

    fn run(rules: &[AlertRule], tx: &EnrichedTransaction) -> Vec<AlertPayload> {
        evaluate(
            "Label",
            "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "testnet",
            "https://horizon-testnet.stellar.org",
            "https://stellar.expert/explorer/testnet",
            rules,
            tx,
        )
    }

    #[test]
    fn any_transaction_always_fires() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].rule_triggered, "AnyTransaction");
    }

    #[test]
    fn any_transaction_fires_on_failed_transaction() {
        let tx = make_tx(false, &[], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].rule_triggered, "AnyTransaction");
    }

    #[test]
    fn rule_label_formats_are_stable() {
        assert_eq!(rule_label(&AlertRule::AnyTransaction), "AnyTransaction");
        assert_eq!(
            rule_label(&AlertRule::TransactionFailed),
            "TransactionFailed"
        );
        assert_eq!(
            rule_label(&AlertRule::LargeTransfer {
                threshold_xlm: 10_000
            }),
            "LargeTransfer(>=10000XLM)"
        );
        assert_eq!(
            rule_label(&AlertRule::FunctionCalled {
                function_name: "withdraw".into()
            }),
            "FunctionCalled(withdraw)"
        );
        assert_eq!(
            rule_label(&AlertRule::AdminFunctionCalled {
                function_names: vec!["set_admin".into(), "upgrade".into()]
            }),
            "AdminFunctionCalled([set_admin, upgrade])"
        );
        assert_eq!(
            rule_label(&AlertRule::HighFee {
                threshold_stroops: 10_000,
                threshold_xlm: None
            }),
            "HighFee(>=10000 stroops)"
        );
    }

    #[test]
    fn transaction_failed_fires_on_failure() {
        let tx = make_tx(false, &[], None);
        let payloads = run(&[AlertRule::TransactionFailed], &tx);
        assert_eq!(payloads.len(), 1);
    }

    #[test]
    fn transaction_failed_does_not_fire_on_success() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::TransactionFailed], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn large_transfer_fires_at_threshold() {
        // exactly 10_000 XLM = 100_000_000_000 stroops
        let tx = make_tx(true, &[], Some(100_000_000_000));
        let payloads = run(
            &[AlertRule::LargeTransfer {
                threshold_xlm: 10_000,
            }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].amount_xlm, Some(10_000));
    }

    #[test]
    fn large_transfer_does_not_fire_below_threshold() {
        let tx = make_tx(true, &[], Some(9_999 * 10_000_000));
        let payloads = run(
            &[AlertRule::LargeTransfer {
                threshold_xlm: 10_000,
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn large_transfer_no_amount_does_not_fire() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::LargeTransfer { threshold_xlm: 1 }], &tx);
        assert!(payloads.is_empty());
    }

    #[test]
    fn large_transfer_overflow_is_handled_gracefully() {
        let tx = make_tx(true, &[], Some(1_000_000_000_000_000));
        let payloads = run(
            &[AlertRule::LargeTransfer {
                threshold_xlm: u64::MAX,
            }],
            &tx,
        );
        assert!(
            payloads.is_empty(),
            "overflowing LargeTransfer thresholds should not panic"
        );
    }

    #[test]
    fn large_transfer_fires_at_exact_threshold() {
        let tx = make_tx(true, &[], Some(10_000 * 10_000_000));
        let payloads = run(
            &[AlertRule::LargeTransfer {
                threshold_xlm: 10_000,
            }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].amount_xlm, Some(10_000));
    }

    #[test]
    fn large_transfer_does_not_fire_one_stroop_below_threshold() {
        let tx = make_tx(true, &[], Some(10_000 * 10_000_000 - 1));
        let payloads = run(
            &[AlertRule::LargeTransfer {
                threshold_xlm: 10_000,
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn function_called_fires_on_match() {
        let tx = make_tx(true, &["withdraw"], None);
        let payloads = run(
            &[AlertRule::FunctionCalled {
                function_name: "withdraw".into(),
            }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].function_name.as_deref(), Some("withdraw"));
    }

    #[test]
    fn function_called_does_not_fire_on_mismatch() {
        let tx = make_tx(true, &["deposit"], None);
        let payloads = run(
            &[AlertRule::FunctionCalled {
                function_name: "withdraw".into(),
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn admin_function_called_fires_on_any_match() {
        let tx = make_tx(true, &["upgrade"], None);
        let payloads = run(
            &[AlertRule::AdminFunctionCalled {
                function_names: vec!["set_admin".into(), "upgrade".into()],
            }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert!(payloads[0].rule_triggered.contains("upgrade"));
    }

    #[test]
    fn function_called_does_not_fire_when_function_name_is_none() {
        let tx = make_tx(true, &[], None);
        let payloads = run(
            &[AlertRule::FunctionCalled {
                function_name: "withdraw".into(),
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn admin_function_called_does_not_fire_when_function_name_is_none() {
        let tx = make_tx(true, &[], None);
        let payloads = run(
            &[AlertRule::AdminFunctionCalled {
                function_names: vec!["set_admin".into(), "upgrade".into()],
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn multiple_rules_can_fire_on_same_tx() {
        let tx = make_tx(false, &["set_admin"], Some(200_000_000_000));
        let rules = vec![
            AlertRule::AnyTransaction,
            AlertRule::TransactionFailed,
            AlertRule::LargeTransfer {
                threshold_xlm: 10_000,
            },
            AlertRule::AdminFunctionCalled {
                function_names: vec!["set_admin".into()],
            },
        ];
        let payloads = run(&rules, &tx);
        assert_eq!(payloads.len(), 4);
    }

    #[test]
    fn horizon_link_is_correct() {
        let tx = make_tx(true, &[], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(
            payloads[0].horizon_link,
            "https://horizon-testnet.stellar.org/transactions/abc123"
        );
    }

    #[test]
    fn url_fields_have_no_trailing_slash_and_exact_format() {
        // Verify both link fields are normalised even when base URLs have trailing slashes.
        fn run_with_bases(horizon_base: &str, explorer_base: &str) -> AlertPayload {
            let tx = EnrichedTransaction {
                hash: "deadbeef".into(),
                timestamp: "2024-01-15T12:00:00Z".parse().unwrap(),
                successful: true,
                paging_token: "1".into(),
                function_names: vec![],
                amount_stroops: None,
                fee_charged_stroops: None,
                events: vec![],
            };
            let mut payloads = evaluate(
                "L",
                "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                "testnet",
                horizon_base,
                explorer_base,
                &[AlertRule::AnyTransaction],
                &tx,
            );
            payloads.remove(0)
        }

        // Without trailing slash — baseline
        let p = run_with_bases(
            "https://horizon-testnet.stellar.org",
            "https://stellar.expert/explorer/testnet",
        );
        assert_eq!(
            p.horizon_link,
            "https://horizon-testnet.stellar.org/transactions/deadbeef"
        );
        assert_eq!(
            p.explorer_link,
            "https://stellar.expert/explorer/testnet/tx/deadbeef"
        );

        // With trailing slash — must produce identical output
        let p2 = run_with_bases(
            "https://horizon-testnet.stellar.org/",
            "https://stellar.expert/explorer/testnet/",
        );
        assert_eq!(p.horizon_link, p2.horizon_link);
        assert_eq!(p.explorer_link, p2.explorer_link);
    }

    #[test]
    fn high_fee_fires_at_threshold() {
        let mut tx = make_tx(true, &[], None);
        tx.fee_charged_stroops = Some(10_000);
        let payloads = run(
            &[AlertRule::HighFee {
                threshold_stroops: 10_000,
                threshold_xlm: None,
            }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert!(payloads[0].rule_triggered.contains("HighFee"));
    }

    #[test]
    fn high_fee_does_not_fire_below_threshold() {
        let mut tx = make_tx(true, &[], None);
        tx.fee_charged_stroops = Some(9_999);
        let payloads = run(
            &[AlertRule::HighFee {
                threshold_stroops: 10_000,
                threshold_xlm: None,
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn high_fee_no_fee_does_not_fire() {
        let tx = make_tx(true, &[], None);
        let payloads = run(
            &[AlertRule::HighFee {
                threshold_stroops: 1,
                threshold_xlm: None,
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn enriched_transaction_parses_timestamp() {
        let raw = HorizonTransaction {
            hash: "h1".into(),
            created_at: "2024-06-01T00:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: Some("100".into()),
            envelope_xdr: None,
            result_xdr: None,
            ledger: None,
        };
        let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();
        assert_eq!(enriched.timestamp.year(), 2024);
    }

    /// Issue #65: from_horizon must return Err when created_at is not a valid RFC 3339 timestamp.
    #[test]
    fn from_horizon_rejects_invalid_timestamp() {
        let raw = HorizonTransaction {
            hash: "badhash".into(),
            created_at: "not-a-timestamp".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: None,
            envelope_xdr: None,
            result_xdr: None,
            ledger: None,
        };
        let result = EnrichedTransaction::from_horizon(raw, vec![], None, None);
        assert!(result.is_err(), "expected Err for invalid timestamp");
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("cannot parse timestamp"),
            "error message should mention 'cannot parse timestamp', got: {}",
            msg
        );
    }

    // ── Issue #77: multiple invoke_host_function ops ──────────────────────────

    #[test]
    fn function_called_fires_when_matching_name_is_second_in_list() {
        // Transaction has two Soroban invocations; rule should match the second
        let tx = make_tx(true, &["deposit", "withdraw"], None);
        let payloads = run(
            &[AlertRule::FunctionCalled {
                function_name: "withdraw".into(),
            }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].function_names, vec!["deposit", "withdraw"]);
    }

    #[test]
    fn function_called_does_not_fire_when_no_names_match() {
        let tx = make_tx(true, &["deposit", "transfer"], None);
        let payloads = run(
            &[AlertRule::FunctionCalled {
                function_name: "withdraw".into(),
            }],
            &tx,
        );
        assert!(payloads.is_empty());
    }

    #[test]
    fn admin_function_called_fires_on_any_of_multiple_invocations() {
        // Two invocations; only the second is an admin function
        let tx = make_tx(true, &["transfer", "set_admin"], None);
        let payloads = run(
            &[AlertRule::AdminFunctionCalled {
                function_names: vec!["set_admin".into(), "upgrade".into()],
            }],
            &tx,
        );
        assert_eq!(payloads.len(), 1);
    }

    #[test]
    fn payload_function_names_contains_all_invocations() {
        let tx = make_tx(true, &["foo", "bar", "baz"], None);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert_eq!(payloads[0].function_names, vec!["foo", "bar", "baz"]);
        // function_name (singular) is the first for backward compat
        assert_eq!(payloads[0].function_name.as_deref(), Some("foo"));
    }

    #[test]
    fn alert_payload_serialises_to_valid_json_with_all_fields_present() {
        let payload = AlertPayload {
            label: "My Contract".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network: "testnet".into(),
            rule_type: "LargeTransfer".into(),
            rule_triggered: "LargeTransfer(>=10000XLM)".into(),
            transaction_hash: "abc123".into(),
            function_name: Some("transfer".into()),
            function_names: vec!["transfer".into()],
            amount_xlm: Some(15000),
            fee_charged_stroops: Some(50000),
            timestamp: 1705316096,
            timestamp_iso: "2024-01-15T12:00:00Z".into(),
            horizon_link: "https://horizon-testnet.stellar.org/transactions/abc123".into(),
            explorer_link: "https://stellar.expert/explorer/testnet/tx/abc123".into(),
            matched_events: vec![],
            suppressed_count: 0,
        };

        let json = serde_json::to_value(payload).expect("serialize AlertPayload to JSON");
        let obj = json
            .as_object()
            .expect("AlertPayload should serialize to a JSON object");

        assert_eq!(obj["label"].as_str(), Some("My Contract"));
        assert_eq!(
            obj["contract_id"].as_str(),
            Some("CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        );
        assert_eq!(obj["network"].as_str(), Some("testnet"));
        assert_eq!(obj["rule_type"].as_str(), Some("LargeTransfer"));
        assert_eq!(
            obj["rule_triggered"].as_str(),
            Some("LargeTransfer(>=10000XLM)")
        );
        assert_eq!(obj["transaction_hash"].as_str(), Some("abc123"));
        assert_eq!(obj["function_name"].as_str(), Some("transfer"));
        assert_eq!(obj["function_names"].as_array().map(|a| a.len()), Some(1));
        assert_eq!(obj["amount_xlm"].as_u64(), Some(15000));
        assert_eq!(obj["fee_charged_stroops"].as_u64(), Some(50000));
        assert_eq!(obj["timestamp"].as_i64(), Some(1705316096));
        assert_eq!(obj["timestamp_iso"].as_str(), Some("2024-01-15T12:00:00Z"));
        assert_eq!(
            obj["horizon_link"].as_str(),
            Some("https://horizon-testnet.stellar.org/transactions/abc123")
        );
        assert_eq!(
            obj["explorer_link"].as_str(),
            Some("https://stellar.expert/explorer/testnet/tx/abc123")
        );
    }

    // ── Issue #48: MAX_XLM_SUPPLY_STROOPS sanity check ────────────────────────

    fn raw_tx(fee_charged: Option<&str>) -> HorizonTransaction {
        HorizonTransaction {
            hash: "h1".into(),
            created_at: "2024-06-01T00:00:00Z".into(),
            successful: true,
            paging_token: "1".into(),
            fee_charged: fee_charged.map(Into::into),
            envelope_xdr: None,
            result_xdr: None,
            ledger: None,
        }
    }

    #[test]
    fn from_horizon_discards_amount_above_total_supply() {
        let enriched = EnrichedTransaction::from_horizon(
            raw_tx(None),
            vec![],
            Some(MAX_XLM_SUPPLY_STROOPS + 1),
            None,
        )
        .unwrap();
        assert_eq!(enriched.amount_stroops, None);
    }

    #[test]
    fn from_horizon_keeps_amount_at_total_supply() {
        let enriched = EnrichedTransaction::from_horizon(
            raw_tx(None),
            vec![],
            Some(MAX_XLM_SUPPLY_STROOPS),
            None,
        )
        .unwrap();
        assert_eq!(enriched.amount_stroops, Some(MAX_XLM_SUPPLY_STROOPS));
    }

    #[test]
    fn from_horizon_discards_fee_above_total_supply() {
        let fee = (MAX_XLM_SUPPLY_STROOPS + 1).to_string();
        let enriched =
            EnrichedTransaction::from_horizon(raw_tx(Some(&fee)), vec![], None, None).unwrap();
        assert_eq!(enriched.fee_charged_stroops, None);
    }

    // ── Issue #50: EventEmitted ───────────────────────────────────────────────

    fn event(topics: Vec<Value>, data: Value) -> ContractEvent {
        ContractEvent {
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            topics,
            data,
        }
    }

    fn transfer_event() -> ContractEvent {
        event(
            vec![
                serde_json::json!({"symbol": "transfer"}),
                serde_json::json!({"address": "GFROM"}),
                serde_json::json!({"address": "GTO"}),
            ],
            serde_json::json!({"i128": "1000"}),
        )
    }

    fn event_rule(topic: &str, topics: &[&str]) -> AlertRule {
        AlertRule::EventEmitted {
            topic: topic.into(),
            topics: topics.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn event_emitted_fires_on_first_topic_symbol() {
        let tx = make_tx(true, &[], None).with_events(vec![transfer_event()]);
        let payloads = run(&[event_rule("transfer", &[])], &tx);
        assert_eq!(payloads.len(), 1);
        assert_eq!(payloads[0].rule_type, "EventEmitted");
        assert_eq!(payloads[0].rule_triggered, "EventEmitted(transfer)");
        assert_eq!(payloads[0].matched_events, vec![transfer_event()]);
    }

    #[test]
    fn event_emitted_does_not_fire_on_other_symbol() {
        let tx = make_tx(true, &[], None).with_events(vec![transfer_event()]);
        assert!(run(&[event_rule("mint", &[])], &tx).is_empty());
    }

    #[test]
    fn event_emitted_does_not_fire_without_events() {
        let tx = make_tx(true, &["transfer"], None);
        assert!(run(&[event_rule("transfer", &[])], &tx).is_empty());
    }

    #[test]
    fn event_emitted_first_topic_must_be_a_symbol() {
        let ev = event(
            vec![serde_json::json!({"string": "transfer"})],
            Value::Null,
        );
        let tx = make_tx(true, &[], None).with_events(vec![ev]);
        assert!(run(&[event_rule("transfer", &[])], &tx).is_empty());
    }

    #[test]
    fn event_emitted_matches_further_topics_positionally() {
        let tx = make_tx(true, &[], None).with_events(vec![transfer_event()]);
        assert_eq!(run(&[event_rule("transfer", &["GFROM"])], &tx).len(), 1);
        assert_eq!(run(&[event_rule("transfer", &["*", "GTO"])], &tx).len(), 1);
        assert!(run(&[event_rule("transfer", &["GTO"])], &tx).is_empty());
        assert!(run(&[event_rule("transfer", &["*", "*", "GX"])], &tx).is_empty());
    }

    #[test]
    fn event_emitted_wildcard_matches_missing_topic() {
        let tx = make_tx(true, &[], None).with_events(vec![transfer_event()]);
        assert_eq!(run(&[event_rule("transfer", &["*", "*", "*"])], &tx).len(), 1);
    }

    #[test]
    fn event_emitted_payload_contains_only_matching_events() {
        let mint = event(
            vec![serde_json::json!({"symbol": "mint"})],
            serde_json::json!({"i128": "5"}),
        );
        let tx = make_tx(true, &[], None).with_events(vec![mint, transfer_event()]);
        let payloads = run(&[event_rule("transfer", &[])], &tx);
        assert_eq!(payloads[0].matched_events, vec![transfer_event()]);
    }

    #[test]
    fn non_event_rules_have_empty_matched_events() {
        let tx = make_tx(true, &[], None).with_events(vec![transfer_event()]);
        let payloads = run(&[AlertRule::AnyTransaction], &tx);
        assert!(payloads[0].matched_events.is_empty());
    }

    #[test]
    fn topic_value_matches_scalars_and_json() {
        assert!(topic_value_matches(&serde_json::json!({"u32": 5}), "5"));
        assert!(topic_value_matches(&serde_json::json!({"bool": true}), "true"));
        assert!(topic_value_matches(&serde_json::json!("plain"), "plain"));
        let vec_val = serde_json::json!({"vec": [{"u32": 1}]});
        assert!(topic_value_matches(&vec_val, &vec_val.to_string()));
        assert!(!topic_value_matches(&vec_val, "1"));
    }

    #[test]
    fn event_emitted_label_includes_extra_topics() {
        assert_eq!(
            rule_label(&event_rule("transfer", &["*", "GTO"])),
            "EventEmitted(transfer, *, GTO)"
        );
    }

    // ── Issue #49: cooldowns ──────────────────────────────────────────────────

    /// A clock the test advances by hand.
    struct TestClock(DateTime<Utc>);

    impl TestClock {
        fn start() -> Self {
            Self("2024-01-15T12:00:00Z".parse().unwrap())
        }
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
        fn advance(&mut self, secs: i64) {
            self.0 += chrono::TimeDelta::seconds(secs);
        }
    }

    fn with_cooldown(rule: AlertRule, cooldown: u64) -> RuleConfig {
        RuleConfig {
            rule,
            cooldown_seconds: Some(cooldown),
        }
    }

    fn fire(
        tracker: &mut CooldownTracker,
        rules: &[RuleConfig],
        clock: &TestClock,
    ) -> Vec<AlertPayload> {
        let tx = make_tx(false, &[], None);
        tracker.apply(rules, run(rules, &tx), clock.now())
    }

    #[test]
    fn cooldown_suppresses_within_window_and_reports_count() {
        let rules = vec![with_cooldown(AlertRule::AnyTransaction, 60)];
        let mut tracker = CooldownTracker::new();
        let mut clock = TestClock::start();

        let first = fire(&mut tracker, &rules, &clock);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].suppressed_count, 0);

        for _ in 0..3 {
            clock.advance(10);
            assert!(fire(&mut tracker, &rules, &clock).is_empty());
        }

        clock.advance(30); // 60s after the first alert
        let next = fire(&mut tracker, &rules, &clock);
        assert_eq!(next.len(), 1);
        assert_eq!(next[0].suppressed_count, 3);

        clock.advance(60);
        let after = fire(&mut tracker, &rules, &clock);
        assert_eq!(after[0].suppressed_count, 0, "count resets after firing");
    }

    #[test]
    fn no_cooldown_passes_everything_through() {
        let rules: Vec<RuleConfig> = vec![AlertRule::AnyTransaction.into()];
        let mut tracker = CooldownTracker::new();
        let clock = TestClock::start();
        for _ in 0..5 {
            let out = fire(&mut tracker, &rules, &clock);
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].suppressed_count, 0);
        }
    }

    #[test]
    fn zero_cooldown_is_disabled() {
        let rules = vec![with_cooldown(AlertRule::AnyTransaction, 0)];
        let mut tracker = CooldownTracker::new();
        let clock = TestClock::start();
        assert_eq!(fire(&mut tracker, &rules, &clock).len(), 1);
        assert_eq!(fire(&mut tracker, &rules, &clock).len(), 1);
    }

    #[test]
    fn cooldown_is_tracked_per_rule() {
        let rules = vec![
            with_cooldown(AlertRule::AnyTransaction, 60),
            AlertRule::TransactionFailed.into(),
        ];
        let mut tracker = CooldownTracker::new();
        let mut clock = TestClock::start();
        assert_eq!(fire(&mut tracker, &rules, &clock).len(), 2);
        clock.advance(1);
        let second = fire(&mut tracker, &rules, &clock);
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].rule_type, "TransactionFailed");
    }

    #[test]
    fn cooldown_is_tracked_per_contract() {
        let mut tracker = CooldownTracker::new();
        let now = TestClock::start().now();
        assert_eq!(tracker.check("CA", "AnyTransaction", 60, now), Some(0));
        assert_eq!(tracker.check("CB", "AnyTransaction", 60, now), Some(0));
        assert_eq!(tracker.check("CA", "AnyTransaction", 60, now), None);
    }
}
