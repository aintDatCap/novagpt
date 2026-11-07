//! Background-safe CDP commands for a specific tab in NovaGPT-owned Chrome.
//! Unlike WebDriver `switch to window`, targeting a page's CDP endpoint does
//! not request foreground activation. This transport is internal only: no
//! arbitrary JavaScript, URLs, or debugging ports come from MCP callers.
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, redirect::Policy};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::time::{Duration, timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use super::UpstreamError;

const MAX_TARGETS_BYTES: usize = 128 * 1024;
const MAX_REPLY_BYTES: usize = 1024 * 1024;
const MAX_EXPRESSION_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub struct BackgroundTarget {
    port: u16,
    id: String,
    http: Client,
}

#[derive(Deserialize)]
struct TargetInfo {
    id: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket: Option<String>,
}

impl BackgroundTarget {
    pub fn new(port: u16, handle: &str) -> Result<Self, UpstreamError> {
        // ChromeDriver may return the raw target ID or prefix it with
        // CDwindow-. Match that exact ID in the owned Chrome target list;
        // never fall back to the first or an unrelated tab.
        let id = handle.strip_prefix("CDwindow-").unwrap_or(handle);
        if port == 0 || !valid_target_id(id) {
            return Err(UpstreamError::Protocol);
        }
        let http = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| UpstreamError::Protocol)?;
        Ok(Self {
            port,
            id: id.to_owned(),
            http,
        })
    }

    async fn websocket(&self) -> Result<String, UpstreamError> {
        let response = self
            .http
            .get(format!("http://127.0.0.1:{}/json/list", self.port))
            .send()
            .await
            .map_err(|_| UpstreamError::WebDriver)?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|len| len > MAX_TARGETS_BYTES as u64)
        {
            return Err(UpstreamError::Protocol);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| UpstreamError::Protocol)?;
        if bytes.len() > MAX_TARGETS_BYTES {
            return Err(UpstreamError::TooLarge);
        }
        let targets: Vec<TargetInfo> =
            serde_json::from_slice(&bytes).map_err(|_| UpstreamError::Protocol)?;
        let target = targets
            .into_iter()
            .find(|target| target.id == self.id && target.kind == "page")
            .ok_or(UpstreamError::WebDriver)?;
        let ws = target.websocket.ok_or(UpstreamError::Protocol)?;
        let expected = format!("ws://127.0.0.1:{}/devtools/page/{}", self.port, self.id);
        let localhost = format!("ws://localhost:{}/devtools/page/{}", self.port, self.id);
        if ws != expected && ws != localhost {
            return Err(UpstreamError::Protocol);
        }
        Ok(ws)
    }

    /// Commands are ordered on one CDP socket. The target is immutable and
    /// independently selected; worker A cannot change worker B's tab.
    pub async fn commands(&self, requests: &[(&str, Value)]) -> Result<Vec<Value>, UpstreamError> {
        if requests.is_empty() || requests.len() > 4 {
            return Err(UpstreamError::Protocol);
        }
        for (method, params) in requests {
            if method.is_empty()
                || method.len() > 128
                || !method
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.')
                || serde_json::to_vec(params)
                    .map_err(|_| UpstreamError::Protocol)?
                    .len()
                    > MAX_REPLY_BYTES
            {
                return Err(UpstreamError::Protocol);
            }
        }
        let ws = self.websocket().await?;
        let (mut socket, _) = timeout(Duration::from_secs(5), connect_async(ws))
            .await
            .map_err(|_| UpstreamError::Timeout)?
            .map_err(|_| UpstreamError::WebDriver)?;
        let mut results = Vec::with_capacity(requests.len());
        for (index, (method, params)) in requests.iter().enumerate() {
            let id = index + 1;
            let request = json!({"id":id,"method":method,"params":params});
            socket
                .send(Message::Text(request.to_string().into()))
                .await
                .map_err(|_| UpstreamError::WebDriver)?;
            let response = timeout(Duration::from_secs(15), async {
                while let Some(frame) = socket.next().await {
                    let frame = frame.map_err(|_| UpstreamError::Protocol)?;
                    if let Message::Text(text) = frame {
                        if text.len() > MAX_REPLY_BYTES {
                            return Err(UpstreamError::TooLarge);
                        }
                        let value: Value =
                            serde_json::from_str(&text).map_err(|_| UpstreamError::Protocol)?;
                        if value["id"] == json!(id) {
                            if value.get("error").is_some() {
                                return Err(UpstreamError::Protocol);
                            }
                            return Ok(value["result"].clone());
                        }
                    }
                }
                Err(UpstreamError::WebDriver)
            })
            .await
            .map_err(|_| UpstreamError::Timeout)??;
            results.push(response);
        }
        Ok(results)
    }

    pub async fn command(&self, method: &str, params: Value) -> Result<Value, UpstreamError> {
        self.commands(&[(method, params)])
            .await?
            .into_iter()
            .next()
            .ok_or(UpstreamError::Protocol)
    }

    /// Raise and focus exactly this tab. Only input that a human would see
    /// may request activation; ordinary observation must not.
    pub async fn bring_to_front(&self) -> Result<(), UpstreamError> {
        self.command("Page.bringToFront", json!({}))
            .await
            .map(|_| ())
    }

    pub async fn evaluate(&self, expression: &str) -> Result<Value, UpstreamError> {
        if expression.is_empty()
            || expression.len() > MAX_EXPRESSION_BYTES
            || expression.contains('\0')
        {
            return Err(UpstreamError::Protocol);
        }
        let result = self
            .command(
                "Runtime.evaluate",
                json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
            )
            .await?;
        if result.get("exceptionDetails").is_some() {
            return Err(UpstreamError::Protocol);
        }
        Ok(result
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }
}

