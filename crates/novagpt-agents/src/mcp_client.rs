//! Opt-in, read-only MCP tool calls over explicitly supplied local I/O.
//!
//! The embedding application owns transport creation, process lifetime and
//! credentials. Only trusted operator configuration may populate the allowlist.
//! MCP servers are not sandboxes: the operator must verify the listed tools are
//! read-only and use least-privilege server credentials. Server output is untrusted.
//! No discovery, process spawning, shell, provider login or automatic retry is
//! performed by this module.

pub use rmcp::model::{CallToolResult, JsonObject};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, CallToolResponse},
    service::{RoleClient, RunningService, ServiceError},
    transport::async_rw::AsyncRwTransport,
};
use std::{collections::BTreeSet, io, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    time::timeout,
};

const MAX_TOOLS: usize = 32;
const MAX_NAME_BYTES: usize = 128;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const MAX_RESULT_BYTES: usize = 256 * 1024;
const MAX_WIRE_BYTES: usize = 512 * 1024;
const MAX_TIMEOUT: Duration = Duration::from_secs(60);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Fixed, redacted error classes. Never exposes remote error payloads.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum McpClientError {
    #[error("Invalid MCP client configuration")]
    InvalidConfig,
    #[error("MCP tool is not explicitly allowed")]
    ToolDenied,
    #[error("MCP tool arguments exceed the configured limit or cannot be encoded")]
    InvalidArguments,
    #[error("MCP connection initialization failed")]
    Initialization,
    #[error("MCP connection is closed or an earlier call was interrupted")]
    Closed,
    #[error("MCP operation timed out")]
    Timeout,
    #[error("MCP transport or protocol failure")]
    Protocol,
    #[error("MCP tool returned an error")]
    ToolFailure,
    #[error("MCP tool requires additional input or asynchronous task handling")]
    Incomplete,
    #[error("MCP tool result exceeds the configured limit or cannot be encoded")]
    InvalidResult,
}

/// Immutable, per-server configuration. Names are exact and case-sensitive.
/// Only the embedding application's trusted configuration may construct this
/// value; never construct it from tool responses or agent-generated requests.
pub struct McpClientConfig {
    read_only_tools: BTreeSet<String>,
    timeout: Duration,
}

impl McpClientConfig {
    pub fn new(
        read_only_tools: impl IntoIterator<Item = impl Into<String>>,
        timeout: Duration,
    ) -> Result<Self, McpClientError> {
        if timeout.is_zero() || timeout > MAX_TIMEOUT {
            return Err(McpClientError::InvalidConfig);
        }
        let mut tools = BTreeSet::new();
        for name in read_only_tools {
            let name = name.into();
            if tools.len() >= MAX_TOOLS
                || name.is_empty()
                || name.len() > MAX_NAME_BYTES
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
                || !tools.insert(name)
            {
                return Err(McpClientError::InvalidConfig);
            }
        }
        if tools.is_empty() {
            return Err(McpClientError::InvalidConfig);
        }
        Ok(Self {
            read_only_tools: tools,
            timeout,
        })
    }

    fn validate_call(&self, name: &str, arguments: &JsonObject) -> Result<(), McpClientError> {
        if !self.read_only_tools.contains(name) {
            return Err(McpClientError::ToolDenied);
        }
        if !fits_json(arguments, MAX_ARGUMENT_BYTES) {
            return Err(McpClientError::InvalidArguments);
        }
        Ok(())
    }
}

/// A single explicitly connected MCP session, with at most one in-flight
/// call. Interrupted calls poison the session; they are never replayed.
pub struct McpClient {
    service: RunningService<RoleClient, ()>,
    config: McpClientConfig,
    usable: bool,
}

