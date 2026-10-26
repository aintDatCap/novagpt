use serde_json::{Value, json};

#[test]
fn explicitly_enabled_vm_inventory_is_reachable_over_real_mcp_stdio() {
    let workspace = tempfile::tempdir().expect("fixture");
    let vm_home = tempfile::tempdir().expect("VM home");
    let private = tempfile::tempdir().expect("isolated state");
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_novagpt"))
        .arg("--root")
        .arg(workspace.path())
        .arg("--vm-home")
        .arg(vm_home.path())
        .env(
            if cfg!(windows) {
                "LOCALAPPDATA"
            } else {
                "XDG_STATE_HOME"
            },
            private.path(),
        )
        .arg("stdio")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("start");
    {
        let mut input = child.stdin.take().expect("stdin");
        for message in [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"nova-vm-test","version":"0.0.1"}
            }}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{
                "name":"vm","arguments":{"action":"images"}
            }}),
        ] {
            writeln!(input, "{message}").expect("request");
        }
    }
    let output = child.wait_with_output().expect("responses");
    assert!(
        output.status.success(),
        "server failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<Value> = String::from_utf8(output.stdout)
        .expect("utf8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("json rpc"))
        .collect();
    let response = replies
        .iter()
        .find(|value| value["id"] == 2)
        .expect("vm response");
    assert_eq!(response["result"]["structuredContent"]["action"], "images");
    assert_eq!(response["result"]["structuredContent"]["image_count"], 0);
    assert_eq!(response["result"]["isError"], false);
}

use std::{
    fs,
    io::Write,
    process::{Command, Stdio},
};

#[test]
fn stdio_initialize_list_and_call_use_the_real_protocol() {
    let workspace = tempfile::tempdir().expect("create isolated fixture root");
    let private = tempfile::tempdir().expect("isolated state");
    fs::write(workspace.path().join("hello.txt"), "test text").expect("write fixture");
    fs::write(
        workspace.path().join("image.png"),
        b"\x89PNG\r\n\x1a\nfixture",
    )
    .expect("image fixture");
    fs::write(workspace.path().join(".env"), "do not expose").expect("write blocked fixture");
    fs::write(
        workspace.path().join("AGENTS.md"),
        "Test workspace instructions",
    )
    .expect("instructions fixture");
    fs::create_dir_all(workspace.path().join(".ai-bridge")).expect("bridge fixture");
    fs::write(
        workspace.path().join(".ai-bridge/handoff-run-state.json"),
        r#"{"state":"completed","plan_hash":"abcd1234","iteration":3,"exit_code":0}"#,
    )
    .expect("handoff fixture");

    let mut child = Command::new(env!("CARGO_BIN_EXE_novagpt"))
        .arg("--root")
        .arg(workspace.path())
        .env(
            if cfg!(windows) {
                "LOCALAPPDATA"
            } else {
                "XDG_STATE_HOME"
            },
            private.path(),
        )
        .arg("stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start nova server");
    {
        let mut input = child.stdin.take().expect("take stdin");
        let messages = [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":"2025-11-25",
                "capabilities":{},
                "clientInfo":{"name":"nova-smoke","version":"0.0.1"}
            }}),
            json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
            json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{
                "name":"read","arguments":{"path":"hello.txt"}
            }}),
            json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{
                "name":"read","arguments":{"path":".env"}
            }}),
            json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{
                "name":"vm","arguments":{"action":"images"}
            }}),
            json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{
                "name":"subagent_status","arguments":{}
            }}),
            json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{
                "name":"view_image","arguments":{"path":"image.png"}
            }}),
            json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{
                "name":"codex_context","arguments":{"target_path":"hello.txt"}
            }}),
            json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{
                "name":"wait_for_handoff","arguments":{"plan_hash":"abcd1234","max_wait_seconds":1}
            }}),
            json!({"jsonrpc":"2.0","id":10,"method":"tools/call","params":{
                "name":"export_pro_context","arguments":{
                    "title":"Selected fixture","selected_paths":["hello.txt"]
                }
            }}),
            json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{
                "name":"browser_open","arguments":{}
            }}),
            json!({"jsonrpc":"2.0","id":12,"method":"tools/call","params":{
                "name":"browser_navigate","arguments":{"url":"https://example.org/"}
            }}),
        ];
        for message in messages {
            writeln!(input, "{message}").expect("send MCP request");
        }
    }
    let output = child.wait_with_output().expect("read MCP output");
    assert!(
        output.status.success(),
        "server exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let replies: Vec<Value> = String::from_utf8(output.stdout)
        .expect("MCP response must be UTF-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("one JSON-RPC response per line"))
        .collect();
    let by_id = |id: i64| {
        replies
            .iter()
            .find(|reply| reply["id"] == id)
            .expect("matching response")
    };
    assert_eq!(by_id(1)["result"]["serverInfo"]["name"], "novagpt");
    let listed = by_id(2)["result"]["tools"]
        .as_array()
        .expect("tools/list array");
    for name in [
        "read",
        "write",
        "view_image",
        "git",
        "git_status",
        "git_diff",
        "show_changes",
        "bash",
        "read_output",
        "apply_patch",
        "import_file",
        "browser_tabs",
        "browser_tab_read",
        "runtime_status",
        "reconnect_workspace",
        "codex_context",
        "server_config",
        "list_workspaces",
        "workspace_snapshot",
        "inspect_workspace",
        "handoff_to_agent",
        "read_handoff",
        "wait_for_handoff",
        "export_pro_context",
        "load_skill",
        "vm",
        "vm_guest_status",
        "browser_open",
        "browser_navigate",
        "browser_read",
        "browser_close",
        "subagent_spawn",
        "subagent_message",
        "subagent_resume",
        "subagent_browser_diagnostics",
        "subagent_status",
        "subagent_result",
        "subagent_cancel",
    ] {
        assert!(
            listed.iter().any(|tool| tool["name"] == name),
            "missing tool: {name}"
        );
    }
    assert_eq!(listed.len(), 43);
    assert_eq!(by_id(3)["result"]["structuredContent"]["text"], "test text");
    assert_eq!(
        by_id(7)["result"]["structuredContent"]["mime_type"],
        "image/png"
    );
    assert_eq!(by_id(7)["result"]["content"][0]["type"], "image");
    assert_eq!(by_id(11)["result"]["isError"], true);
    assert_eq!(by_id(12)["result"]["isError"], true);
    assert_eq!(by_id(10)["result"]["isError"], false);
    assert_eq!(
        by_id(10)["result"]["structuredContent"]["files_included"][0],
        "hello.txt"
    );
    assert_eq!(
        by_id(9)["result"]["structuredContent"]["awaited_completed"],
        true
    );
    assert_eq!(
        by_id(8)["result"]["structuredContent"]["instructions_changed"],
        true
    );
    assert!(
        by_id(8)["result"]["structuredContent"]["text"]
            .as_str()
            .unwrap_or("")
            .contains("Test workspace instructions")
    );
    assert_eq!(by_id(4)["result"]["isError"], true);
    assert!(
        by_id(4)["result"]["structuredContent"]["error"]
            .as_str()
            .unwrap_or("")
            .contains("blocked")
    );
    for id in [5, 6] {
        assert_eq!(by_id(id)["result"]["isError"], true);
        assert!(
            by_id(id)["result"]["structuredContent"]["error"]
                .as_str()
                .unwrap_or("")
                .contains("disabled")
        );
    }
}
