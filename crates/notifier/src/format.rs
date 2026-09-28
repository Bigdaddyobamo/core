//! Maps an [`AlertPayload`] to the body each supported receiver expects.
//!
//! - `txwatch`: the payload itself.
//! - `slack`: a Slack incoming-webhook message (`text` fallback plus Block Kit
//!   `blocks`).
//! - `discord`: a Discord webhook message (`content` plus one embed), with
//!   mentions disabled so a label can never ping `@everyone`.
//! - `pagerduty`: a PagerDuty Events API v2 `trigger` event whose `dedup_key`
//!   is the alert's `alert_id`.

use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use txwatch_config::WebhookFormat;
use txwatch_rules::AlertPayload;

/// Renders the request body for `format`. `routing_key` is required for
/// PagerDuty and ignored otherwise.
pub fn render_body(
    format: WebhookFormat,
    payload: &AlertPayload,
    routing_key: Option<&str>,
) -> Result<String> {
    let body = match format {
        WebhookFormat::Txwatch => serde_json::to_value(payload)?,
        WebhookFormat::Slack => slack(payload),
        WebhookFormat::Discord => discord(payload),
        WebhookFormat::Pagerduty => pagerduty(
            payload,
            routing_key.ok_or_else(|| anyhow!("PagerDuty destinations need a routing_key"))?,
        ),
    };
    Ok(serde_json::to_string(&body)?)
}

/// One-line summary shared by the chat formats.
fn headline(p: &AlertPayload) -> String {
    format!(
        "TxWatch alert: {} on {} ({})",
        p.rule_triggered, p.label, p.network
    )
}

/// Optional detail lines as (name, value) pairs, in display order.
fn details(p: &AlertPayload) -> Vec<(&'static str, String)> {
    let mut details = Vec::new();
    if let Some(xlm) = p.amount_xlm {
        details.push(("Amount", format!("{} XLM", xlm)));
    }
    if let Some(fee) = p.fee_charged_stroops {
        details.push(("Fee", format!("{} stroops", fee)));
    }
    if !p.function_names.is_empty() {
        details.push(("Functions", p.function_names.join(", ")));
    }
    details
}

/// Truncates to at most `max` characters, marking the cut with `…`.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max - 1).collect();
    out.push('…');
    out
}

// ── Slack ─────────────────────────────────────────────────────────────────────

/// Escapes the three characters Slack mrkdwn treats as control characters.
fn slack_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Slack incoming-webhook message.
pub fn slack(p: &AlertPayload) -> Value {
    let mut fields = vec![
        json!({ "type": "mrkdwn", "text": format!("*Contract*\n`{}`", p.contract_id) }),
        json!({
            "type": "mrkdwn",
            "text": format!("*Transaction*\n<{}|{}>", p.explorer_link, p.transaction_hash),
        }),
    ];
    for (name, value) in details(p) {
        fields.push(json!({
            "type": "mrkdwn",
            "text": format!("*{}*\n{}", name, slack_escape(&value)),
        }));
    }
    fields.push(json!({ "type": "mrkdwn", "text": format!("*Time*\n{}", p.timestamp_iso) }));

    json!({
        "text": slack_escape(&headline(p)),
        "blocks": [
            {
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": format!(
                        "*{}* on *{}* ({})",
                        slack_escape(&p.rule_triggered),
                        slack_escape(&p.label),
                        slack_escape(&p.network)
                    ),
                },
            },
            { "type": "section", "fields": fields },
            {
                "type": "context",
                "elements": [{
                    "type": "mrkdwn",
                    "text": format!(
                        "<{}|Explorer> · <{}|Horizon> · alert `{}`",
                        p.explorer_link, p.horizon_link, p.alert_id
                    ),
                }],
            },
        ],
    })
}

// ── Discord ───────────────────────────────────────────────────────────────────

/// Embed colour: red for failures and admin calls, blue otherwise.
fn discord_color(rule_type: &str) -> u32 {
    match rule_type {
        "TransactionFailed" | "AdminFunctionCalled" => 0xE7_4C_3C,
        _ => 0x34_98_DB,
    }
}

/// Discord webhook message.
pub fn discord(p: &AlertPayload) -> Value {
    let mut fields = vec![
        json!({ "name": "Contract", "value": format!("`{}`", p.contract_id), "inline": false }),
        json!({
            "name": "Transaction",
            "value": truncate(&format!("[{}]({})", p.transaction_hash, p.explorer_link), 1024),
            "inline": false,
        }),
    ];
    for (name, value) in details(p) {
        fields.push(json!({ "name": name, "value": truncate(&value, 1024), "inline": true }));
    }
    fields.push(json!({
        "name": "Horizon",
        "value": truncate(&format!("[transaction]({})", p.horizon_link), 1024),
        "inline": true,
    }));

    json!({
        "username": "TxWatch",
        "content": truncate(&headline(p), 2000),
        "allowed_mentions": { "parse": [] },
        "embeds": [{
            "title": truncate(&p.rule_triggered, 256),
            "url": p.explorer_link,
            "description": truncate(&format!("{} ({})", p.label, p.network), 4096),
            "color": discord_color(&p.rule_type),
            "timestamp": p.timestamp_iso,
            "fields": fields,
            "footer": { "text": format!("TxWatch · alert {}", p.alert_id) },
        }],
    })
}

