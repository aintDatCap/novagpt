//! A tunnel must not be spawned if another process already owns the MCP port.
use serde_json::Value;
use std::{net::TcpListener, process::Command};

#[test]
fn ngrok_start_refuses_an_occupied_loopback_port_and_records_diagnostic() {
    let root = tempfile::tempdir().expect("workspace");
    let private = tempfile::tempdir().expect("isolated audit/token state");
    let occupied = TcpListener::bind("127.0.0.1:0").expect("occupied socket");
    let port = occupied.local_addr().expect("local port").port();
    let credential = "Q".repeat(40);
    let binary = env!("CARGO_BIN_EXE_novagpt");

    let mut server = Command::new(binary);
    server
        .arg("--root")
        .arg(root.path())
        .args(["http", "--ngrok", "--port"])
        .arg(port.to_string())
        .env("NOVAGPT_HTTP_TOKEN", &credential);
    if cfg!(windows) {
        server.env("LOCALAPPDATA", private.path());
    } else {
        server.env("XDG_STATE_HOME", private.path());
    }
    let outcome = server.output().expect("port conflict exits");
    assert!(
        !outcome.status.success(),
        "an owned port must prevent tunnel launch"
    );
    let stderr = String::from_utf8_lossy(&outcome.stderr);
    assert!(
        stderr.contains("Porta MCP occupata"),
        "specific safe error expected"
    );
    assert!(!stderr.contains(&credential), "never show credentials");

    let mut logs = Command::new(binary);
    logs.arg("--root")
        .arg(root.path())
        .args(["logs", "tail", "--json"]);
    if cfg!(windows) {
        logs.env("LOCALAPPDATA", private.path());
    } else {
        logs.env("XDG_STATE_HOME", private.path());
    }
    let output = logs.output().expect("persisted diagnostics");
    assert!(
        output.status.success(),
        "audit must be readable after early failure"
    );
    let snapshot: Value = serde_json::from_slice(&output.stdout).expect("safe JSON");
    assert!(snapshot.to_string().contains("http_bind_failed"));
    assert!(!snapshot.to_string().contains(&credential));
}
