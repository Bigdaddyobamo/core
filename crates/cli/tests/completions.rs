use std::process::Command;

fn txwatch(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_txwatch"))
        .args(args)
        .output()
        .expect("failed to run txwatch")
}

#[test]
fn completions_cover_subcommands_and_flags() {
    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = txwatch(&["completions", shell]);
        assert!(output.status.success(), "{shell} completions failed");
        let script = String::from_utf8_lossy(&output.stdout);
        assert!(
            script.contains("validate"),
            "{shell}: missing validate subcommand"
        );
        assert!(
            script.contains("check-webhooks"),
            "{shell}: missing --check-webhooks"
        );
        assert!(script.contains("dry-run"), "{shell}: missing --dry-run");
    }
}

#[test]
fn completions_reject_an_unknown_shell() {
    assert!(!txwatch(&["completions", "tcsh"]).status.success());
}

#[test]
fn man_page_is_roff_for_txwatch() {
    let output = txwatch(&["man"]);
    assert!(output.status.success());
    let page = String::from_utf8_lossy(&output.stdout);
    assert!(
        page.starts_with(".ie") || page.contains(".TH txwatch"),
        "not a roff page: {}",
        &page[..page.len().min(200)]
    );
    assert!(page.contains("validate"));
}