// ── PagerDuty ─────────────────────────────────────────────────────────────────

/// PagerDuty severity: failed transactions are `error`, admin calls
/// `critical`, test webhooks `info`, everything else `warning`.
fn pagerduty_severity(rule_type: &str) -> &'static str {
    match rule_type {
        "TransactionFailed" => "error",
        "AdminFunctionCalled" => "critical",
        "TestWebhook" => "info",
        _ => "warning",
    }
}

/// PagerDuty Events API v2 `trigger` event. The full alert is attached as
/// `payload.custom_details`.
pub fn pagerduty(p: &AlertPayload, routing_key: &str) -> Value {
    json!({
        "routing_key": routing_key,
        "event_action": "trigger",
        "dedup_key": p.alert_id,
        "client": "TxWatch",
        "client_url": p.explorer_link,
        "links": [
            { "href": p.explorer_link, "text": "View transaction" },
            { "href": p.horizon_link, "text": "Horizon" },
        ],
        "payload": {
            "summary": truncate(
                &format!("{} — tx {}", headline(p), p.transaction_hash),
                1024,
            ),
            "source": p.contract_id,
            "severity": pagerduty_severity(&p.rule_type),
            "timestamp": p.timestamp_iso,
            "component": p.label,
            "group": p.network,
            "class": p.rule_type,
            "custom_details": p,
        },
    })
}


