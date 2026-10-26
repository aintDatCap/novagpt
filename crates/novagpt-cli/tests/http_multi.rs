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
    let alternate = tempfile::tempdir().expect("second authorized root");
    std::fs::write(alternate.path().join("alternate.txt"), "separate").expect("second fixture");
    let private = tempfile::tempdir().expect("private state");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("temporary TCP port");
    let port = listener.local_addr().expect("addr").port();
    drop(listener);
    let token = "T".repeat(40);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_novagpt"));
    cmd.arg("--root")
        .arg(root.path())
        .arg("--allow-root")
        .arg(alternate.path())
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

    // "No authentication" client: token is provided only in the query URL.
    let base = format!("http://127.0.0.1:{port}/mcp");
    let key = "codexpro_token";
    let query_url = format!("{base}?{key}={token}");
    let unauth = client
        .post(&base)
        .header(header::ACCEPT, "application/json, text/event-stream")
        .json(&initialize(70))
        .send()
        .await
        .expect("unauthorized request");
    assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);
    let wrong = client
        .post(format!("{base}?{key}={}", "incorrect"))
        .header(header::ACCEPT, "application/json, text/event-stream")
        .json(&initialize(71))
        .send()
        .await
        .expect("invalid token");
    assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);
    let duplicated = client
        .post(format!("{query_url}&{key}={token}"))
        .header(header::ACCEPT, "application/json, text/event-stream")
        .json(&initialize(72))
        .send()
        .await
        .expect("duplicate");
    assert_eq!(duplicated.status(), reqwest::StatusCode::UNAUTHORIZED);
    let incompatible = client
        .post(&query_url)
        .bearer_auth("wrong")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .json(&initialize(73))
        .send()
        .await
        .expect("mismatched header");
    assert_eq!(incompatible.status(), reqwest::StatusCode::UNAUTHORIZED);
    let response = client
        .post(&query_url)
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25")
        .json(&initialize(74))
        .send()
        .await
        .expect("query authenticated init");
    assert!(
        response.status().is_success(),
        "query initialize must succeed"
    );
    let query_sid = response
        .headers()
        .get("mcp-session-id")
        .and_then(|value| value.to_str().ok())
        .expect("query session")
        .to_owned();
    let initialize_body = response.text().await.expect("initialize response");
    assert!(initialize_body.contains("novagpt"));
    let stream_by_query = client
        .get(&query_url)
        .header(header::ACCEPT, "text/event-stream")
        .header("Mcp-Session-Id", &query_sid)
        .send()
        .await
        .expect("query SSE GET");
    assert!(stream_by_query.status().is_success());
    drop(stream_by_query);
    let ended = client
        .delete(&query_url)
        .header("Mcp-Session-Id", query_sid)
        .send()
        .await
        .expect("query DELETE");
    assert!(ended.status().is_success());

    // Redacted persisted diagnostics must distinguish HTTP routing, auth failure
    // reasons, and successful MCP traffic without recording the token or URL.
    let (diagnostics, _) = rpc(
        &client,
        port,
        &token,
        Some(&sid_a),
        json!({"jsonrpc":"2.0","id":76,"method":"tools/call",
            "params":{"name":"runtime_status","arguments":{}}}),
    )
    .await
    .expect("diagnostic snapshot");
    let snapshot = diagnostics["result"]["structuredContent"].to_string();
    for expected in [
        "http_server_listening",
        "http_request_post",
        "http_request_get",
        "http_request_delete",
        "http_auth_missing",
        "http_auth_query_invalid",
        "http_auth_query_duplicate",
        "http_auth_conflicting",
        "http_auth_query_ok",
        "http_auth_bearer_ok",
        "http_status_401",
        "http_status_200",
    ] {
        assert!(
            snapshot.contains(expected),
            "missing safe diagnostic {expected}"
        );
    }
    assert!(!snapshot.contains(&token), "secret leaked to diagnostics");
    assert!(
        !snapshot.contains(&query_url),
        "connector URL leaked to diagnostics"
    );

    let mut local_probe = Command::new(env!("CARGO_BIN_EXE_novagpt"));
    local_probe
        .arg("--root")
        .arg(root.path())
        .arg("connection-test")
        .arg("--port")
        .arg(port.to_string())
        .arg("--json")
        .env("NOVAGPT_HTTP_TOKEN", &token);
    if cfg!(windows) {
        local_probe.env("LOCALAPPDATA", private.path());
    } else {
        local_probe.env("XDG_STATE_HOME", private.path());
    }
    let probe = local_probe.output().expect("local connection test");
    assert!(
        probe.status.success(),
        "local connection-test failed: {}",
        String::from_utf8_lossy(&probe.stderr)
    );
    let probe_json: Value = serde_json::from_slice(&probe.stdout).expect("connection JSON");
    assert_eq!(probe_json["status"], "ok");
    assert_eq!(probe_json["authenticated"], true);
    assert!(!String::from_utf8_lossy(&probe.stdout).contains(&token));

    let open_alternate = json!({"jsonrpc":"2.0","id":20,"method":"tools/call",
    "params":{"name":"open_workspace","arguments":{
        "root": alternate.path().to_string_lossy()
    }}});
    let (opened, _) = rpc(&client, port, &token, Some(&sid_a), open_alternate)
        .await
        .expect("explicit alternate root");
    let other_id = opened["result"]["structuredContent"]["workspace_id"]
        .as_str()
        .expect("alternate workspace id");
    let selected = json!({"jsonrpc":"2.0","id":21,"method":"tools/call",
    "params":{"name":"read","arguments":{
        "workspace_id":other_id,"path":"alternate.txt"
    }}});
    let (alt_read, _) = rpc(&client, port, &token, Some(&sid_b), selected)
        .await
        .expect("other transport can explicitly select root");
    assert_eq!(alt_read["result"]["structuredContent"]["text"], "separate");
    let omitted = json!({"jsonrpc":"2.0","id":22,"method":"tools/call",
        "params":{"name":"read","arguments":{"path":"alternate.txt"}}});
    let (denied, _) = rpc(&client, port, &token, Some(&sid_b), omitted)
        .await
        .expect("default root unchanged");
    assert_eq!(denied["result"]["isError"], true);
    let unknown = json!({"jsonrpc":"2.0","id":23,"method":"tools/call",
    "params":{"name":"read","arguments":{
        "workspace_id":"ws_unrecognized","path":"shared.txt"
    }}});
    let (invalid, _) = rpc(&client, port, &token, Some(&sid_a), unknown)
        .await
        .expect("unknown root denied");
    assert_eq!(invalid["result"]["isError"], true);

    // Retained Bash output can be paged by another authorized transport,
    // but cannot be fetched using a different workspace identity.
    let (shell, _) = rpc(
        &client,
        port,
        &token,
        Some(&sid_a),
        json!({"jsonrpc":"2.0","id":40,"method":"tools/call",
            "params":{"name":"bash","arguments":{"command":"echo NOVAGPT_HTTP_SHELL_FIXTURE"}}}),
    )
    .await
    .expect("bounded shell command");
    assert_eq!(shell["result"]["isError"], false);
    let resource = shell["result"]["structuredContent"]["output_resource_id"]
        .as_str()
        .expect("retained output resource");
    let read_output = |workspace_id: Option<&str>| {
        json!({"jsonrpc":"2.0","id":41,
        "method":"tools/call","params":{"name":"read_output","arguments":{
            "output_resource_id":resource,"stream":"stdout","offset":0,
            "max_chars":40,"workspace_id":workspace_id
        }}})
    };
    let (paged, _) = rpc(&client, port, &token, Some(&sid_b), read_output(None))
        .await
        .expect("other transport reads output");
    assert_eq!(paged["result"]["isError"], false);
    assert!(
        paged["result"]["structuredContent"]["text"]
            .as_str()
            .unwrap_or("")
            .contains("NOVAGPT_HTTP_SHELL_FIXTURE")
    );
    let (blocked_output, _) = rpc(
        &client,
        port,
        &token,
        Some(&sid_b),
        read_output(Some(other_id)),
    )
    .await
    .expect("workspace mismatch");
    assert_eq!(blocked_output["result"]["isError"], true);

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
    assert_eq!(activity["mode"], "metadata-only-persistent");
    assert_eq!(activity["write_errors"], 0);
    let recorded_before_restart = activity["total_calls"].as_u64().expect("counter");

    let mut cli_logs = Command::new(env!("CARGO_BIN_EXE_novagpt"));
    cli_logs
        .arg("--root")
        .arg(root.path())
        .args(["logs", "tail"]);
    if cfg!(windows) {
        cli_logs.env("LOCALAPPDATA", private.path());
    } else {
        cli_logs.env("XDG_STATE_HOME", private.path());
    }
    let persisted_output = cli_logs.output().expect("logs tail command");
    assert!(
        persisted_output.status.success(),
        "{:?}",
        persisted_output.status
    );
    let disk_text = String::from_utf8_lossy(&persisted_output.stdout);
    assert!(disk_text.contains("Event #"), "{disk_text}");
    assert!(disk_text.contains("tool "));
    assert!(
        !disk_text.trim_start().starts_with('{'),
        "logs must be plain text"
    );
    assert!(!disk_text.contains(&token));
    assert!(!disk_text.contains("original"));
    assert!(!disk_text.contains("winner"));
    assert!(recorded_before_restart >= 5);
    // A JSON option must not remain on this logging-only CLI command.
    let rejected = Command::new(env!("CARGO_BIN_EXE_novagpt"))
        .arg("--root")
        .arg(root.path())
        .args(["logs", "tail", "--json"])
        .output()
        .expect("removed JSON mode");
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("unexpected argument"));

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

    child.0.kill().expect("stop first server");
    child.0.wait().expect("reap first server");
    let mut restarted = ProcessGuard(cmd.spawn().expect("restart from same workspace"));
    let mut restarted_ready = false;
    for _ in 0..100 {
        if restarted.0.try_wait().expect("poll restarted").is_some() {
            panic!("restarted server exited");
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            restarted_ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(restarted_ready, "restarted HTTP server failed to bind");
    let (_, fresh_session) = rpc(&client, port, &token, None, initialize(30))
        .await
        .expect("new transport after restart");
    let new_id = fresh_session.expect("new transport ID");
    let (recovered, _) = rpc(
        &client,
        port,
        &token,
        Some(&new_id),
        json!({"jsonrpc":"2.0","id":31,"method":"tools/call",
            "params":{"name":"runtime_status","arguments":{}}}),
    )
    .await
    .expect("persisted history visible after restart");
    assert_eq!(
        recovered["result"]["structuredContent"]["mode"],
        "metadata-only-persistent"
    );
    assert!(
        recovered["result"]["structuredContent"]["total_calls"]
            .as_u64()
            .unwrap_or_default()
            >= recorded_before_restart
    );
    assert!(
        recovered["result"]["structuredContent"]["llm"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| item
                .as_str()
                .is_some_and(|line| line.contains("tool=write"))))
    );
}
