use std::{
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const CONTRACT_ID: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";

fn txwatch() -> Command {
    Command::new(env!("CARGO_BIN_EXE_txwatch"))
}

fn fresh_output(name: &str) -> PathBuf {
    let path = env::temp_dir().join(format!("txwatch_init_{name}.toml"));
    let _ = fs::remove_file(&path);
    path
}

fn init(output: &Path, extra: &[&str]) -> Output {
    txwatch()
        .args(["init", "--output", output.to_str().unwrap()])
        .args(extra)
        .output()
        .expect("failed to run txwatch")
}

#[test]
fn init_writes_a_config_that_validates() {
    let output = fresh_output("valid");
    let result = init(
        &output,
        &[
            "--contract-id",
            CONTRACT_ID,
            "--network",
            "mainnet",
            "--webhook-url",
            "https://hooks.example.com/alerts",
        ],
    );
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    let written = fs::read_to_string(&output).unwrap();
    assert!(written.contains(CONTRACT_ID));
    assert!(written.contains(r#"network     = "mainnet""#));

    let validate = txwatch()
        .args(["--config", output.to_str().unwrap(), "validate"])
        .status()
        .unwrap();
    assert!(
        validate.success(),
        "the generated config must pass validate"
    );
}

#[test]
fn init_refuses_to_overwrite_without_force() {
    let output = fresh_output("exists");
    fs::write(&output, "keep me").unwrap();
    let args = [
        "--contract-id",
        CONTRACT_ID,
        "--network",
        "testnet",
        "--webhook-url",
        "https://hooks.example.com/alerts",
    ];

    let result = init(&output, &args);
    assert_eq!(result.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&result.stderr).contains("--force"));
    assert_eq!(fs::read_to_string(&output).unwrap(), "keep me");

    let mut forced = args.to_vec();
    forced.push("--force");
    assert!(init(&output, &forced).status.success());
    assert!(fs::read_to_string(&output).unwrap().contains(CONTRACT_ID));
}

#[test]
fn init_rejects_invalid_input_without_writing() {
    let output = fresh_output("invalid");
    let result = init(
        &output,
        &[
            "--contract-id",
            "not-a-contract",
            "--network",
            "testnet",
            "--webhook-url",
            "https://hooks.example.com/alerts",
        ],
    );
    assert_eq!(result.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&result.stderr).contains("not a valid Stellar contract address")
    );
    assert!(!output.exists(), "nothing is written when validation fails");
}

#[test]
fn init_prompts_for_missing_values() {
    let output = fresh_output("prompted");
    let mut child = txwatch()
        .args(["init", "--output", output.to_str().unwrap()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Contract ID, network (blank = testnet), webhook URL.
    write!(
        child.stdin.take().unwrap(),
        "{CONTRACT_ID}\n\nhttps://hooks.example.com/prompted\n"
    )
    .unwrap();
    let result = child.wait_with_output().unwrap();
    assert!(
        result.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&result.stderr)
    );

    let written = fs::read_to_string(&output).unwrap();
    assert!(written.contains(r#"network     = "testnet""#));
    assert!(written.contains("https://hooks.example.com/prompted"));
}
