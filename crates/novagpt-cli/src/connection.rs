//! Bounded loopback-only authenticated MCP connectivity and development restart.
use serde_json::{Value, json};
use std::{error::Error, io, time::Duration};

const MAX_RESPONSE_BYTES: usize = 16 * 1024;

#[cfg(all(debug_assertions, unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStatus {
    Scheduled,
    AlreadyPending,
}

fn protocol_error(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn client() -> Result<reqwest::Client, Box<dyn Error>> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
        .build()?)
}

fn validate(port: u16, token: &str) -> Result<(), Box<dyn Error>> {
    if port == 0 || token.len() < 24 {
        return Err("Invalid local port or MCP authorization".into());
    }
    Ok(())
}

async fn decode_response(mut response: reqwest::Response) -> Result<Value, Box<dyn Error>> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| protocol_error("Invalid MCP response"))?
    {
        if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(protocol_error("Local MCP response exceeded the limit").into());
        }
        body.extend_from_slice(&chunk);
    }
    let data =
        std::str::from_utf8(&body).map_err(|_| protocol_error("Invalid MCP response text"))?;
    if let Ok(message) = serde_json::from_str::<Value>(data) {
        return Ok(message);
    }
    data.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .map(str::trim)
        .filter(|payload| payload.starts_with('{'))
        .find_map(|payload| serde_json::from_str::<Value>(payload).ok())
        .ok_or_else(|| protocol_error("Invalid MCP response envelope").into())
}

/// Initialize a real HTTP MCP session without exposing the user token.
async fn initialize(
    client: &reqwest::Client,
    port: u16,
    token: &str,
) -> Result<Option<reqwest::header::HeaderValue>, Box<dyn Error>> {
    let response = client
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
    let session = response.headers().get("Mcp-Session-Id").cloned();
    let message = decode_response(response).await?;
    if message["result"]["serverInfo"]["name"] != "novagpt" {
        return Err("Endpoint is not a NovaGPT MCP server".into());
    }
    Ok(session)
}

pub async fn probe(port: u16, token: &str) -> Result<Value, Box<dyn Error>> {
    validate(port, token)?;
    let client = client()?;
    initialize(&client, port, token).await?;
    Ok(json!({
        "status":"ok","server":"novagpt",
        "transport":"streamable-http-loopback",
        "authenticated":true
    }))
}

/// A foreground-only CLI alternative when a ChatGPT connector's cached tool list
/// has not yet exposed the debug_restart MCP tool. It invokes the same
/// authenticated, supervisor-owned MCP operation; no direct socket access.
#[cfg(all(debug_assertions, unix))]
pub async fn restart(port: u16, token: &str) -> Result<RestartStatus, Box<dyn Error>> {
    validate(port, token)?;
    let client = client()?;
    let session = initialize(&client, port, token)
        .await?
        .ok_or_else(|| protocol_error("MCP session was not established"))?;
    let response = client
        .post(format!("http://127.0.0.1:{port}/mcp"))
        .bearer_auth(token)
        .header("Accept", "application/json, text/event-stream")
        .header("MCP-Protocol-Version", "2025-11-25")
        .header("Mcp-Session-Id", session)
        .json(&json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"debug_restart","arguments":{}}
        }))
        .send()
        .await
        .map_err(|_| "Local MCP restart request could not be sent")?;
    if !response.status().is_success() {
        return Err("Local MCP restart request was rejected".into());
    }
    let message = decode_response(response).await?;
    if message.get("error").is_some() || message["result"]["isError"] == true {
        return Err("Development supervisor unavailable or restart denied".into());
    }
    match message["result"]["structuredContent"]["scheduled"].as_bool() {
        Some(true) => Ok(RestartStatus::Scheduled),
        Some(false) => Ok(RestartStatus::AlreadyPending),
        None => Err(protocol_error("Invalid development restart acknowledgement").into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sse_envelopes_accept_empty_events_and_reject_free_text() {
        let data = "data: \nid: 0\n\nretry: 3000\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n";
        let parsed = data
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim)
            .find_map(|payload| serde_json::from_str::<Value>(payload).ok())
            .expect("MCP server event");
        assert_eq!(parsed["result"]["ok"], true);
    }
}
