use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn setup_requires_interactive_terminal_and_never_writes_unattended() {
    let workspace = tempfile::tempdir().expect("workspace");
    let source = tempfile::tempdir().expect("codex profile home");
    let output = Command::new(env!("CARGO_BIN_EXE_novagpt"))
        .arg("--root")
        .arg(workspace.path())
        .arg("--codexpro-home")
        .arg(source.path())
        .arg("--setup")
        .stdin(Stdio::null())
        .output()
        .expect("run setup");
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive terminal"));
    assert!(!workspace.path().join(".novagpt").exists());
}

#[test]
fn omitted_root_uses_cwd_for_explicit_stdio_transport() {
    let workspace = tempfile::tempdir().expect("workspace");
    fs::write(workspace.path().join("cwd.txt"), "from cwd").expect("fixture");
    let mut child = Command::new(env!("CARGO_BIN_EXE_novagpt"))
        .current_dir(workspace.path())
        .arg("stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start stdio");
    let mut input = child.stdin.take().expect("stdin");
    for request in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"cwd-test","version":"0.0.1"}
        }}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
            "name":"read","arguments":{"path":"cwd.txt"}
        }}),
    ] {
        writeln!(input, "{request}").expect("RPC");
    }
    drop(input);
    let result = child.wait_with_output().expect("RPC results");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let responses: Vec<Value> = String::from_utf8(result.stdout)
        .expect("UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid RPC"))
        .collect();
    assert_eq!(
        responses.iter().find(|v| v["id"] == 2).expect("result")["result"]["structuredContent"]["text"],
        "from cwd"
    );
}
