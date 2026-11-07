//! CLI log history and portable support-bundle contract.
use sha2::{Digest, Sha256};
use std::{fs, process::Command};

#[test]
fn cli_tail_reads_beyond_memory_ring_and_exports_private_bundle() {
    let root = tempfile::tempdir().expect("workspace");
    let state = tempfile::tempdir().expect("isolated application state");
    let canonical = fs::canonicalize(root.path()).expect("canonical workspace");
    let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
    let logs = state
        .path()
        .join("NovaGPT")
        .join("logs")
        .join(hex::encode(digest));
    let audit = novagpt_mcp::AuditTrail::persistent(logs).expect("audit writer");
    let runtime = tokio::runtime::Runtime::new().expect("runtime");
    for i in 0..350 {
        audit.record("read", false);
        if i % 25 == 24 {
            runtime.block_on(audit.flush());
        }
    }
    runtime.block_on(audit.flush());
    drop(audit);

    let binary = env!("CARGO_BIN_EXE_novagpt");
    let mut tail = Command::new(binary);
    tail.arg("--root")
        .arg(root.path())
        .args(["logs", "tail", "--lines", "350"]);
    let mut export = Command::new(binary);
    let destination = state.path().join("novagpt-support.log.zst");
    export
        .arg("--root")
        .arg(root.path())
        .args(["logs", "export", "--output"])
        .arg(&destination);
    if cfg!(windows) {
        tail.env("LOCALAPPDATA", state.path());
        export.env("LOCALAPPDATA", state.path());
    } else {
        tail.env("XDG_STATE_HOME", state.path());
        export.env("XDG_STATE_HOME", state.path());
    }

    let history = tail.output().expect("CLI tail");
    assert!(
        history.status.success(),
        "{}",
        String::from_utf8_lossy(&history.stderr)
    );
    let output = String::from_utf8_lossy(&history.stdout);
    assert!(
        output.contains("Event #1 "),
        "earliest event should survive"
    );
    assert!(
        output.contains("Event #350 "),
        "latest event should survive"
    );
    assert_eq!(output.lines().count(), 350, "tail must not stop at 128");

    let exported = export.output().expect("CLI export");
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    assert!(String::from_utf8_lossy(&exported.stdout).contains("Exported 350 audit events"));
    let bundle = fs::read(&destination).expect("bundle");
    assert_eq!(&bundle[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
    let repeated = export.output().expect("no overwrite");
    assert!(
        !repeated.status.success(),
        "must not overwrite existing bundle"
    );
}
