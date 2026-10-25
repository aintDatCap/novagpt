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
    pub async fn start(port: u16, domain: Option<String>) -> Result<Self, Box<dyn Error>> {
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
        // Agent API is loopback-only. Confirm this process' intended upstream port;
        // never present the address of an unrelated tunnel as ready.
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .no_proxy()
            .build()
            .ok()?;
        let list: TunnelList = client
            .get("http://127.0.0.1:4040/api/tunnels")
            .send()
            .await
            .ok()?
            .json()
            .await
            .ok()?;
        for item in list.tunnels {
            let Some(address) = item.config.and_then(|config| config.addr) else {
                continue;
            };
            let allowed_targets = [
                format!("http://127.0.0.1:{}", self.port),
                format!("http://localhost:{}", self.port),
                format!("127.0.0.1:{}", self.port),
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
            if let Some(domain) = self.configured_domain.as_deref()
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

#[cfg(test)]
mod tests {
    use super::*;
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