impl McpClient {
    /// Connect to an existing trusted, asynchronous byte stream (e.g. a
    /// configured server's piped stdout/stdin). This method never launches
    /// a process or opens a network connection.
    pub async fn connect<R, W>(
        config: McpClientConfig,
        read: R,
        write: W,
    ) -> Result<Self, McpClientError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let transport = AsyncRwTransport::<RoleClient, _, _>::new(read, write)
            .with_max_line_length(MAX_WIRE_BYTES);
        let service = timeout(config.timeout, ().serve(transport))
            .await
            .map_err(|_| McpClientError::Timeout)?
            .map_err(|_| McpClientError::Initialization)?;
        Ok(Self {
            service,
            config,
            usable: true,
        })
    }

    /// Send exactly one allowlisted tools/call request. Arguments and returned
    /// results have independent JSON byte caps; the SDK also caps incoming
    /// wire frames. A timeout is not proof that a remote operation stopped.
    pub async fn call_tool(
        &mut self,
        name: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, McpClientError> {
        if !self.usable || self.service.is_closed() {
            return Err(McpClientError::Closed);
        }
        self.config.validate_call(name, &arguments)?;
        // Mark before awaiting, so cancelling/dropping the call future
        // cannot silently make the session reusable.
        self.usable = false;
        let request = CallToolRequestParams::new(name.to_owned()).with_arguments(arguments);
        let response = timeout(
            self.config.timeout,
            self.service.peer().call_tool_once(request),
        )
        .await;
        let result = match response {
            Ok(Ok(CallToolResponse::Complete(result))) => result,
            Ok(Ok(_)) => {
                self.service.cancellation_token().cancel();
                return Err(McpClientError::Incomplete);
            }
            Ok(Err(error)) => {
                self.service.cancellation_token().cancel();
                return Err(match error {
                    ServiceError::TransportClosed => McpClientError::Closed,
                    ServiceError::Timeout { .. } => McpClientError::Timeout,
                    _ => McpClientError::Protocol,
                });
            }
            Err(_) => {
                self.service.cancellation_token().cancel();
                return Err(McpClientError::Timeout);
            }
        };
        self.usable = true;
        if result.is_error == Some(true) {
            return Err(McpClientError::ToolFailure);
        }
        if !fits_json(&result, MAX_RESULT_BYTES) {
            return Err(McpClientError::InvalidResult);
        }
        Ok(result)
    }

    /// Request graceful shutdown and wait for a bounded interval. A caller
    /// that owns a child process must also manage that process independently.
    pub async fn close(mut self) -> Result<(), McpClientError> {
        match self.service.close_with_timeout(CLOSE_TIMEOUT).await {
            Ok(Some(_)) => Ok(()),
            Ok(None) => Err(McpClientError::Timeout),
            Err(_) => Err(McpClientError::Protocol),
        }
    }
}

