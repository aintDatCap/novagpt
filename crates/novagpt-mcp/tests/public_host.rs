//! Regression for ChatGPT/ngrok Host-header authorization through real MCP HTTP.
use novagpt_core::WorkspaceRegistry;
use novagpt_mcp::{NovaExtras, run_http_on_listener_with};
use reqwest::{Client, StatusCode, header};
use serde_json::json;
use std::time::Duration;

#[tokio::test]
async fn explicitly_approved_public_host_can_initialize_but_other_hosts_cannot() {
    let root = tempfile::tempdir().expect("workspace fixture");
    let workspaces = WorkspaceRegistry::new(root.path(), &[]).expect("workspace");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let port = listener.local_addr().expect("address").port();
    let token = "U".repeat(40);
    let extras = NovaExtras {
        http_public_hostname: Some("approved.ngrok-free.dev".into()),
        ..NovaExtras::default()
    };
    let token_for_server = token.clone();
    let server = tokio::spawn(async move {
        run_http_on_listener_with(workspaces, listener, token_for_server, extras)
            .await
            .expect("server");
    });

    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .no_proxy()
        .build()
        .expect("client");
    let request = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
        "protocolVersion":"2025-11-25","capabilities":{},
        "clientInfo":{"name":"host-regression","version":"1"}
    }});
    let url = format!("http://127.0.0.1:{port}/mcp?codexpro_token={token}");
    let initialize = |host: &str| {
        client
            .post(&url)
            .header(header::HOST, host)
            .header(header::ACCEPT, "application/json, text/event-stream")
            .json(&request)
    };
    // Before the fix, the SDK returned 403 for this exact valid host.
    let valid = initialize("approved.ngrok-free.dev")
        .send()
        .await
        .expect("public request");
    assert_eq!(
        valid.status(),
        StatusCode::OK,
        "approved ngrok Host should initialize"
    );
    let body = valid.text().await.expect("body");
    assert!(
        body.contains("novagpt"),
        "actual MCP initialize response expected"
    );

    let local = initialize(&format!("127.0.0.1:{port}"))
        .send()
        .await
        .expect("loopback request");
    assert_eq!(
        local.status(),
        StatusCode::OK,
        "local diagnostics stay functional"
    );

    let forbidden = initialize("other.ngrok-free.dev")
        .send()
        .await
        .expect("unknown external host");
    assert_eq!(
        forbidden.status(),
        StatusCode::FORBIDDEN,
        "DNS rebinding protection must remain active"
    );

    let missing = client
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .header(header::HOST, "approved.ngrok-free.dev")
        .header(header::ACCEPT, "application/json, text/event-stream")
        .json(&request)
        .send()
        .await
        .expect("missing credential");
    assert_eq!(
        missing.status(),
        StatusCode::UNAUTHORIZED,
        "Host allowlist never bypasses credential checks"
    );

    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn invalid_public_hostname_is_rejected_before_server_starts() {
    let root = tempfile::tempdir().expect("workspace fixture");
    let workspaces = WorkspaceRegistry::new(root.path(), &[]).expect("workspace");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let extras = NovaExtras {
        http_public_hostname: Some("*.example.com".into()),
        ..NovaExtras::default()
    };
    let result = run_http_on_listener_with(workspaces, listener, "U".repeat(40), extras).await;
    assert!(result.is_err(), "never authorize wildcard public hosts");
}
