//! Keeps the webhook payload examples in the docs in sync with `AlertPayload`.

use std::collections::BTreeSet;

use serde_json::{Map, Value};
use txwatch_rules::AlertPayload;

/// The first ```json block after the `## Webhook payload` heading.
fn payload_example(doc: &str) -> Map<String, Value> {
    let section = &doc[doc.find("## Webhook payload").expect("payload section")..];
    let start = section.find("```json").expect("json block") + "```json".len();
    let end = start + section[start..].find("```").expect("closing fence");
    serde_json::from_str(&section[start..end]).expect("example is valid JSON")
}

fn assert_matches_alert_payload(name: &str, doc: &str) {
    let example = payload_example(doc);
    let payload: AlertPayload = serde_json::from_value(Value::Object(example.clone()))
        .unwrap_or_else(|e| panic!("{name} example does not deserialize as AlertPayload: {e}"));
    let expected: BTreeSet<String> = serde_json::to_value(payload)
        .expect("serialize")
        .as_object()
        .expect("object")
        .keys()
        .cloned()
        .collect();
    let documented: BTreeSet<String> = example.keys().cloned().collect();
    assert_eq!(
        documented, expected,
        "{name} example fields differ from AlertPayload"
    );
}

#[test]
fn configuration_md_payload_example_matches_alert_payload() {
    assert_matches_alert_payload(
        "docs/configuration.md",
        include_str!("../../../docs/configuration.md"),
    );
}

#[test]
fn readme_payload_example_matches_alert_payload() {
    assert_matches_alert_payload("README.md", include_str!("../../../README.md"));
}