// ── Snapshot tests ────────────────────────────────────────────────────────────

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A LargeTransfer alert whose label needs escaping in Slack mrkdwn.
    fn fixture() -> AlertPayload {
        AlertPayload {
            alert_id: "3f2b9c1d8e7a6b5c4d3e2f1a0b9c8d7e".into(),
            label: "Escrow <main> & co".into(),
            contract_id: "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            network: "testnet".into(),
            rule_type: "LargeTransfer".into(),
            rule_triggered: "LargeTransfer(>=10000XLM)".into(),
            transaction_hash: "abc123".into(),
            function_name: Some("transfer".into()),
            function_names: vec!["transfer".into()],
            amount_xlm: Some(15000),
            fee_charged_stroops: Some(50000),
            timestamp: 1705320000,
            timestamp_iso: "2024-01-15T12:00:00Z".into(),
            horizon_link: "https://horizon-testnet.stellar.org/transactions/abc123".into(),
            explorer_link: "https://stellar.expert/explorer/testnet/tx/abc123".into(),
        }
    }

    fn render(format: WebhookFormat, payload: &AlertPayload) -> Value {
        serde_json::from_str(&render_body(format, payload, Some("R0UT1NGKEY")).unwrap()).unwrap()
    }

    fn snapshot(expected: &str) -> Value {
        serde_json::from_str(expected).expect("snapshot is valid JSON")
    }

    #[test]
    fn txwatch_format_is_the_alert_payload() {
        let p = fixture();
        assert_eq!(
            render(WebhookFormat::Txwatch, &p),
            serde_json::to_value(&p).unwrap()
        );
    }

    #[test]
    fn slack_snapshot() {
        let expected = r#"{
          "text": "TxWatch alert: LargeTransfer(&gt;=10000XLM) on Escrow &lt;main&gt; &amp; co (testnet)",
          "blocks": [
            { "type": "section",
              "text": { "type": "mrkdwn", "text": "*LargeTransfer(&gt;=10000XLM)* on *Escrow &lt;main&gt; &amp; co* (testnet)" } },
            { "type": "section", "fields": [
              { "type": "mrkdwn", "text": "*Contract*\n`CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA`" },
              { "type": "mrkdwn", "text": "*Transaction*\n<https://stellar.expert/explorer/testnet/tx/abc123|abc123>" },
              { "type": "mrkdwn", "text": "*Amount*\n15000 XLM" },
              { "type": "mrkdwn", "text": "*Fee*\n50000 stroops" },
              { "type": "mrkdwn", "text": "*Functions*\ntransfer" },
              { "type": "mrkdwn", "text": "*Time*\n2024-01-15T12:00:00Z" }
            ] },
            { "type": "context", "elements": [
              { "type": "mrkdwn", "text": "<https://stellar.expert/explorer/testnet/tx/abc123|Explorer> · <https://horizon-testnet.stellar.org/transactions/abc123|Horizon> · alert `3f2b9c1d8e7a6b5c4d3e2f1a0b9c8d7e`" }
            ] }
          ]
        }"#;
        assert_eq!(render(WebhookFormat::Slack, &fixture()), snapshot(expected));
    }

    #[test]
    fn discord_snapshot() {
        let expected = r#"{
          "username": "TxWatch",
          "content": "TxWatch alert: LargeTransfer(>=10000XLM) on Escrow <main> & co (testnet)",
          "allowed_mentions": { "parse": [] },
          "embeds": [{
            "title": "LargeTransfer(>=10000XLM)",
            "url": "https://stellar.expert/explorer/testnet/tx/abc123",
            "description": "Escrow <main> & co (testnet)",
            "color": 3447003,
            "timestamp": "2024-01-15T12:00:00Z",
            "fields": [
              { "name": "Contract", "value": "`CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA`", "inline": false },
              { "name": "Transaction", "value": "[abc123](https://stellar.expert/explorer/testnet/tx/abc123)", "inline": false },
              { "name": "Amount", "value": "15000 XLM", "inline": true },
              { "name": "Fee", "value": "50000 stroops", "inline": true },
              { "name": "Functions", "value": "transfer", "inline": true },
              { "name": "Horizon", "value": "[transaction](https://horizon-testnet.stellar.org/transactions/abc123)", "inline": true }
            ],
            "footer": { "text": "TxWatch · alert 3f2b9c1d8e7a6b5c4d3e2f1a0b9c8d7e" }
          }]
        }"#;
        assert_eq!(render(WebhookFormat::Discord, &fixture()), snapshot(expected));
    }

    #[test]
    fn pagerduty_snapshot() {
        let expected = r#"{
          "routing_key": "R0UT1NGKEY",
          "event_action": "trigger",
          "dedup_key": "3f2b9c1d8e7a6b5c4d3e2f1a0b9c8d7e",
          "client": "TxWatch",
          "client_url": "https://stellar.expert/explorer/testnet/tx/abc123",
          "links": [
            { "href": "https://stellar.expert/explorer/testnet/tx/abc123", "text": "View transaction" },
            { "href": "https://horizon-testnet.stellar.org/transactions/abc123", "text": "Horizon" }
          ],
          "payload": {
            "summary": "TxWatch alert: LargeTransfer(>=10000XLM) on Escrow <main> & co (testnet) — tx abc123",
            "source": "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "severity": "warning",
            "timestamp": "2024-01-15T12:00:00Z",
            "component": "Escrow <main> & co",
            "group": "testnet",
            "class": "LargeTransfer",
            "custom_details": {
              "alert_id": "3f2b9c1d8e7a6b5c4d3e2f1a0b9c8d7e",
              "label": "Escrow <main> & co",
              "contract_id": "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
              "network": "testnet",
              "rule_type": "LargeTransfer",
              "rule_triggered": "LargeTransfer(>=10000XLM)",
              "transaction_hash": "abc123",
              "function_name": "transfer",
              "function_names": ["transfer"],
              "amount_xlm": 15000,
              "fee_charged_stroops": 50000,
              "timestamp": 1705320000,
              "timestamp_iso": "2024-01-15T12:00:00Z",
              "horizon_link": "https://horizon-testnet.stellar.org/transactions/abc123",
              "explorer_link": "https://stellar.expert/explorer/testnet/tx/abc123"
            }
          }
        }"#;
        assert_eq!(render(WebhookFormat::Pagerduty, &fixture()), snapshot(expected));
    }

    #[test]
    fn optional_details_are_omitted_when_absent() {
        let mut p = fixture();
        p.amount_xlm = None;
        p.fee_charged_stroops = None;
        p.function_name = None;
        p.function_names.clear();

        let slack = render(WebhookFormat::Slack, &p);
        let names: Vec<&str> = slack["blocks"][1]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["text"].as_str().unwrap().split('\n').next().unwrap())
            .collect();
        assert_eq!(names, ["*Contract*", "*Transaction*", "*Time*"]);

        let discord = render(WebhookFormat::Discord, &p);
        let names: Vec<&str> = discord["embeds"][0]["fields"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Contract", "Transaction", "Horizon"]);
    }

    #[test]
    fn severity_and_colour_follow_the_rule_type() {
        let mut p = fixture();
        for (rule_type, severity, colour) in [
            ("TransactionFailed", "error", 0xE7_4C_3C),
            ("AdminFunctionCalled", "critical", 0xE7_4C_3C),
            ("TestWebhook", "info", 0x34_98_DB),
            ("AnyTransaction", "warning", 0x34_98_DB),
        ] {
            p.rule_type = rule_type.into();
            assert_eq!(
                render(WebhookFormat::Pagerduty, &p)["payload"]["severity"],
                severity
            );
            assert_eq!(render(WebhookFormat::Discord, &p)["embeds"][0]["color"], colour);
        }
    }

    #[test]
    fn pagerduty_requires_a_routing_key() {
        let err = render_body(WebhookFormat::Pagerduty, &fixture(), None).unwrap_err();
        assert!(err.to_string().contains("routing_key"), "got: {}", err);
    }

    #[test]
    fn long_text_is_truncated_to_provider_limits() {
        let mut p = fixture();
        p.rule_triggered = "x".repeat(300);
        let discord = render(WebhookFormat::Discord, &p);
        let title = discord["embeds"][0]["title"].as_str().unwrap();
        assert_eq!(title.chars().count(), 256);
        assert!(title.ends_with('\u{2026}'));

        p.label = "y".repeat(2000);
        let pd = render(WebhookFormat::Pagerduty, &p);
        assert_eq!(
            pd["payload"]["summary"].as_str().unwrap().chars().count(),
            1024
        );
    }
}
