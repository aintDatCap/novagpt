//! Explicit ngrok subprocess for the token-protected loopback MCP server.
//! Never starts without an HTTP CLI opt-in, never leaks bearer headers to ngrok inspector.
use serde::Deserialize;
use std::{error::Error, process::Stdio, time::Duration};
use tokio::process::{Child, Command};

pub struct NgrokTunnel {
    pub child: Child,
    port: u16,
    configured_domain: Option<String>,
}

#[derive(Deserialize)]
struct TunnelList {
    tunnels: Vec<TunnelEntry>,
}

#[derive(Deserialize)]
struct TunnelEntry {
    public_url: Option<String>,
    config: Option<TunnelConfig>,
}

#[derive(Deserialize)]
struct TunnelConfig {
    addr: Option<String>,
}

pub fn valid_domain(value: &str) -> bool {
    if value.len() > 253 || !value.contains('.') || value.starts_with('.') || value.ends_with('.') {
        return false;
    }
    value.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

impl NgrokTunnel {
    pub async fn start(
        port: u16,
        domain: Option<String>,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        if let Some(value) = domain.as_deref()
            && !valid_domain(value)
        {
            return Err("Invalid ngrok domain (expected a hostname without scheme or path)".into());
        }

        let version = Command::new("ngrok")
            .arg("version")
            .output()
            .await
            .map_err(
                |_| "ngrok executable unavailable; install ngrok v3 and configure your own account",
            )?;

        if !version.status.success() {
            return Err("ngrok executable cannot start".into());
        }

        let mut command = Command::new("ngrok");
        command
            .arg("http")
            .arg(format!("127.0.0.1:{port}"))
            .arg("--inspect=false")
            .arg("--log=false");

        if let Some(hostname) = domain.as_deref() {
            command.arg("--url").arg(format!("https://{hostname}"));
        }

        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;

        Ok(Self {
            child,
            port,
            configured_domain: domain,
        })
    }

    pub async fn endpoint(&self) -> Option<String> {
        Self::discover_endpoint(self.port, self.configured_domain.as_deref()).await
    }

    /// Local discovery never prints or persists a token and does not start a tunnel.
    pub async fn discover_endpoint(port: u16, configured_domain: Option<&str>) -> Option<String> {
        Self::discover_endpoint_at(port, configured_domain, "http://127.0.0.1:4040/api/tunnels")
            .await
    }

    async fn discover_endpoint_at(
        port: u16,
        configured_domain: Option<&str>,
        api: &str,
    ) -> Option<String> {
        if port == 0 {
            return None;
        }
        // Agent API is loopback-only. Confirm this process' intended upstream port;
        // never present the address of an unrelated tunnel as ready.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .no_proxy()
            .build()
            .ok()?;
        let response = client.get(api).send().await.ok()?;
        if !response.status().is_success()
            || response.content_length().is_some_and(|len| len > 64 * 1024)
        {
            return None;
        }
        let body = response.bytes().await.ok()?;
        if body.len() > 64 * 1024 {
            return None;
        }
        let list: TunnelList = serde_json::from_slice(&body).ok()?;
        for item in list.tunnels {
            let Some(address) = item.config.and_then(|config| config.addr) else {
                continue;
            };
            let allowed_targets = [
                format!("http://127.0.0.1:{port}"),
                format!("http://localhost:{port}"),
                format!("127.0.0.1:{port}"),
            ];
            if !allowed_targets.contains(&address) {
                continue;
            }
            let Some(url) = item.public_url else {
                continue;
            };
            let Ok(parsed) = reqwest::Url::parse(&url) else {
                continue;
            };
            if parsed.scheme() != "https"
                || parsed.port().is_some()
                || parsed.path() != "/"
                || parsed.username() != ""
                || parsed.password().is_some()
            {
                continue;
            }
            if let Some(domain) = configured_domain
                && parsed.host_str() != Some(domain)
            {
                continue;
            }
            return Some(format!("{}://{}/mcp", parsed.scheme(), parsed.host_str()?));
        }
        None
    }

    pub async fn stop(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// Only caller-initiated rendering of this secret-bearing URL is permitted.
/// Do not send the result to tracing, TUI log entries or the clipboard implicitly.
pub fn connector_url(endpoint: &str, token: &str) -> Result<String, &'static str> {
    let mut url = reqwest::Url::parse(endpoint).map_err(|_| "Invalid ngrok endpoint")?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || url.port().is_some()
        || url.path() != "/mcp"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || token.len() < 24
        || token.len() > 256
    {
        return Err("Invalid endpoint or workspace token");
    }
    url.query_pairs_mut().append_pair("codexpro_token", token);
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn tunnel_discovery_rejects_foreign_upstreams_and_unsafe_urls() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("agent API mock");
        let api = format!("http://{}/api/tunnels", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let mut buffer = [0u8; 2048];
            let len = stream.read(&mut buffer).await.expect("read request");
            assert!(String::from_utf8_lossy(&buffer[..len]).contains("GET /api/tunnels"));
            let body = serde_json::json!({"tunnels":[
                {"public_url":"https://wrong.ngrok-free.app","config":{"addr":"127.0.0.1:9999"}},
                {"public_url":"https://host.ngrok-free.app/other","config":{"addr":"127.0.0.1:8787"}},
                {"public_url":"https://right.ngrok-free.app","config":{"addr":"http://127.0.0.1:8787"}}
            ]}).to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("HTTP response");
        });
        let result = NgrokTunnel::discover_endpoint_at(8787, Some("right.ngrok-free.app"), &api)
            .await
            .expect("matching destination");
        assert_eq!(result, "https://right.ngrok-free.app/mcp");
        server.await.expect("API fixture");
    }

    #[test]
    fn connector_url_is_canonical_percent_encoded_and_explicit() {
        let value = "ABCD+&EFGH".repeat(4);
        let complete = connector_url("https://test.ngrok-free.app/mcp", &value).expect("url");
        let parsed = reqwest::Url::parse(&complete).expect("valid HTTPS URL");
        assert_eq!(parsed.path(), "/mcp");
        assert_eq!(parsed.query_pairs().count(), 1);
        assert_eq!(parsed.query_pairs().next().unwrap().1, value);
        for invalid in [
            "http://test.ngrok-free.app/mcp",
            "https://test.ngrok-free.app/other",
            "https://test.ngrok-free.app/mcp?existing=1",
            "https://other@host.ngrok-free.app/mcp",
        ] {
            assert!(connector_url(invalid, &value).is_err(), "{invalid}");
        }
    }

    #[test]
    fn ngrok_domain_is_plain_hostname_and_rejects_url_injection() {
        assert!(valid_domain("my-space.ngrok-free.app"));
        for invalid in [
            "https://example.com",
            "example.com/x",
            "example.com:8443",
            "-bad.example.com",
            "a..example.com",
            "",
        ] {
            assert!(!valid_domain(invalid), "{invalid}");
        }
    }
}
