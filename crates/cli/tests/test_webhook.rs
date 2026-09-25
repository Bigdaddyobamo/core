use std::{env, fs, process::Command};

fn txwatch_bin() -> Command {
    let bin = env!("CARGO_BIN_EXE_txwatch");
    Command::new(bin)
}

const VALID_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "Test Contract"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
network     = "testnet"
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

#[test]
fn test_webhook_exits_one_when_url_is_unreachable() {
    let dir = env::temp_dir();
    let cfg_path = dir.join("txwatch_test_webhook_unreachable.toml");
    fs::write(&cfg_path, VALID_CONFIG).unwrap();

    // Port 1 on loopback is never open (requires root to bind), so the
    // connection is always refused — making this a reliable unreachable URL.
    let status = txwatch_bin()
        .args([
            "--config",
            cfg_path.to_str().unwrap(),
            "test-webhook",
            "--url",
            "http://127.0.0.1:1/webhook",
        ])
        .status()
        .expect("failed to run txwatch");

    assert_eq!(
        status.code(),
        Some(1),
        "expected exit code 1 when webhook URL is unreachable"
    );
}

#[test]
fn test_webhook_does_not_require_a_config_file() {
    let status = txwatch_bin()
        .args([
            "test-webhook",
            "--url",
            "http://127.0.0.1:1/webhook",
            "--network",
            "testnet",
            "--secret",
            "local-secret",
        ])
        .status()
        .expect("failed to run txwatch");
    assert_eq!(status.code(), Some(1));
}
