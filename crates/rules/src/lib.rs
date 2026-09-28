#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used)]

//! txwatch-rules evaluates `AlertRule` conditions against enriched Stellar transactions
//! and constructs structured `AlertPayload` webhook bodies.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use txwatch_config::AlertRule;

// ── Constants ──────────────────────────────────────────────────────────────────

/// Maximum XLM supply in stroops: 50 billion XLM × 10^7 stroops/XLM.
/// The total Stellar XLM supply is capped at ~50 billion XLM. This constant serves
/// as a reference for validating that u64 is sufficient for any realistic transaction
/// amount, since 500 trillion is well below u64::MAX (18.4 quintillion).
pub const MAX_XLM_SUPPLY_STROOPS: u64 = 500_000_000_000_000_000;

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
    /// Uses u64 because the total XLM supply is ~50 billion XLM = ~500 trillion stroops,
    /// which is well within u64::MAX (18.4 quintillion). This type is sufficient for any
    /// realistic transaction amount on the Stellar network.
    pub amount_stroops: Option<u64>,
    /// Fee charged for this transaction in stroops.
    pub fee_charged_stroops: Option<u64>,
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

        Ok(Self {
            hash: tx.hash,
            timestamp,
            successful: tx.successful,
            paging_token: tx.paging_token,
            function_names,
            amount_stroops,
            fee_charged_stroops: fee_charged_stroops.or_else(|| {
                tx.fee_charged
                    .as_deref()
                    .and_then(|s| s.parse::<u64>().ok())
            }),
        })
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
    /// Transaction hash, or `null` for synthetic alerts (e.g. `NoActivity`).
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
    /// `true` for a recovery alert (e.g. activity resumed after `NoActivity`).
    /// `false` (the default) for an incident alert.
    #[serde(default)]
    pub resolved: bool,
}

// ── Rule evaluation ───────────────────────────────────────────────────────────

/// Context passed to [`evaluate`] to identify the contract being evaluated
/// and provide the link base URLs needed to build webhook payloads.
///
/// Using a struct instead of five positional `&str` parameters prevents
/// argument-order bugs (e.g. swapping `horizon_base` and `explorer_base`).
///
/// # Example
/// ```
/// use txwatch_rules::EvalContext;
/// use txwatch_config::{Network, WatchedContract, AlertRule};
///
/// let ctx = EvalContext::from_contract(
///     &WatchedContract {
///         label: "My Oracle".into(),
///         contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
///         network: Network::Testnet,
///         rules: vec![AlertRule::AnyTransaction],
///         webhook_url: "https://hooks.example.com/hook".into(),
///         webhook_secret: None,
///         poll_interval_seconds: None,
///         horizon_base_url_override: None,
///     },
/// );
/// assert_eq!(ctx.network, "testnet");
/// ```
#[derive(Debug, Clone)]
pub struct EvalContext<'a> {
    pub label: &'a str,
    pub contract_id: &'a str,
    pub network: &'a str,
    pub horizon_base: &'a str,
    /// Explorer base URL for the network; `None` for custom networks that
    /// have no configured explorer (links will fall back to `horizon_link`).
    pub explorer_base: Option<&'a str>,
}

impl<'a> EvalContext<'a> {
    /// Convenience constructor: derive all fields from a [`WatchedContract`].
    /// Callers that need to override the Horizon base URL (e.g. tests using a
    /// mock server) should fill in the fields manually instead.
    pub fn from_contract(contract: &'a txwatch_config::WatchedContract) -> Self {
        let horizon_base = contract
            .horizon_base_url_override
            .as_deref()
            .unwrap_or_else(|| contract.network.horizon_base_url());
        Self {
            label: &contract.label,
            contract_id: &contract.contract_id,
            network: contract.network.as_str(),
            horizon_base,
            explorer_base: contract.network.explorer_base_url(),
        }
    }
}

/// Shared, thread-safe counter map used to rate-limit repeated evaluation
/// warnings. Keyed by `"<contract_id>:<rule_label>"`.
#[derive(Debug, Default, Clone)]
pub struct WarningSuppressor(Arc<Mutex<HashMap<String, u64>>>);

