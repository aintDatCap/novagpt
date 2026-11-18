//! Real MCP stdio round trip against an exclusively disposable blank QEMU disk.
//! No existing personal, installer or production VM is ever started.
#![cfg(target_os = "linux")]

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::Duration,
};

struct RpcClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl RpcClient {
    fn new(
        workspace: &std::path::Path,
        vm_home: &std::path::Path,
        state: &std::path::Path,
    ) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_novagpt"))
            .arg("--root")
            .arg(workspace)
            .arg("--vm-home")
            .arg(vm_home)
            .arg("stdio")
            .env("XDG_STATE_HOME", state)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("launch isolated NovaGPT MCP server");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut client = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let init = client.call_raw(
            "initialize",
            json!({
                "protocolVersion":"2025-11-25", "capabilities":{},
                "clientInfo":{"name":"vm-disposable-mcp-smoke","version":"0.0.1"}
            }),
        );
        assert_eq!(init["result"]["serverInfo"]["name"], "novagpt");
        writeln!(
            client.stdin,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        client
    }

    fn call_raw(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        writeln!(
            self.stdin,
            "{}",
            json!({
                "jsonrpc":"2.0", "id":id, "method":method, "params":params
            })
        )
        .expect("send request");
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read MCP response");
        assert!(!line.is_empty(), "MCP server unexpectedly exited");
        let value: Value = serde_json::from_str(&line).expect("MCP JSON response");
        assert_eq!(value["id"], id);
        value
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Value {
        let result = self.call_raw("tools/call", json!({"name":name,"arguments":arguments}));
        assert_eq!(
            result["result"]["isError"], false,
            "{name} failed: {result}"
        );
        result["result"]["structuredContent"].clone()
    }
}

impl Drop for RpcClient {
    fn drop(&mut self) {
        drop(self.stdin.flush());
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_vm_create_desktop_snapshots_and_destroy_disposable_guest() {
    let workspace = tempfile::tempdir().unwrap();
    let vm_storage = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let base = private.path().join("test.qcow2");
    let created = Command::new("qemu-img")
        .args(["create", "-f", "qcow2"])
        .arg(&base)
        .arg("32M")
        .output()
        .expect("qemu-img installed for test");
    assert!(created.status.success());
    let manager = novagpt_vm::VmManager::new(vm_storage.path()).unwrap();
    manager.import_image("mcp-fixture", &base, 1, 512).unwrap();
    drop(manager); // Release the exclusive home lease before starting MCP.

    let mut client = RpcClient::new(workspace.path(), vm_storage.path(), private.path());
    let available = client.tool("vm_image", json!({}));
    assert_eq!(available["image_count"], 1);
    let created = client.tool(
        "vm",
        json!({
            "action":"create", "image":"mcp-fixture",
            "cpus":1, "memory_mb":512, "networks":["none"],
            "display":true, "guest_agent":true
        }),
    );
    let id = created["instance"]["id"]
        .as_str()
        .expect("VM id")
        .to_owned();

    let test = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut shot = None;
        for _ in 0..15 {
            let result = client.call_raw(
                "tools/call",
                json!({
                    "name":"vm_desktop_screenshot","arguments":{"id":id}
                }),
            );
            if result["result"]["isError"] == false {
                shot = Some(result["result"]["structuredContent"].clone());
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let shot = shot.expect("QMP screenshot should be available on managed blank QEMU fork");
        assert_eq!(shot["mime_type"], "image/png");
        assert!(shot["width"].as_u64().unwrap_or(0) > 300);
        assert!(shot["data_base64"].as_str().unwrap_or("").len() > 100);

        client.tool(
            "vm_desktop_input",
            json!({"action":"keys","id":id,"keys":["a"]}),
        );
        client.tool(
            "vm_desktop_input",
            json!({"action":"pointer","id":id,"x":2048,"y":2048,"button":"left"}),
        );
        client.tool("vm", json!({"action":"stop","id":id,"force":true}));
        client.tool(
            "vm_snapshot",
            json!({"action":"create","id":id,"name":"clean"}),
        );
        let listed = client.tool("vm_snapshot", json!({"action":"list","id":id}));
        assert_eq!(listed["snapshots"][0]["name"], "clean");
        client.tool(
            "vm_snapshot",
            json!({"action":"restore","id":id,"name":"clean"}),
        );
        client.tool(
            "vm_snapshot",
            json!({"action":"delete","id":id,"name":"clean"}),
        );
        client.tool("vm", json!({"action":"start","id":id}));
        let status = client.tool("vm", json!({"action":"status","id":id}));
        assert_eq!(status["instance"]["state"], "running");
    }));

    let cleanup = client.call_raw(
        "tools/call",
        json!({
            "name":"vm","arguments":{"action":"destroy","id":id}
        }),
    );
    assert_eq!(
        cleanup["result"]["isError"], false,
        "disposable cleanup failure: {cleanup}"
    );
    assert!(!vm_storage.path().join("instances").join(&id).exists());
    if let Err(error) = test {
        std::panic::resume_unwind(error);
    }
}
