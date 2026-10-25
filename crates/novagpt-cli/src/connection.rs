//! Bounded local-only authenticated MCP health probe.
use serde_json::{Value, json};
use std::{error::Error, time::Duration};

pub async fn probe(port: u16, token: &str) -> Result<Value, Box<dyn Error>> {
    if port == 0 || token.len() < 24 {
        return Err("Invalid local port or MCP authorization".into());
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?;
    let mut response = client
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .bearer_auth(token)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25")
        .json(&json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{},
                "clientInfo":{"name":"novagpt-local-probe","version":"0.1"}}
        }))
        .send()
        .await
        .map_err(|_| "Local MCP server unreachable")?;
    if !response.status().is_success() {
        return Err("Local MCP authentication or protocol initialization failed".into());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len() + chunk.len() > 16 * 1024 {
            return Err("Local MCP initialize response exceeded the limit".into());
        }
        body.extend_from_slice(&chunk);
    }
    let data = String::from_utf8(body).map_err(|_| "Invalid response text")?;
    let message: Value = serde_json::from_str(&data).or_else(|_| {
        data.lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .find_map(|payload| serde_json::from_str(payload).ok())
            .ok_or_else(|| {
                serde_json::from_str::<Value>("invalid-response")
                    .expect_err("intentionally invalid fixture")
            })
    })?;
    if message["result"]["serverInfo"]["name"] != "novagpt" {
        return Err("Endpoint is not a NovaGPT MCP server".into());
    }
    Ok(json!({
        "status":"ok","server":"novagpt",
        "transport":"streamable-http-loopback",
        "authenticated":true
    }))
}