impl WarningSuppressor {
    /// Returns `true` if the warning for this key should be emitted (i.e. the
    /// first occurrence or every 100th recurrence).
    pub fn should_warn(&self, key: &str) -> bool {
        if let Ok(mut map) = self.0.lock() {
            let count = map.entry(key.to_owned()).or_insert(0);
            *count += 1;
            *count == 1 || *count % 100 == 0
        } else {
            true // lock poisoned — always warn rather than silently drop
        }
    }

    /// Returns how many times a warning has been suppressed for the given key.
    #[cfg(test)]
    pub fn suppressed_count(&self, key: &str) -> u64 {
        self.0
            .lock()
            .ok()
            .and_then(|m| m.get(key).copied())
            .unwrap_or(0)
    }
}

/// Evaluate all per-transaction rules for one contract against one transaction.
/// Returns one `AlertPayload` per matching rule.
/// Never panics — errors in individual rule evaluation are logged and skipped.
/// Repeated errors for the same rule are suppressed after the first occurrence
/// (see [`WarningSuppressor`]).
///
/// Pass `suppressor` as `None` to use a one-shot suppressor (appropriate for
/// replay and tests). The poller keeps a per-contract suppressor to
/// deduplicate repeated warnings across poll cycles.
pub fn evaluate(
    ctx: &EvalContext<'_>,
    rules: &[AlertRule],
    tx: &EnrichedTransaction,
    suppressor: Option<&WarningSuppressor>,
) -> Vec<AlertPayload> {
    let horizon_base = ctx.horizon_base.trim_end_matches('/');
    let horizon_link = format!("{}/transactions/{}", horizon_base, tx.hash);
    let explorer_link = match ctx.explorer_base {
        Some(base) => format!("{}/tx/{}", base.trim_end_matches('/'), tx.hash),
        None => horizon_link.clone(),
    };
    let timestamp = tx.timestamp.timestamp();
    let timestamp_iso = tx.timestamp.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    let _local_suppressor;
    let suppressor = match suppressor {
        Some(s) => s,
        None => {
            _local_suppressor = WarningSuppressor::default();
            &_local_suppressor
        }
    };

    rules
        .iter()
        .filter_map(|rule| match eval_rule(rule, tx) {
            Ok(true) => Some(AlertPayload {
                label: ctx.label.to_string(),
                contract_id: ctx.contract_id.to_string(),
                network: ctx.network.to_string(),
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
                resolved: false,
            }),
            Ok(false) => None,
            Err(e) => {
                let key = format!("{}:{}", ctx.contract_id, rule_label(rule));
                if suppressor.should_warn(&key) {
                    tracing::warn!(
                        tx = %tx.hash,
                        rule = %rule_label(rule),
                        error = %e,
                        "rule evaluation error — skipping"
                    );
                }
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

        // NoActivity is a poll-cycle-level rule evaluated by `check_no_activity`,
        // not a per-transaction rule.  It never fires here.
        AlertRule::NoActivity { .. } => false,
    })
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
        AlertRule::NoActivity { minutes } => format!("NoActivity({}min)", minutes),
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
        AlertRule::NoActivity { .. } => "NoActivity".into(),
    }
}

// ── NoActivity poll-cycle evaluation ─────────────────────────────────────────

/// State machine for a single `NoActivity` rule instance on one contract.
///
/// The poller keeps one `NoActivityState` per `(contract_id, rule_index)` pair
/// and calls [`check_no_activity`] once per poll cycle — even when there are no
/// new transactions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoActivityState {
    /// The contract is active (or we haven't yet exceeded the threshold).
    Active,
    /// The threshold was exceeded and an alert was fired.  We're in the quiet
    /// window; we will fire a recovery alert the next time a transaction arrives.
    Alerting,
}

impl Default for NoActivityState {
    fn default() -> Self {
        Self::Active
    }
}