/// Count serialization bytes without allocating a second copy of the value.
fn fits_json(value: &impl serde::Serialize, limit: usize) -> bool {
    struct Budget(usize);
    impl io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| io::Error::other("JSON byte limit exceeded"))?;
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(limit), value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

    const TEST_TIMEOUT: Duration = Duration::from_secs(2);

    fn config(duration: Duration) -> McpClientConfig {
        McpClientConfig::new(["get_quote"], duration).expect("valid fixture config")
    }

    #[test]
    fn exact_allowlist_and_configuration_bounds() {
        assert!(McpClientConfig::new(["*"], TEST_TIMEOUT).is_err());
        assert!(McpClientConfig::new(["get_quote", "get_quote"], TEST_TIMEOUT).is_err());
        assert!(McpClientConfig::new(["get_quote"], Duration::ZERO).is_err());
        assert!(McpClientConfig::new(["get_quote"], MAX_TIMEOUT + TEST_TIMEOUT).is_err());
        assert!(McpClientConfig::new(Vec::<String>::new(), TEST_TIMEOUT).is_err());
        assert!(
            McpClientConfig::new((0..=MAX_TOOLS).map(|i| format!("tool_{i}")), TEST_TIMEOUT)
                .is_err()
        );
        let config = config(TEST_TIMEOUT);
        assert_eq!(
            config.validate_call("GET_QUOTE", &JsonObject::new()),
            Err(McpClientError::ToolDenied)
        );
        assert_eq!(
            config.validate_call("place_order", &JsonObject::new()),
            Err(McpClientError::ToolDenied)
        );
        let huge =
            JsonObject::from_iter([("symbol".into(), json!("x".repeat(MAX_ARGUMENT_BYTES)))]);
        assert_eq!(
            config.validate_call("get_quote", &huge),
            Err(McpClientError::InvalidArguments)
        );
    }

    async fn read_message<R: AsyncRead + Unpin>(reader: &mut BufReader<R>) -> Value {
        let mut line = String::new();
        assert_ne!(reader.read_line(&mut line).await.expect("fixture read"), 0);
        serde_json::from_str(&line).expect("fixture JSON")
    }

    async fn send_message<W: AsyncWrite + Unpin>(writer: &mut W, message: Value) {
        writer
            .write_all(format!("{message}\n").as_bytes())
            .await
            .expect("fixture write");
    }

    async fn handshake(
        stream: DuplexStream,
    ) -> (
        BufReader<tokio::io::ReadHalf<DuplexStream>>,
        tokio::io::WriteHalf<DuplexStream>,
    ) {
        let (read, mut write) = tokio::io::split(stream);
        let mut read = BufReader::new(read);
        let request = read_message(&mut read).await;
        assert_eq!(request["method"], "initialize");
        send_message(
            &mut write,
            json!({
                "jsonrpc": "2.0",
                "id": request["id"],
                "result": {
                    "protocolVersion": request["params"]["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "fixture", "version": "1"}
                }
            }),
        )
        .await;
        let initialized = read_message(&mut read).await;
        assert_eq!(initialized["method"], "notifications/initialized");
        (read, write)
    }

    #[tokio::test]
    async fn sends_only_allowlisted_calls_and_bounds_results() {
        let (client, server) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(client);
        let fixture = async {
            let (mut read, mut write) = handshake(server).await;
            for result in [
                json!({"content": [], "structuredContent": {"price": 42}}),
                json!({"content": [], "isError": true}),
                json!({"content": [{"type": "text", "text": "x".repeat(MAX_RESULT_BYTES)}]}),
            ] {
                let request = read_message(&mut read).await;
                assert_eq!(request["method"], "tools/call");
                assert_eq!(request["params"]["name"], "get_quote");
                send_message(
                    &mut write,
                    json!({"jsonrpc": "2.0", "id": request["id"], "result": result}),
                )
                .await;
            }
            // Hold the stream open until the client processes the final result.
            (read, write)
        };
        let calls = async {
            let mut client = McpClient::connect(config(TEST_TIMEOUT), read, write)
                .await
                .expect("connect to fixture");
            assert!(matches!(
                client.call_tool("place_order", JsonObject::new()).await,
                Err(McpClientError::ToolDenied)
            ));
            let large =
                JsonObject::from_iter([("x".into(), json!("x".repeat(MAX_ARGUMENT_BYTES)))]);
            assert!(matches!(
                client.call_tool("get_quote", large).await,
                Err(McpClientError::InvalidArguments)
            ));
            let result = client
                .call_tool("get_quote", JsonObject::new())
                .await
                .expect("quote");
            assert_eq!(result.structured_content, Some(json!({"price": 42})));
            assert!(matches!(
                client.call_tool("get_quote", JsonObject::new()).await,
                Err(McpClientError::ToolFailure)
            ));
            assert!(matches!(
                client.call_tool("get_quote", JsonObject::new()).await,
                Err(McpClientError::InvalidResult)
            ));
            client.close().await.expect("close fixture session");
        };
        timeout(TEST_TIMEOUT * 3, async { tokio::join!(fixture, calls) })
            .await
            .expect("bounded fixture");
    }

    #[tokio::test]
    async fn initialization_timeout_is_bounded() {
        let (client, _server) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(client);
        assert!(matches!(
            McpClient::connect(config(Duration::from_millis(50)), read, write).await,
            Err(McpClientError::Timeout)
        ));
    }

    #[tokio::test]
    async fn timed_out_call_is_not_retried() {
        let (client, server) = tokio::io::duplex(4096);
        let (read, write) = tokio::io::split(client);
        let fixture = async {
            let (mut read, write) = handshake(server).await;
            let request = read_message(&mut read).await;
            assert_eq!(request["method"], "tools/call");
            // Keep the transport alive without replying.
            (read, write)
        };
        let calls = async {
            let mut client = McpClient::connect(config(Duration::from_millis(500)), read, write)
                .await
                .expect("connect to fixture");
            assert!(matches!(
                client.call_tool("get_quote", JsonObject::new()).await,
                Err(McpClientError::Timeout)
            ));
            assert!(matches!(
                client.call_tool("get_quote", JsonObject::new()).await,
                Err(McpClientError::Closed)
            ));
            client.close().await.expect("close timed-out session");
        };
        timeout(TEST_TIMEOUT * 3, async { tokio::join!(fixture, calls) })
            .await
            .expect("bounded fixture");
    }
}
