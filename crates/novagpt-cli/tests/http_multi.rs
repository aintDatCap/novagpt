//! Two independent real HTTP MCP clients observe one shared workspace.
use reqwest::{Client, header};
use serde_json::{Value, json};
use std::{
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    time::Duration,
};

struct ProcessGuard(Child);
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

async fn rpc(
    client: &Client,
    port: u16,
    token: &str,
    session: Option<&str>,
    body: Value,
) -> Result<(Value, Option<String>), Box<dyn std::error::Error>> {
    let mut request = client
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .bearer_auth(token)
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25")
        .json(&body);
    if let Some(id) = session {
        request = request.header("Mcp-Session-Id", id);
    }
    let response = request.send().await?;
    let status = response.status();
    let sid = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let text = response.text().await?;
    assert!(status.is_success(), "MCP status {status}, response omitted");
    let parsed = serde_json::from_str(&text).or_else(|_| {
        text.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .find_map(|line| serde_json::from_str(line).ok())
            .ok_or_else(|| serde_json::from_str::<Value>("not-json").expect_err("invalid"))
    })?;
    Ok((parsed, sid))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn separate_http_sessions_share_files_and_conflicts() {
    let root = tempfile::tempdir().expect("workspace");
    std::fs::write(root.path().join("shared.txt"), "original").expect("fixture");
    let private = tempfile::tempdir().expect("private state");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("temporary TCP port");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let token = "T".repeat(40);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_novagpt"));
    cmd.arg("--root")
        .arg(root.path())
        .arg("http")
        .arg("--port")
        .arg(port.to_string())
        .env("NOVAGPT_HTTP_TOKEN", &token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if cfg!(windows) {
        cmd.env("LOCALAPPDATA", private.path());
    } else {
        cmd.env("XDG_STATE_HOME", private.path());
    }
    let mut child = ProcessGuard(cmd.spawn().expect("launch"));
    let mut ready = false;
    for _ in 0..100 {
        if child.0.try_wait().expect("poll").is_some() {
            panic!("server terminated before readiness");
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(ready, "HTTP MCP server startup failed");
    let client = Client::builder()
        .timeout(Duration::from_secs(7))
        .no_proxy()
        .build()
        .expect("client");

    let initialize = |id| {
        json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"parallel-fixture","version":"1"}
        }})
    };
    let (first_result, second_result) = tokio::join!(
        rpc(&client, port, &token, None, initialize(1)),
        rpc(&client, port, &token, None, initialize(2))
    );
    let (first, sid_a) = first_result.expect("first init");
    let (second, sid_b) = second_result.expect("second init");
    let sid_a = sid_a.expect("first session id");
    let sid_b = sid_b.expect("second session id");
    assert!(
        first["result"]["serverInfo"].is_object(),
        "first initialization"
    );
    assert!(
        second["result"]["serverInfo"].is_object(),
        "second initialization"
    );
    assert_ne!(sid_a, sid_b, "independent protocol transports must differ");

    let read = |id| {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
        "params":{"name":"read","arguments":{"path":"shared.txt"}}})
    };
    let (a, _) = rpc(&client, port, &token, Some(&sid_a), read(3))
        .await
        .expect("a read");
    let (b, _) = rpc(&client, port, &token, Some(&sid_b), read(4))
        .await
        .expect("b read");
    assert_eq!(a["result"]["structuredContent"]["text"], "original");
    assert_eq!(b["result"]["structuredContent"]["text"], "original");
    let original_hash = a["result"]["structuredContent"]["sha256"]
        .as_str()
        .expect("sha")
        .to_owned();
    let write = |id, content: &str| {
        json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
        "params":{"name":"write","arguments":{
            "path":"shared.txt","content":content,"expected_sha256":original_hash
        }}})
    };
    let (first_write, _) = rpc(&client, port, &token, Some(&sid_a), write(5, "winner"))
        .await
        .expect("a write");
    assert_eq!(first_write["result"]["isError"], false);
    let (stale, _) = rpc(&client, port, &token, Some(&sid_b), write(6, "loser"))
        .await
        .expect("b stale");
    assert_eq!(stale["result"]["isError"], true);
    let (final_read, _) = rpc(&client, port, &token, Some(&sid_b), read(7))
        .await
        .expect("b observes");
    assert_eq!(final_read["result"]["structuredContent"]["text"], "winner");
    let audit_req = json!({"jsonrpc":"2.0","id":8,"method":"tools/call",
        "params":{"name":"runtime_status","arguments":{}}});
    let (audit, _) = rpc(&client, port, &token, Some(&sid_a), audit_req)
        .await
        .expect("shared audit");
    let activity = &audit["result"]["structuredContent"];
    assert!(activity["total_calls"].as_u64().unwrap_or_default() >= 5);
    let serialized = activity.to_string();
    assert!(serialized.contains("read"));
    assert!(serialized.contains("write"));
    assert!(!serialized.contains("original"));
    assert!(!serialized.contains("winner"));

    // The independent transport holds an SSE GET stream without consuming
    // another client's session. Explicit session DELETE is then isolated.
    let stream = client
        .get(format!("http://127.0.0.1:{port}/mcp"))
        .bearer_auth(&token)
        .header(header::ACCEPT, "text/event-stream")
        .header("Mcp-Session-Id", &sid_a)
        .send()
        .await
        .expect("SSE GET");
    assert!(
        stream.status().is_success(),
        "SSE GET status: {}",
        stream.status()
    );
    drop(stream);
    let terminated = client
        .delete(format!("http://127.0.0.1:{port}/mcp"))
        .bearer_auth(&token)
        .header("Mcp-Session-Id", &sid_a)
        .send()
        .await
        .expect("session DELETE");
    assert!(
        terminated.status().is_success(),
        "session DELETE status: {}",
        terminated.status()
    );
    let (remaining, _) = rpc(&client, port, &token, Some(&sid_b), read(9))
        .await
        .expect("unaffected transport");
    assert_eq!(remaining["result"]["structuredContent"]["text"], "winner");
    assert_eq!(
        std::fs::read_to_string(root.path().join("shared.txt")).expect("disk"),
        "winner"
    );
}