/// Check the `NoActivity` rule for a single contract after one poll cycle.
///
/// * `rule`              — must be `AlertRule::NoActivity { minutes }`.
/// * `last_seen`         — the timestamp of the most recent transaction seen,
///                         or `None` if we have never seen any transaction.
/// * `now`               — current time (injectable for testing).
/// * `state`             — mutable state carried across poll cycles.
/// * `ctx`               — context used to build the `AlertPayload`.
///
/// Returns `Some(payload)` when the rule fires (either an incident or a
/// recovery); returns `None` when nothing changed.
///
/// Behaviour:
/// - First breach → returns an incident payload and transitions to `Alerting`.
/// - Still quiet (consecutive breaches) → returns `None` (already alerting).
/// - Activity resumes after breach → returns a recovery payload
///   (`resolved = true`) and transitions back to `Active`.
/// - Activity present and never breached → returns `None`.
pub fn check_no_activity(
    rule: &AlertRule,
    last_seen: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    state: &mut NoActivityState,
    ctx: &EvalContext<'_>,
) -> Option<AlertPayload> {
    let AlertRule::NoActivity { minutes } = rule else {
        return None;
    };

    let horizon_base = ctx.horizon_base.trim_end_matches('/');
    let explorer_base = ctx.explorer_base.map(|b| b.trim_end_matches('/'));

    let threshold = chrono::Duration::minutes(*minutes as i64);
    let quiet_since = last_seen
        .map(|t| now - t)
        .unwrap_or_else(|| chrono::Duration::MAX);
    let is_quiet = quiet_since >= threshold;

    let ts = now.timestamp();
    let ts_iso = now.format("%Y-%m-%dT%H:%M:%SZ").to_string();

    // Synthetic payloads have no transaction — use empty hash and link to Horizon
    // account page rather than a specific transaction.
    let synthetic_hash = String::new();
    let synthetic_horizon_link = format!(
        "{}/accounts/{}",
        horizon_base, ctx.contract_id
    );
    let synthetic_explorer_link = match explorer_base {
        Some(base) => format!("{}/contract/{}", base, ctx.contract_id),
        None => synthetic_horizon_link.clone(),
    };

    match (&*state, is_quiet) {
        // Threshold just exceeded for the first time → fire incident.
        (NoActivityState::Active, true) => {
            *state = NoActivityState::Alerting;
            Some(AlertPayload {
                label: ctx.label.to_string(),
                contract_id: ctx.contract_id.to_string(),
                network: ctx.network.to_string(),
                rule_type: "NoActivity".into(),
                rule_triggered: format!("NoActivity({}min)", minutes),
                transaction_hash: synthetic_hash,
                function_name: None,
                function_names: vec![],
                amount_xlm: None,
                fee_charged_stroops: None,
                timestamp: ts,
                timestamp_iso: ts_iso,
                horizon_link: synthetic_horizon_link,
                explorer_link: synthetic_explorer_link,
                resolved: false,
            })
        }
        // Already alerting and activity has resumed → fire recovery.
        (NoActivityState::Alerting, false) => {
            *state = NoActivityState::Active;
            let tx_ts = last_seen.unwrap_or(now);
            let last_horizon_link = match last_seen {
                Some(_) => format!("{}/accounts/{}", horizon_base, ctx.contract_id),
                None => synthetic_horizon_link,
            };
            let last_explorer_link = match (explorer_base, last_seen) {
                (Some(base), _) => format!("{}/contract/{}", base, ctx.contract_id),
                _ => last_horizon_link.clone(),
            };
            Some(AlertPayload {
                label: ctx.label.to_string(),
                contract_id: ctx.contract_id.to_string(),
                network: ctx.network.to_string(),
                rule_type: "NoActivity".into(),
                rule_triggered: format!("NoActivity({}min) resolved", minutes),
                transaction_hash: synthetic_hash,
                function_name: None,
                function_names: vec![],
                amount_xlm: None,
                fee_charged_stroops: None,
                timestamp: tx_ts.timestamp(),
                timestamp_iso: tx_ts.format("%Y-%m-%dT%H:%M:%SZ").to_string(),
                horizon_link: last_horizon_link,
                explorer_link: last_explorer_link,
                resolved: true,
            })
        }
        // No change in state.
        _ => None,
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
        }
    }

    fn run(rules: &[AlertRule], tx: &EnrichedTransaction) -> Vec<AlertPayload> {
        let contract = txwatch_config::WatchedContract {
            label: "Label".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network: txwatch_config::Network::Testnet,
            rules: rules.to_vec(),
            webhook_url: "https://hooks.example.com/hook".into(),
            webhook_secret: None,
            poll_interval_seconds: None,
            horizon_base_url_override: None,
        };
        let ctx = EvalContext::from_contract(&contract);
        evaluate(&ctx, rules, tx, None)
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
        fn run_with_bases(horizon_base: &str, explorer_base: Option<&str>) -> AlertPayload {
            let tx = EnrichedTransaction {
                hash: "deadbeef".into(),
                timestamp: "2024-01-15T12:00:00Z".parse().unwrap(),
                successful: true,
                paging_token: "1".into(),
                function_names: vec![],
                amount_stroops: None,
                fee_charged_stroops: None,
            };
            let ctx = EvalContext {
                label: "L",
                contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
                network: "testnet",
                horizon_base,
                explorer_base,
            };
            let mut payloads = evaluate(
                &ctx,
                &[AlertRule::AnyTransaction],
                &tx,
                None,
            );
            payloads.remove(0)
        }

        // Without trailing slash — baseline
        let p = run_with_bases(
            "https://horizon-testnet.stellar.org",
            Some("https://stellar.expert/explorer/testnet"),
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
            Some("https://stellar.expert/explorer/testnet/"),
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
            resolved: false,
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
        assert_eq!(obj["resolved"].as_bool(), Some(false));
    }
}

