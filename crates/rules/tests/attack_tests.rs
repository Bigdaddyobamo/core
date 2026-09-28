//! Attack tests for `txwatch-rules` verifying defenses against numeric overflow,
//! symbol spoofing, and evasion attacks.

use txwatch_config::AlertRule;
use txwatch_rules::{evaluate, EnrichedTransaction, HorizonTransaction};

fn dummy_tx(hash: &str, successful: bool, fee_charged: Option<&str>) -> HorizonTransaction {
    HorizonTransaction {
        hash: hash.into(),
        created_at: "2024-06-01T12:00:00Z".into(),
        successful,
        paging_token: "1000".into(),
        fee_charged: fee_charged.map(|s| s.into()),
        envelope_xdr: None,
        result_xdr: None,
    }
}

#[test]
fn test_attack_overflow_amount_handled_safely() {
    // Attack with maximum u64 value for amount_stroops to verify no panic occurs
    let raw = dummy_tx("overflow_tx", true, None);
    let enriched = EnrichedTransaction::from_horizon(raw, vec!["transfer".into()], Some(u64::MAX), None).unwrap();

    let rules = vec![AlertRule::LargeTransfer { threshold_xlm: 10_000 }];
    let payloads = evaluate(
        "Escrow",
        "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "testnet",
        "https://horizon-testnet.stellar.org",
        "https://stellar.expert/explorer/testnet",
        &rules,
        &enriched,
    );

    assert_eq!(payloads.len(), 1, "LargeTransfer must safely detect overflow-scale amount without panicking");
}

#[test]
fn test_attack_case_spoofing_admin_function() {
    // Attack trying to bypass AdminFunctionCalled rule using mixed-case invocation
    let raw = dummy_tx("admin_tx", true, None);
    let enriched = EnrichedTransaction::from_horizon(raw, vec!["Set_Admin".into()], None, None).unwrap();

    let rules = vec![AlertRule::AdminFunctionCalled {
        function_names: vec!["set_admin".into(), "upgrade".into()],
    }];
    let payloads = evaluate(
        "Escrow",
        "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "testnet",
        "https://horizon-testnet.stellar.org",
        "https://stellar.expert/explorer/testnet",
        &rules,
        &enriched,
    );

    assert_eq!(payloads.len(), 1, "AdminFunctionCalled must catch case variations of sensitive functions");
}

#[test]
fn test_attack_status_spoofing_transaction_failed() {
    // Ensure successful transaction cannot trick TransactionFailed rule into firing
    let raw = dummy_tx("success_tx", true, None);
    let enriched = EnrichedTransaction::from_horizon(raw, vec![], None, None).unwrap();

    let rules = vec![AlertRule::TransactionFailed];
    let payloads = evaluate(
        "Escrow",
        "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        "testnet",
        "https://horizon-testnet.stellar.org",
        "https://stellar.expert/explorer/testnet",
        &rules,
        &enriched,
    );

    assert!(payloads.is_empty(), "TransactionFailed must never trigger on successful transactions");
}