fn valid_target_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|byte| byte.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_chromedriver_target_handles_are_accepted() {
        assert!(BackgroundTarget::new(9222, "CDwindow-ABCDEF0123456789").is_ok());
        assert!(BackgroundTarget::new(9222, "ABCDEF0123456789").is_ok());
        for handle in [
            "CDwindow-",
            "CDwindow-../other",
            "other",
            "CDwindow-xyz",
            "CDwindow-123/2",
        ] {
            assert!(BackgroundTarget::new(9222, handle).is_err());
        }
        assert!(BackgroundTarget::new(0, "CDwindow-ABCD").is_err());
    }

    /// Opt-in compositor smoke test. It touches only a disposable Chrome
    /// profile and restores the window that was focused before launching.
    /// Niri exposes focused-window state; X11/macOS/other Wayland compositors
    /// require separate manual confirmation.
    #[tokio::test]
    #[ignore = "requires a live niri Wayland session, Chrome and a cached/available ChromeDriver"]
    async fn real_niri_focus_remains_elsewhere_during_concurrent_background_input() {
        use crate::undetected::{ChromeDriverStore, DriverChoice, ManagedConfig, UndetectedChrome};
        use std::{path::Path, sync::Arc};
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
            process::Command,
        };

        async fn niri(args: &[&str]) -> String {
            let output = Command::new("niri")
                .args(args)
                .output()
                .await
                .expect("niri");
            assert!(output.status.success(), "niri command failed");
            String::from_utf8(output.stdout).expect("JSON")
        }
        let initial: Value = serde_json::from_str(&niri(&["msg", "-j", "focused-window"]).await)
            .expect("initial focus");
        let focus_id = initial["id"].as_u64().expect("focused window id");
        let id_text = focus_id.to_string();

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture server");
        let port = listener.local_addr().expect("addr").port();
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut request = [0_u8; 4096];
                    let _ = socket.read(&mut request).await;
                    let body = "<html><head><title>Background target</title></head><body><input id='entry'><button id='send' onclick=\"document.querySelector('#out').textContent=document.querySelector('#entry').value\">Go</button><p id='out'>unchanged</p></body></html>";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        let root = tempfile::tempdir().expect("profile");
        let store = ChromeDriverStore::new(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/novagpt-chromedriver-store"),
        )
        .expect("store");
        let chrome = crate::undetected::discover_chrome(None).expect("Chrome");
        let driver = UndetectedChrome::launch(ManagedConfig::new(
            chrome,
            root.path().join("profile"),
            DriverChoice::Install(Arc::new(store)),
        ))
        .await
        .expect("launch");
        let url = format!("http://127.0.0.1:{port}/");
        let first = driver.new_tab(&url).await.expect("first");
        let second = driver.new_tab(&url).await.expect("second");
        let a = BackgroundTarget::new(driver.debug_port(), first.handle()).expect("a");
        let b = BackgroundTarget::new(driver.debug_port(), second.handle()).expect("b");
        // ChromeDriver's pageLoadStrategy=none can return before navigation
        // commits. Wait for both fixture pages before sampling desktop focus.
        let mut loaded = false;
        for _ in 0..40 {
            if a.evaluate("document.title").await.ok() == Some(json!("Background target"))
                && b.evaluate("document.title").await.ok() == Some(json!("Background target"))
            {
                loaded = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(loaded, "both disposable fixture tabs must be ready");

        // Chrome may activate once at launch. Explicitly return focus to the
        // previously selected user's application before background work.
        let focused = Command::new("niri")
            .args(["msg", "action", "focus-window", "--id", &id_text])
            .output()
            .await
            .expect("focus other app");
        assert!(focused.status.success(), "focus restoration");
        assert_eq!(
            serde_json::from_str::<Value>(&niri(&["msg", "-j", "focused-window"]).await)
                .expect("focus")["id"],
            json!(focus_id),
        );

        for _ in 0..8 {
            assert_eq!(
                a.evaluate("document.title").await.expect("first tab"),
                "Background target"
            );
            assert_eq!(
                b.evaluate("document.title").await.expect("second tab"),
                "Background target"
            );
            assert_eq!(
                serde_json::from_str::<Value>(&niri(&["msg", "-j", "focused-window"]).await)
                    .expect("focus")["id"],
                json!(focus_id),
                "ordinary background polling must not activate Chrome"
            );
        }
        assert_eq!(
            a.evaluate("(() => { const el=document.querySelector('#entry'); el.focus(); return document.activeElement === el })()")
                .await.expect("editor focus"), json!(true),
        );
        a.command("Input.insertText", json!({"text":"background text"}))
            .await
            .expect("input");
        let point = a.evaluate("(() => {const r=document.querySelector('#send').getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()")
            .await.expect("button bounds");
        let x = point["x"].as_f64().expect("x");
        let y = point["y"].as_f64().expect("y");
        a.commands(&[
            (
                "Input.dispatchMouseEvent",
                json!({"type":"mousePressed","x":x,"y":y,"button":"left","clickCount":1}),
            ),
            (
                "Input.dispatchMouseEvent",
                json!({"type":"mouseReleased","x":x,"y":y,"button":"left","clickCount":1}),
            ),
        ])
        .await
        .expect("background click");
        assert_eq!(
            a.evaluate("document.querySelector('#out').textContent")
                .await
                .expect("output"),
            "background text",
        );
        assert_eq!(
            b.evaluate("document.querySelector('#out').textContent")
                .await
                .expect("isolated"),
            "unchanged",
        );
        assert_eq!(
            serde_json::from_str::<Value>(&niri(&["msg", "-j", "focused-window"]).await)
                .expect("final focus")["id"],
            json!(focus_id),
            "background CDP input must not steal desktop focus",
        );
        driver.shutdown().await;
        server.abort();
    }

    #[tokio::test]
    async fn background_target_can_raise_its_own_tab() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock");
        let port = listener.local_addr().expect("port").port();
        let fixture = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("HTTP");
            let mut request = [0; 4096];
            let count = stream.read(&mut request).await.expect("read");
            assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /json/list"));
            let body = json!([
                {"id":"ABCDEF","type":"page","webSocketDebuggerUrl":format!("ws://127.0.0.1:{port}/devtools/page/ABCDEF")}
            ]).to_string();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await
                .expect("write");
            drop(stream);
            let (stream, _) = listener.accept().await.expect("WS");
            let mut socket = accept_async(stream).await.expect("accept");
            let frame = socket.next().await.expect("frame").expect("valid");
            let data: Value = serde_json::from_str(frame.to_text().expect("text")).expect("JSON");
            assert_eq!(data["method"], "Page.bringToFront");
            socket
                .send(Message::Text(
                    json!({"id":1,"result":{}}).to_string().into(),
                ))
                .await
                .expect("respond");
        });
        let target = BackgroundTarget::new(port, "CDwindow-ABCDEF").expect("target");
        target.bring_to_front().await.expect("activate");
        fixture.await.expect("fixture");
    }

    #[tokio::test]
    async fn background_cdp_reads_exact_target_without_window_activation() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;
        use tokio_tungstenite::accept_async;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock");
        let port = listener.local_addr().expect("port").port();
        let fixture = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("HTTP");
            let mut request = [0; 4096];
            let count = stream.read(&mut request).await.expect("read");
            assert!(String::from_utf8_lossy(&request[..count]).starts_with("GET /json/list"));
            let body = json!([
                {"id":"ABCDEF","type":"page","webSocketDebuggerUrl":format!("ws://127.0.0.1:{port}/devtools/page/ABCDEF")},
                {"id":"OTHER","type":"page","webSocketDebuggerUrl":format!("ws://127.0.0.1:{port}/devtools/page/OTHER")}
            ]).to_string();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                )
                .await
                .expect("write");
            drop(stream);
            let (stream, _) = listener.accept().await.expect("WS");
            let mut socket = accept_async(stream).await.expect("accept");
            let frame = socket.next().await.expect("frame").expect("valid");
            let data: Value = serde_json::from_str(frame.to_text().expect("text")).expect("JSON");
            assert_eq!(data["method"], "Runtime.evaluate");
            assert_eq!(data["params"]["expression"], "location.href");
            socket
                .send(Message::Text(
                    json!({"id":1,"result":{"result":{"value":"https://chatgpt.com/"}}})
                        .to_string()
                        .into(),
                ))
                .await
                .expect("respond");
        });
        let target = BackgroundTarget::new(port, "CDwindow-ABCDEF").expect("target");
        assert_eq!(
            target.evaluate("location.href").await.expect("evaluate"),
            "https://chatgpt.com/"
        );
        fixture.await.expect("fixture");
    }
}
