use serde_json::Value;
use std::{fs, process::Command};

fn run(root: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_novagpt"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .expect("NovaGPT command")
}

#[test]
fn doctor_and_inspect_are_read_only_and_structured() {
    let workspace = tempfile::tempdir().expect("root");
    fs::write(workspace.path().join("hello.rs"), "fn main() {}\n").expect("file");
    fs::write(workspace.path().join(".env"), "private").expect("fixture");

    let output = run(workspace.path(), &["doctor", "--json"]);
    assert!(output.status.success(), "{:?}", output.status);
    let doctor: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(doctor["feature_parity"], "INITIAL");
    assert_eq!(doctor["verified_remote_mcp"], false);
    assert!(
        doctor["workspace_id"]
            .as_str()
            .is_some_and(|s| s.starts_with("ws_"))
    );

    let output = run(workspace.path(), &["inspect", "--json"]);
    assert!(output.status.success());
    let summary: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(summary["files"], 1);
    assert!(!workspace.path().join(".novagpt").exists());
}

#[test]
fn review_reports_disposable_git_state() {
    let workspace = tempfile::tempdir().expect("root");
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(workspace.path())
        .output()
        .expect("Git");
    assert!(init.status.success());
    fs::write(workspace.path().join("test.txt"), "text").expect("fixture");
    let output = run(workspace.path(), &["review", "--json"]);
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(value["action"], "status");
    assert!(value["stdout"].as_str().unwrap_or("").contains("test.txt"));
}
