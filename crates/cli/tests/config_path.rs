use std::{env, fs, process::Command};

fn txwatch_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_txwatch"))
}

const VALID_CONFIG: &str = r#"
poll_interval_seconds = 10

[[contracts]]
label       = "From Env"
contract_id = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"
network     = "testnet"
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
"#;

/// Without --config, TXWATCH_CONFIG or ./txwatch.toml the CLI fails with a
/// helpful message instead of falling back to the example config. Closes #100.
#[test]
fn missing_default_config_fails_with_helpful_message() {
    let dir = env::temp_dir().join("txwatch_no_default_config");
    fs::create_dir_all(&dir).unwrap();
    let _ = fs::remove_file(dir.join("txwatch.toml"));

    let output = txwatch_bin()
        .current_dir(&dir)
        .env_remove("TXWATCH_CONFIG")
        .arg("validate")
        .output()
        .expect("failed to run txwatch");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("config file 'txwatch.toml' not found"),
        "{}",
        stderr
    );
    assert!(stderr.contains("TXWATCH_CONFIG"), "{}", stderr);
}

#[test]
fn config_path_is_read_from_txwatch_config_env_var() {
    let path = env::temp_dir().join("txwatch_env_var_config.toml");
    fs::write(&path, VALID_CONFIG).unwrap();

    let output = txwatch_bin()
        .env("TXWATCH_CONFIG", &path)
        .arg("validate")
        .output()
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("From Env"));
}

#[test]
fn default_config_path_is_txwatch_toml_in_working_directory() {
    let dir = env::temp_dir().join("txwatch_default_config");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("txwatch.toml"), VALID_CONFIG).unwrap();

    let output = txwatch_bin()
        .current_dir(&dir)
        .env_remove("TXWATCH_CONFIG")
        .arg("validate")
        .output()
        .expect("failed to run txwatch");

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("From Env"));
}