// ── Property-based tests (#61) ────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;
    use txwatch_config::AlertRule;

    /// Minimal valid `EnrichedTransaction` for property tests.
    fn arb_tx(
        amount_stroops: Option<u64>,
        fee_stroops: Option<u64>,
        function_names: Vec<String>,
        successful: bool,
    ) -> EnrichedTransaction {
        EnrichedTransaction {
            hash: "proptesthash".into(),
            timestamp: "2024-01-01T00:00:00Z".parse().unwrap(),
            successful,
            paging_token: "1".into(),
            function_names,
            amount_stroops,
            fee_charged_stroops: fee_stroops,
        }
    }

    fn eval_one(rule: AlertRule, tx: &EnrichedTransaction) -> Vec<AlertPayload> {
        let contract = txwatch_config::WatchedContract {
            label: "PropTest".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network: txwatch_config::Network::Testnet,
            rules: vec![rule.clone()],
            webhook_url: "https://hooks.example.com/hook".into(),
            webhook_secret: None,
            poll_interval_seconds: None,
            horizon_base_url_override: None,
        };
        let ctx = EvalContext::from_contract(&contract);
        evaluate(&ctx, &[rule], tx, None)
    }

    proptest! {
        /// LargeTransfer fires iff amount_stroops >= threshold_xlm × 10^7,
        /// for all valid (non-zero, non-overflowing) threshold values.
        #[test]
        fn large_transfer_fires_iff_at_or_above_threshold(
            amount_stroops in 0u64..=u64::MAX / 2,
            threshold_xlm in 1u64..=1_000_000_000u64,
        ) {
            let tx = arb_tx(Some(amount_stroops), None, vec![], true);
            let rule = AlertRule::LargeTransfer { threshold_xlm };
            let payloads = eval_one(rule, &tx);
            let threshold_stroops = threshold_xlm.saturating_mul(10_000_000);
            let should_fire = amount_stroops >= threshold_stroops;
            prop_assert_eq!(payloads.len() == 1, should_fire,
                "amount={} threshold_xlm={} threshold_stroops={} should_fire={}",
                amount_stroops, threshold_xlm, threshold_stroops, should_fire);
        }

        /// LargeTransfer never panics for any u64 threshold and amount combination.
        #[test]
        fn large_transfer_never_panics(
            amount_stroops in 0u64..=u64::MAX,
            threshold_xlm in 0u64..=u64::MAX,
        ) {
            let tx = arb_tx(Some(amount_stroops), None, vec![], true);
            // Use u64::MAX as threshold to exercise checked_mul overflow path.
            // validate() rejects threshold_xlm=0 so we can pass any value here
            // directly — eval_rule returns Ok(false) on overflow.
            let rule = AlertRule::LargeTransfer { threshold_xlm };
            // Must not panic regardless of inputs.
            let _ = eval_one(rule, &tx);
        }

        /// HighFee fires iff fee_charged_stroops >= threshold_stroops, for all u64 values.
        #[test]
        fn high_fee_fires_iff_at_or_above_threshold(
            fee in 0u64..=u64::MAX,
            threshold in 1u64..=u64::MAX,
        ) {
            let tx = arb_tx(None, Some(fee), vec![], true);
            let rule = AlertRule::HighFee { threshold_stroops: threshold, threshold_xlm: None };
            let payloads = eval_one(rule, &tx);
            let should_fire = fee >= threshold;
            prop_assert_eq!(payloads.len() == 1, should_fire,
                "fee={} threshold={} should_fire={}", fee, threshold, should_fire);
        }

        /// evaluate never panics for arbitrary EnrichedTransaction inputs.
        #[test]
        fn evaluate_never_panics(
            amount_stroops in proptest::option::of(0u64..=u64::MAX),
            fee_stroops in proptest::option::of(0u64..=u64::MAX),
            successful in proptest::bool::ANY,
        ) {
            let tx = arb_tx(amount_stroops, fee_stroops, vec![], successful);
            let rules = vec![
                AlertRule::AnyTransaction,
                AlertRule::TransactionFailed,
                AlertRule::LargeTransfer { threshold_xlm: 1 },
                AlertRule::HighFee { threshold_stroops: 1, threshold_xlm: None },
            ];
            let contract = txwatch_config::WatchedContract {
                label: "PropTest".into(),
                contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
                network: txwatch_config::Network::Testnet,
                rules: rules.clone(),
                webhook_url: "https://hooks.example.com/hook".into(),
                webhook_secret: None,
                poll_interval_seconds: None,
                horizon_base_url_override: None,
            };
            let ctx = EvalContext::from_contract(&contract);
            // Must not panic.
            let _ = evaluate(&ctx, &rules, &tx, None);
        }
    }
}

