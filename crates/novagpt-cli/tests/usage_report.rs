//! Smoke test the public, read-only CLI contract without enabling collection.
use serde_json::Value;
use std::process::Command;

#[test]
fn usage_report_is_sanitized_and_read_only_with_no_records() {
    let workspace = tempfile::tempdir().expect("temporary workspace");
    let result = Command::new(env!("CARGO_BIN_EXE_novagpt"))
        .arg("--root")
        .arg(workspace.path())
        .args(["usage", "report", "--json"])
        .output()
        .expect("usage report command");
    assert!(result.status.success(), "status: {:?}", result.status);
    let report: Value = serde_json::from_slice(&result.stdout).expect("typed JSON");
    assert_eq!(report["totals"]["calls"], 0);
    assert_eq!(report["totals"]["provider_input"], 0);
    assert_eq!(report["totals"]["estimated_output"], 0);
    assert!(report["by_task"].as_object().is_some());
    assert!(report["by_stage"].as_object().is_some());
    assert!(!workspace.path().join(".novagpt").exists());
    let plain = Command::new(env!("CARGO_BIN_EXE_novagpt"))
        .arg("--root")
        .arg(workspace.path())
        .args(["usage", "report"])
        .output()
        .expect("usage text report");
    assert!(plain.status.success());
    let output = String::from_utf8(plain.stdout).expect("UTF-8 text");
    assert!(output.contains("NOT provider-billed tokens"));
    assert!(output.contains("Provider cost: unavailable"));
}
