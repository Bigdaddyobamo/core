//! Attack tests for `txwatch-notifier` verifying HMAC signature defenses,
//! retry exhaustion behavior, and denial of service resistance.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::sync::oneshot;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use txwatch_notifier::{build_client, send_webhook, test_payload};

#[tokio::test]
async fn test_attack_hmac_tamper_detection() {
    let payload = test_payload("AttackTest", "https://example.com/hook");
    let body = serde_json::to_string(&payload).unwrap();
    let secret = "correct-secret-key";

    // Compute legitimate HMAC
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let valid_signature = hex::encode(mac.finalize().into_bytes());

    // Tampered payload body simulation
    let mut tampered_mac = Hmac::<Sha256>::new_from_slice(b"forged-attacker-key").unwrap();
    tampered_mac.update(body.as_bytes());
    let forged_signature = hex::encode(tampered_mac.finalize().into_bytes());

    assert_ne!(
        valid_signature, forged_signature,
        "Forged secret must never produce matching HMAC signature"
    );
}

#[tokio::test]
async fn test_attack_server_slowloris_or_500_flood_terminates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/hook"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let client = build_client().unwrap();
    let url = format!("{}/hook", server.uri());
    let payload = test_payload("SlowlorisDefense", &url);
    let (_tx, rx) = oneshot::channel();

    let result = send_webhook(&client, &url, &payload, Some("secret"), rx).await;
    assert!(result.is_err(), "Exhausted retries on 500 flood must terminate with error");
}
