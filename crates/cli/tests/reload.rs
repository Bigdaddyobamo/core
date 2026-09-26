//! SIGHUP config reload for `txwatch watch`. Closes #98.
#![cfg(unix)]

use std::{
    env, fs,
    io::{BufRead, BufReader},
    process::{Child, Command, Stdio},
    sync::mpsc::{channel, Receiver},
    thread,
    time::{Duration, Instant},
};

/// A contract on a custom network whose Horizon is never reachable, so the
/// watcher runs without touching the internet (polls just log an error).
fn contract(label: &str, contract_id: &str) -> String {
    format!(
        r#"
[[contracts]]
label       = "{label}"
contract_id = "{contract_id}"
network     = {{ horizon_url = "http://127.0.0.1:1" }}
webhook_url = "https://hooks.example.com/test"

  [[contracts.rules]]
  type = "AnyTransaction"
"#
    )
}

const ALPHA: &str = "CAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAABSC4";
const BETA: &str = "CAAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQCAIBAEAQC526";

fn wait_for(lines: &Receiver<String>, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
        match lines.recv_timeout(remaining) {
            Ok(line) if line.contains(needle) => return line,
            Ok(_) => {}
            Err(_) => break,
        }
    }
    panic!("timed out waiting for log line containing {:?}", needle);
}

fn sighup(child: &Child) {
    let status = Command::new("kill")
        .args(["-HUP", &child.id().to_string()])
        .status()
        .expect("failed to run kill");
    assert!(status.success());
}

#[test]
fn watch_reloads_config_on_sighup_and_keeps_old_config_when_invalid() {
    let path = env::temp_dir().join("txwatch_sighup_reload.toml");
    fs::write(
        &path,
        format!("poll_interval_seconds = 10\n{}", contract("Alpha", ALPHA)),
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args(["--config", path.to_str().unwrap(), "watch", "--dry-run"])
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to start txwatch");

    let stdout = child.stdout.take().unwrap();
    let (tx, lines) = channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });

    wait_for(&lines, "TxWatch polling engine started");

    // Invalid file: reload is rejected and the old config keeps running.
    fs::write(
        &path,
        format!("poll_interval_seconds = 1\n{}", contract("Alpha", ALPHA)),
    )
    .unwrap();
    sighup(&child);
    let failed = wait_for(&lines, "config reload failed");
    assert!(
        failed.contains("poll_interval_seconds must be >= 5"),
        "{}",
        failed
    );

    // Valid file with an extra contract: reload is applied.
    fs::write(
        &path,
        format!(
            "poll_interval_seconds = 10\n{}{}",
            contract("Alpha", ALPHA),
            contract("Beta", BETA)
        ),
    )
    .unwrap();
    sighup(&child);
    let reloaded = wait_for(&lines, "configuration reloaded");
    assert!(reloaded.contains("contracts=2"), "{}", reloaded);
    assert!(
        child.try_wait().unwrap().is_none(),
        "watcher must still be running"
    );

    let _ = child.kill();
    let _ = child.wait();
}
