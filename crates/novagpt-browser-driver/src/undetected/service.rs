//! Ownership of one ChromeDriver child process.
//!
//! ChromeDriver is a separate executable from Chrome. NovaGPT chooses a
//! loopback port, launches only the binary it installed or was explicitly
//! given, waits for `/status.ready`, and kills only that child on shutdown.
//! It never scans for or kills unrelated driver processes (upstream's
//! `force_kill_instances` is deliberately not ported; see the parity doc).
use super::{error::UpstreamError, webdriver::WebDriverClient};
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    net::TcpListener,
    process::{Child, Command},
    time::{sleep, timeout},
};

pub struct ChromeDriverService {
    child: Child,
    port: u16,
    client: WebDriverClient,
}

impl ChromeDriverService {
    pub async fn start(
        executable: &Path,
        startup_timeout: Duration,
    ) -> Result<Self, UpstreamError> {
        Self::start_with_timeout(executable, startup_timeout, Duration::from_secs(60)).await
    }

    pub async fn start_with_timeout(
        executable: &Path,
        startup_timeout: Duration,
        command_timeout: Duration,
    ) -> Result<Self, UpstreamError> {
        let executor = executable.to_path_buf();
        let metadata = tokio::fs::symlink_metadata(&executor)
            .await
            .map_err(|_| UpstreamError::Service)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            return Err(UpstreamError::Service);
        }
        let port = free_loopback_port().await?;
        Self::start_on_with_timeout(executable, port, startup_timeout, command_timeout).await
    }

    /// Explicit port is used by deterministic tests and by deployments that
    /// reserve a local port. The client still only talks to loopback.
    pub(crate) async fn start_on_with_timeout(
        executable: &Path,
        port: u16,
        startup_timeout: Duration,
        command_timeout: Duration,
    ) -> Result<Self, UpstreamError> {
        if port == 0 || startup_timeout.is_zero() || command_timeout.is_zero() {
            return Err(UpstreamError::InvalidConfig);
        }
        let port_arg = format!("--port={port}");
        let mut command = Command::new(executable);
        command
            .args([
                port_arg.as_str(),
                "--host=127.0.0.1",
                "--allowed-ips=127.0.0.1",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = super::spawn_with_retry(&mut command)
            .await
            .map_err(|_| UpstreamError::Service)?;
        let client = WebDriverClient::with_timeout(port, command_timeout)?;
        let deadline = Instant::now() + startup_timeout;
        loop {
            if client.ready().await {
                // `/status` can be answered by a stale or unrelated listener
                // in a race. Confirm the owned child is still alive before
                // accepting readiness.
                sleep(Duration::from_millis(20)).await;
                if child
                    .try_wait()
                    .map_err(|_| UpstreamError::Service)?
                    .is_some()
                {
                    return Err(UpstreamError::Service);
                }
                return Ok(Self {
                    child,
                    port,
                    client,
                });
            }
            if child
                .try_wait()
                .map_err(|_| UpstreamError::Service)?
                .is_some()
            {
                return Err(UpstreamError::Service);
            }
            if Instant::now() >= deadline {
                let _ = child.start_kill();
                let _ = timeout(Duration::from_secs(3), child.wait()).await;
                return Err(UpstreamError::ServiceTimeout);
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn client(&self) -> WebDriverClient {
        self.client.clone()
    }

    pub async fn is_running(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Kill and reap only the child owned by this service.
    pub async fn stop(&mut self) {
        let _ = self.child.start_kill();
        let _ = timeout(Duration::from_secs(5), self.child.wait()).await;
    }

    pub async fn shutdown(mut self) {
        self.stop().await;
    }
}

async fn free_loopback_port() -> Result<u16, UpstreamError> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|_| UpstreamError::Service)?;
    let port = listener
        .local_addr()
        .map_err(|_| UpstreamError::Service)?
        .port();
    if port == 0 {
        return Err(UpstreamError::Service);
    }
    Ok(port)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn status_server(ready: bool) -> (u16, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("status listener");
        let port = listener.local_addr().expect("addr").port();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let mut buffer = [0_u8; 1024];
                    let _ = socket.read(&mut buffer).await;
                    let body = if ready {
                        r#"{"value":{"ready":true}}"#
                    } else {
                        r#"{"value":{"ready":false}}"#
                    };
                    let _ = socket
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                                body.len()
                            )
                            .as_bytes(),
                        )
                        .await;
                });
            }
        });
        (port, task)
    }

    fn shell_fixture(body: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        use std::io::Write;
        // Some hosts mount the system temporary directory noexec; build the
        // process fixture inside the repository's executable build tree.
        let base = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/novagpt-test-fixtures");
        std::fs::create_dir_all(&base).unwrap();
        let directory = tempfile::Builder::new()
            .prefix("driver-service-")
            .tempdir_in(base)
            .unwrap();
        let path = directory.path().join("chromedriver-fixture");
        let mut script = std::fs::File::create(&path).unwrap();
        writeln!(script, "#!/bin/sh\n{body}").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (directory, path)
    }

    #[tokio::test]
    async fn starts_ready_service_and_stops_its_owned_child() {
        let (port, server) = status_server(true).await;
        let (_directory, fixture) = shell_fixture("sleep 30");
        let mut service = ChromeDriverService::start_on_with_timeout(
            &fixture,
            port,
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .await
        .unwrap_or_else(|error| panic!("service failed: {error:?}"));
        assert_eq!(service.port(), port);
        assert!(service.is_running().await);
        service.stop().await;
        assert!(!service.is_running().await);
        server.abort();
    }

    #[tokio::test]
    async fn reports_unready_timeout_without_leaking_a_child() {
        let (port, server) = status_server(false).await;
        let (_directory, fixture) = shell_fixture("sleep 30");
        let result = ChromeDriverService::start_on_with_timeout(
            &fixture,
            port,
            Duration::from_millis(300),
            Duration::from_secs(5),
        )
        .await;
        match result {
            Err(UpstreamError::ServiceTimeout) => {}
            Err(error) => panic!("expected ServiceTimeout, got {error:?}"),
            Ok(_) => panic!("expected ServiceTimeout, got a running service"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn reports_early_child_exit() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (_directory, fixture) = shell_fixture("exit 3");
        let result = ChromeDriverService::start_on_with_timeout(
            &fixture,
            port,
            Duration::from_secs(2),
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(result, Err(UpstreamError::Service)));
    }
}
