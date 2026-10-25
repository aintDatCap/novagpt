//! Real stdio MCP boundary acceptance for the initial Git/Bash/patch slice.
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

fn git(root: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .expect("git executable");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn initial_git_bash_patch_protocol_and_negative_paths() {
    let fixture = tempfile::tempdir().expect("isolated root");
    git(fixture.path(), &["init", "-q"]);
    git(
        fixture.path(),
        &["config", "user.email", "fixture@example.test"],
    );
    git(fixture.path(), &["config", "user.name", "Fixture"]);
    fs::write(fixture.path().join("sample.txt"), "before\n").expect("text");
    git(fixture.path(), &["add", "sample.txt"]);
    git(fixture.path(), &["commit", "-qm", "initial"]);
    fs::write(fixture.path().join("sample.txt"), "after\n").expect("working change");

    let mut child = Command::new(env!("CARGO_BIN_EXE_novagpt"))
        .arg("--root")
        .arg(fixture.path())
        .arg("--bash-mode")
        .arg("safe")
        .arg("stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("server");
    {
        let mut input = child.stdin.take().expect("stdin");
        let items = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"nova-a-test","version":"0.1"}
            }}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"git_status","arguments":{}}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"bash","arguments":{"command":"cargo --version"}}}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"bash","arguments":{"command":"cargo --version && echo bad"}}}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"apply_patch","arguments":{
                "patch":"--- a/sample.txt\n+++ b/sample.txt\n@@ -1 +1 @@\n-after\n+final\n"
            }}}),
            json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"git_diff","arguments":{"path":"sample.txt"}}}),
            json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"git","arguments":{"action":"stage","files":["../outside"]}}}),
            json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"read","arguments":{"path":"sample.txt"}}}),
        ];
        for item in items {
            writeln!(input, "{item}").expect("write MCP request");
        }
    }
    let output = child.wait_with_output().expect("responses");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let messages: Vec<Value> = String::from_utf8(output.stdout)
        .expect("utf8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid json rpc"))
        .collect();
    let get = |id| {
        messages
            .iter()
            .find(|value| value["id"] == id)
            .expect("response present")
    };
    assert_eq!(get(2)["result"]["isError"], false);
    assert_eq!(get(3)["result"]["structuredContent"]["exitCode"], 0);
    assert_eq!(get(4)["result"]["isError"], true);
    assert_eq!(get(5)["result"]["structuredContent"]["changed"], true);
    assert!(
        get(6)["result"]["structuredContent"]["stdout"]
            .as_str()
            .unwrap_or("")
            .contains("+")
    );
    assert_eq!(get(7)["result"]["isError"], true);
    assert!(
        get(8)["result"]["structuredContent"]["text"]
            .as_str()
            .unwrap_or("")
            .contains("after")
    );
    assert_eq!(
        fs::read_to_string(fixture.path().join("sample.txt")).expect("postcondition"),
        "final\n"
    );
}