// ── NoActivity tests (#62) ────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod no_activity_tests {
    use super::*;
    use chrono::TimeZone;
    use txwatch_config::AlertRule;

    fn ctx() -> txwatch_config::WatchedContract {
        txwatch_config::WatchedContract {
            label: "Oracle".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network: txwatch_config::Network::Testnet,
            rules: vec![AlertRule::NoActivity { minutes: 5 }],
            webhook_url: "https://hooks.example.com/hook".into(),
            webhook_secret: None,
            poll_interval_seconds: None,
            horizon_base_url_override: None,
        }
    }

    fn ts(h: i32, m: i32) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2024, 1, 15, h as u32, m as u32, 0).unwrap()
    }

    #[test]
    fn no_activity_fires_when_threshold_exceeded() {
        let contract = ctx();
        let eval_ctx = EvalContext::from_contract(&contract);
        let rule = AlertRule::NoActivity { minutes: 5 };
        let mut state = NoActivityState::default();

        // Last seen 6 minutes ago — threshold is 5 min, so this should fire.
        let last_seen = ts(12, 0);
        let now = ts(12, 6);
        let payload = check_no_activity(&rule, Some(last_seen), now, &mut state, &eval_ctx);

        assert!(payload.is_some(), "should fire when quiet for > threshold");
        let p = payload.unwrap();
        assert_eq!(p.rule_type, "NoActivity");
        assert!(!p.resolved, "incident payload must have resolved=false");
        assert!(p.transaction_hash.is_empty(), "synthetic payload has empty hash");
        assert_eq!(state, NoActivityState::Alerting);
    }

    #[test]
    fn no_activity_does_not_fire_below_threshold() {
        let contract = ctx();
        let eval_ctx = EvalContext::from_contract(&contract);
        let rule = AlertRule::NoActivity { minutes: 5 };
        let mut state = NoActivityState::default();

        // Last seen 4 minutes ago — below threshold.
        let last_seen = ts(12, 0);
        let now = ts(12, 4);
        let payload = check_no_activity(&rule, Some(last_seen), now, &mut state, &eval_ctx);

        assert!(payload.is_none());
        assert_eq!(state, NoActivityState::Active);
    }

    #[test]
    fn no_activity_fires_exactly_at_threshold() {
        let contract = ctx();
        let eval_ctx = EvalContext::from_contract(&contract);
        let rule = AlertRule::NoActivity { minutes: 5 };
        let mut state = NoActivityState::default();

        // Exactly 5 minutes gap — should fire.
        let last_seen = ts(12, 0);
        let now = ts(12, 5);
        let payload = check_no_activity(&rule, Some(last_seen), now, &mut state, &eval_ctx);

        assert!(payload.is_some(), "should fire at exactly threshold");
    }

    #[test]
    fn no_activity_does_not_repeat_while_alerting() {
        let contract = ctx();
        let eval_ctx = EvalContext::from_contract(&contract);
        let rule = AlertRule::NoActivity { minutes: 5 };
        let mut state = NoActivityState::Alerting;

        // Still quiet — already alerting, so no second payload.
        let last_seen = ts(12, 0);
        let now = ts(12, 20);
        let payload = check_no_activity(&rule, Some(last_seen), now, &mut state, &eval_ctx);

        assert!(payload.is_none(), "should not re-fire while already alerting");
        assert_eq!(state, NoActivityState::Alerting);
    }

    #[test]
    fn no_activity_fires_recovery_when_activity_resumes() {
        let contract = ctx();
        let eval_ctx = EvalContext::from_contract(&contract);
        let rule = AlertRule::NoActivity { minutes: 5 };
        let mut state = NoActivityState::Alerting;

        // Activity just happened (1 minute ago) while we were in Alerting state.
        let last_seen = ts(12, 10);
        let now = ts(12, 11);
        let payload = check_no_activity(&rule, Some(last_seen), now, &mut state, &eval_ctx);

        assert!(payload.is_some(), "should fire recovery when activity resumes");
        let p = payload.unwrap();
        assert!(p.resolved, "recovery payload must have resolved=true");
        assert_eq!(p.rule_type, "NoActivity");
        assert_eq!(state, NoActivityState::Active);
    }

    #[test]
    fn no_activity_no_last_seen_fires_immediately() {
        let contract = ctx();
        let eval_ctx = EvalContext::from_contract(&contract);
        let rule = AlertRule::NoActivity { minutes: 5 };
        let mut state = NoActivityState::default();

        // No transactions ever seen — treated as infinite quiet period.
        let now = ts(12, 0);
        let payload = check_no_activity(&rule, None, now, &mut state, &eval_ctx);

        assert!(payload.is_some(), "should fire when no transactions ever seen");
        assert!(!payload.unwrap().resolved);
    }
}

// ── WarningSuppressor tests (#60) ─────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod suppressor_tests {
    use super::*;

    #[test]
    fn first_warning_is_always_emitted() {
        let s = WarningSuppressor::default();
        assert!(s.should_warn("contract:rule"), "first occurrence must warn");
    }

    #[test]
    fn second_through_99th_are_suppressed() {
        let s = WarningSuppressor::default();
        s.should_warn("k"); // first — emitted
        for _ in 2..100 {
            assert!(!s.should_warn("k"), "occurrences 2-99 must be suppressed");
        }
    }

    #[test]
    fn hundredth_occurrence_is_emitted() {
        let s = WarningSuppressor::default();
        for _ in 0..99 {
            s.should_warn("k");
        }
        assert!(s.should_warn("k"), "100th occurrence must be emitted");
    }

    #[test]
    fn different_keys_are_independent() {
        let s = WarningSuppressor::default();
        assert!(s.should_warn("a"));
        assert!(s.should_warn("b"));
    }

    #[test]
    fn suppressed_count_tracks_calls() {
        let s = WarningSuppressor::default();
        s.should_warn("x");
        s.should_warn("x");
        s.should_warn("x");
        assert_eq!(s.suppressed_count("x"), 3);
    }
}
