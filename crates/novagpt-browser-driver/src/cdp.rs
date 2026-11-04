//! Bounded CDP commands and browser-neutral page actions.
//! JavaScript evaluation is trusted internal code only: never pass expressions
//! supplied by web pages, tool callers or a model without review.
use crate::{BrowserDriver, DriverError, lifecycle::TargetInfo};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpStream,
    sync::Mutex,
    time::{Instant, sleep, timeout},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;
const MAX_COMMAND_BYTES: usize = 512 * 1024;
const MAX_EVENT_BYTES: usize = 8 * 1024;
const MAX_EVENTS: usize = 80;
const MAX_SCREENSHOT_BASE64: usize = 8 * 1024 * 1024;
const MAX_SELECTOR_BYTES: usize = 512;

struct PageInner {
    driver: BrowserDriver,
    target_id: String,
    epoch: u64,
    socket: Mutex<Socket>,
    request_id: AtomicU64,
    closed: AtomicBool,
    events: Mutex<VecDeque<Value>>,
}

/// A single CDP target with independent request IDs, websocket and timeout.
#[derive(Clone)]
pub struct DriverPage {
    inner: Arc<PageInner>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitState {
    Attached,
    Detached,
    Visible,
    Hidden,
}

#[derive(Clone)]
pub struct Locator {
    page: DriverPage,
    selector: String,
}

impl BrowserDriver {
    /// Create one blank page on the shared Chrome process. Navigation is
    /// explicit and happens after CDP is attached.
    pub async fn new_page(&self) -> Result<DriverPage, DriverError> {
        let target = self.new_target().await?;
        match self.attach_target(target.clone()).await {
            Ok(page) => Ok(page),
            Err(err) => {
                let _ = self.close_target(&target.id).await;
                Err(err)
            }
        }
    }

    async fn attach_target(&self, target: TargetInfo) -> Result<DriverPage, DriverError> {
        let (socket, _) = timeout(
            self.config().command_timeout,
            connect_async(&target.websocket),
        )
        .await
        .map_err(|_| DriverError::Timeout)?
        .map_err(|_| DriverError::Protocol)?;
        let page = DriverPage {
            inner: Arc::new(PageInner {
                driver: self.clone(),
                target_id: target.id,
                epoch: target.epoch,
                socket: Mutex::new(socket),
                request_id: AtomicU64::new(1),
                closed: AtomicBool::new(false),
                events: Mutex::new(VecDeque::new()),
            }),
        };
        page.command("Runtime.enable", json!({})).await?;
        Ok(page)
    }

    /// Test-only direct websocket attachment, without launching Chrome.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn connect_fixture(&self, url: &str, id: &str) -> Result<DriverPage, DriverError> {
        let target = TargetInfo {
            id: id.to_string(),
            websocket: url.to_string(),
            epoch: self.epoch(),
        };
        self.attach_target(target).await
    }
}

impl DriverPage {
    pub fn target_id(&self) -> &str {
        &self.inner.target_id
    }

    pub fn is_stale(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire) || self.inner.epoch != self.inner.driver.epoch()
    }

    /// Send one preapproved CDP method; payloads and protocol errors are never
    /// logged. Only a narrow browser-automation command set is accepted.
    pub async fn command(&self, method: &str, params: Value) -> Result<Value, DriverError> {
        if self.is_stale() {
            return Err(DriverError::StalePage);
        }
        if !matches!(
            method,
            "Runtime.enable"
                | "Runtime.evaluate"
                | "Page.enable"
                | "Page.navigate"
                | "Page.captureScreenshot"
                | "Input.insertText"
                | "Input.dispatchMouseEvent"
                | "Input.dispatchKeyEvent"
                | "Log.enable"
                | "Network.enable"
                | "Emulation.setDeviceMetricsOverride"
        ) {
            return Err(DriverError::InvalidControl);
        }
        let id = self.inner.request_id.fetch_add(1, Ordering::Relaxed);
        let packet = json!({"id":id,"method":method,"params":params}).to_string();
        if packet.len() > MAX_COMMAND_BYTES {
            return Err(DriverError::TooLarge);
        }
        let mut socket = self.inner.socket.lock().await;
        socket
            .send(Message::Text(packet.into()))
            .await
            .map_err(|_| DriverError::Protocol)?;
        let response = timeout(self.inner.driver.config().command_timeout, async {
            while let Some(message) = socket.next().await {
                if self.is_stale() {
                    return Err(DriverError::StalePage);
                }
                let message = message.map_err(|_| DriverError::Protocol)?;
                let Message::Text(raw) = message else {
                    continue;
                };
                if raw.len() > MAX_COMMAND_BYTES + MAX_SCREENSHOT_BASE64 {
                    return Err(DriverError::TooLarge);
                }
                let reply: Value = serde_json::from_str(&raw).map_err(|_| DriverError::Protocol)?;
                if reply["id"].as_u64() == Some(id) {
                    if reply.get("error").is_some() {
                        return Err(DriverError::Protocol);
                    }
                    return Ok(reply.get("result").cloned().unwrap_or(Value::Null));
                }
                // No unsolicited event is persisted or logged. Keep only the
                // bounded event kinds consumed by opt-in diagnostics.
                if matches!(
                    reply["method"].as_str(),
                    Some(
                        "Runtime.consoleAPICalled"
                            | "Runtime.exceptionThrown"
                            | "Log.entryAdded"
                            | "Network.loadingFailed"
                            | "Network.responseReceived"
                    )
                ) && raw.len() <= MAX_EVENT_BYTES
                {
                    let mut events = self.inner.events.lock().await;
                    if events.len() == MAX_EVENTS {
                        events.pop_front();
                    }
                    events.push_back(reply);
                }
            }
            Err(DriverError::Protocol)
        })
        .await
        .map_err(|_| DriverError::Timeout)??;
        if self.is_stale() {
            return Err(DriverError::StalePage);
        }
        Ok(response)
    }

    /// Returns bounded in-memory events to the trusted application adapter.
    /// Never forward raw events to an MCP client or persistent log.
    pub async fn drain_events(&self) -> Vec<Value> {
        self.inner.events.lock().await.drain(..).collect()
    }

    /// Evaluate a fixed, application-owned expression. This is intentionally
    /// not exposed through MCP or a model-supplied script API.
    pub async fn evaluate_trusted(&self, expression: &str) -> Result<Value, DriverError> {
        let response = self
            .command(
                "Runtime.evaluate",
                json!({"expression":expression,"returnByValue":true,"awaitPromise":true}),
            )
            .await?;
        if response.get("exceptionDetails").is_some() {
            return Err(DriverError::Protocol);
        }
        Ok(response
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }

    /// Navigate to HTTP(S) with a bounded DOMContentLoaded-style readiness
    /// wait. The embedding application must validate its allowed origins.
    pub async fn goto(&self, input: &str, limit: Duration) -> Result<(), DriverError> {
        let url = reqwest::Url::parse(input).map_err(|_| DriverError::InvalidControl)?;
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(DriverError::InvalidControl);
        }
        self.command("Page.enable", json!({})).await?;
        let value = self
            .command("Page.navigate", json!({"url":url.as_str()}))
            .await?;
        if value.get("errorText").is_some() {
            return Err(DriverError::Protocol);
        }
        let deadline = Instant::now() + limit;
        loop {
            if self.is_stale() {
                return Err(DriverError::StalePage);
            }
            if let Ok(status) = self
                .evaluate_trusted("({url:location.href,ready:document.readyState})")
                .await
                && status["url"].as_str() == Some(url.as_str())
                && matches!(status["ready"].as_str(), Some("interactive" | "complete"))
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(DriverError::Timeout);
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn locator(&self, selector: &str) -> Result<Locator, DriverError> {
        if selector.trim().is_empty()
            || selector.len() > MAX_SELECTOR_BYTES
            || selector.contains('\0')
        {
            return Err(DriverError::InvalidControl);
        }
        Ok(Locator {
            page: self.clone(),
            selector: selector.to_string(),
        })
    }

    pub async fn screenshot(&self) -> Result<String, DriverError> {
        self.command("Page.enable", json!({})).await?;
        let data = self
            .command(
                "Page.captureScreenshot",
                json!({"format":"png","captureBeyondViewport":false,"fromSurface":true}),
            )
            .await?;
        let data = data["data"].as_str().ok_or(DriverError::Protocol)?;
        if data.len() > MAX_SCREENSHOT_BASE64 {
            return Err(DriverError::TooLarge);
        }
        Ok(data.to_owned())
    }

    /// Close only this page, leaving the shared Chrome process and other
    /// pages intact. Safe to call more than once.
    pub async fn close(&self) -> Result<(), DriverError> {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        if self.inner.epoch != self.inner.driver.epoch() {
            return Ok(());
        }
        self.inner.driver.close_target(&self.inner.target_id).await
    }
}

impl Locator {
    pub async fn wait_for(&self, state: WaitState, limit: Duration) -> Result<(), DriverError> {
        let deadline = Instant::now() + limit;
        let expression = format!(
            r#"(() => {{
            let el; try {{ el = document.querySelector({}); }} catch {{ return false; }}
            if (!el) return false;
            const r = el.getBoundingClientRect(), s = getComputedStyle(el);
            return r.width > 0 && r.height > 0 && s.display !== 'none' &&
                s.visibility !== 'hidden';
        }})()"#,
            json!(self.selector)
        );
        loop {
            if self.page.is_stale() {
                return Err(DriverError::StalePage);
            }
            let attached = self
                .page
                .evaluate_trusted(&format!(
                    "(() => !!document.querySelector({}))()",
                    json!(self.selector)
                ))
                .await?;
            let visible = self.page.evaluate_trusted(&expression).await?;
            let attached = attached == true;
            let visible = visible == true;
            let ready = match state {
                WaitState::Attached => attached,
                WaitState::Detached => !attached,
                WaitState::Visible => visible,
                WaitState::Hidden => !visible,
            };
            if ready {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(DriverError::Timeout);
            }
            sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn click(&self) -> Result<(), DriverError> {
        let expression = format!(
            r#"(() => {{
            const el = document.querySelector({});
            if (!el || el.disabled || el.getAttribute('aria-disabled') === 'true') return null;
            el.scrollIntoView({{block:'center',inline:'nearest'}});
            const r = el.getBoundingClientRect(), s = getComputedStyle(el);
            if (r.width <= 0 || r.height <= 0 || s.display === 'none' ||
                s.visibility === 'hidden') return null;
            const x = r.left + r.width/2, y = r.top + r.height/2;
            const hit = document.elementFromPoint(x,y);
            if (!hit || !(el === hit || el.contains(hit))) return null;
            return {{x,y}};
        }})()"#,
            json!(self.selector)
        );
        let point = self.page.evaluate_trusted(&expression).await?;
        let (Some(x), Some(y)) = (point["x"].as_f64(), point["y"].as_f64()) else {
            return Err(DriverError::InvalidControl);
        };
        for kind in ["mousePressed", "mouseReleased"] {
            self.page
                .command(
                    "Input.dispatchMouseEvent",
                    json!({"type":kind,"x":x,"y":y,"button":"left","clickCount":1}),
                )
                .await?;
        }
        Ok(())
    }

    /// Replace text in visible input/textarea/contenteditable controls using
    /// Chrome input events, without writing their values via synthetic DOM.
    /// Password, file and hidden controls are never filled automatically.
    pub async fn fill(&self, text: &str) -> Result<(), DriverError> {
        if text.len() > 80_000 {
            return Err(DriverError::TooLarge);
        }
        let expression = format!(
            r#"(() => {{
            const el = document.querySelector({});
            if (!el || el.disabled || el.readOnly || el.getAttribute('aria-disabled') === 'true') return false;
            const r = el.getBoundingClientRect(), s = getComputedStyle(el);
            if (r.width <= 0 || r.height <= 0 || s.display === 'none' || s.visibility === 'hidden') return false;
            const tag = el.tagName.toLowerCase();
            if (tag === 'input' && !['text','search','email','url','tel','number'].includes(el.type)) return false;
            if (!['input','textarea'].includes(tag) && !el.isContentEditable) return false;
            el.focus();
            if (tag === 'input' || tag === 'textarea') el.select();
            else {{
                const range = document.createRange(), sel = getSelection();
                range.selectNodeContents(el); sel.removeAllRanges(); sel.addRange(range);
            }}
            return document.activeElement === el;
        }})()"#,
            json!(self.selector)
        );
        if self.page.evaluate_trusted(&expression).await? != true {
            return Err(DriverError::InvalidControl);
        }
        if !text.is_empty() {
            self.page
                .command("Input.insertText", json!({"text":text}))
                .await?;
        }
        Ok(())
    }

    /// Limited keyboard actions for a currently focused locator.
    pub async fn press(&self, key: &str) -> Result<(), DriverError> {
        let (code, key_code) = match key {
            "Enter" => ("Enter", 13),
            "Tab" => ("Tab", 9),
            "Escape" => ("Escape", 27),
            "Backspace" => ("Backspace", 8),
            _ => return Err(DriverError::InvalidControl),
        };
        let focus = format!(
            r#"(() => {{
            const el = document.querySelector({});
            if (!el) return false;
            el.focus(); return document.activeElement === el;
        }})()"#,
            json!(self.selector)
        );
        if self.page.evaluate_trusted(&focus).await? != true {
            return Err(DriverError::InvalidControl);
        }
        for event in ["keyDown", "keyUp"] {
            self.page
                .command(
                    "Input.dispatchKeyEvent",
                    json!({"type":event,"key":key,"code":code,"windowsVirtualKeyCode":key_code}),
                )
                .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DriverConfig;

    async fn fixture(index: usize) -> (DriverPage, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("fixture listener");
        let addr = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("fixture accept");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("fixture socket");
            let mut seen = Vec::new();
            while let Some(Ok(Message::Text(raw))) = socket.next().await {
                let request: Value = serde_json::from_str(&raw).expect("fixture JSON");
                let method = request["method"].as_str().expect("method").to_owned();
                seen.push(method.clone());
                let value = match method.as_str() {
                    "Runtime.evaluate" => {
                        let script = request["params"]["expression"].as_str().unwrap_or("");
                        if script.contains("document.readyState") {
                            json!({"url":"https://example.org/","ready":"complete"})
                        } else if script.contains("document.elementFromPoint") {
                            json!({"x":25.0,"y":25.0})
                        } else if script.contains("document.querySelector") {
                            json!(true)
                        } else {
                            json!(format!("agent-{index}"))
                        }
                    }
                    _ => Value::Null,
                };
                let result = if method == "Page.captureScreenshot" {
                    json!({"data":"iVBORw0KGgo="})
                } else if method == "Runtime.evaluate" {
                    json!({"result":{"value":value}})
                } else {
                    json!({})
                };
                socket
                    .send(Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .expect("fixture reply");
                if method == "Page.captureScreenshot" {
                    break;
                }
            }
            seen
        });
        let bin = tempfile::NamedTempFile::new().expect("fixture executable");
        let dir = tempfile::tempdir().expect("fixture profile");
        let driver = BrowserDriver::new(
            DriverConfig::new(bin.path(), dir.path().join("owned")).expect("config"),
        )
        .expect("driver");
        let page = driver
            .connect_fixture(&format!("ws://{addr}/"), "abcd1234")
            .await
            .expect("connect");
        (page, server)
    }

    #[tokio::test]
    async fn mock_cdp_navigate_read_click_fill_screenshot_and_denied_methods() {
        let (page, server) = fixture(1).await;
        assert!(matches!(
            page.command("Network.getAllCookies", json!({})).await,
            Err(DriverError::InvalidControl)
        ));
        assert!(matches!(page.locator(""), Err(DriverError::InvalidControl)));
        assert_eq!(page.evaluate_trusted("1").await.expect("read"), "agent-1");
        page.goto("https://example.org/", Duration::from_secs(1))
            .await
            .expect("navigate");
        let input = page.locator("#task").expect("locator");
        input
            .wait_for(WaitState::Visible, Duration::from_secs(1))
            .await
            .expect("visible");
        input.fill("fixture input").await.expect("fill");
        input.click().await.expect("click");
        input.press("Enter").await.expect("press");
        assert_eq!(page.screenshot().await.expect("screenshot"), "iVBORw0KGgo=");
        let methods = server.await.expect("fixture server");
        assert!(methods.iter().any(|m| m == "Page.navigate"));
        assert!(methods.iter().any(|m| m == "Input.insertText"));
        assert!(methods.iter().any(|m| m == "Input.dispatchMouseEvent"));
        assert!(methods.iter().any(|m| m == "Input.dispatchKeyEvent"));
        assert!(methods.iter().any(|m| m == "Page.captureScreenshot"));
    }

    #[tokio::test]
    async fn four_independent_page_sessions_do_not_mix_results() {
        let mut pages = Vec::new();
        let mut servers = Vec::new();
        for i in 0..4 {
            let (page, server) = fixture(i).await;
            pages.push((i, page));
            servers.push(server);
        }
        let replies =
            futures_util::future::join_all(pages.into_iter().rev().map(|(i, page)| async move {
                let result = page.evaluate_trusted("1").await.expect("page value");
                assert_eq!(result, format!("agent-{i}"));
                assert_eq!(page.screenshot().await.expect("page image"), "iVBORw0KGgo=");
                i
            }))
            .await;
        assert_eq!(replies, vec![3, 2, 1, 0]);
        for server in servers {
            assert!(!server.await.expect("server").is_empty());
        }
    }

    #[test]
    fn typed_states() {
        assert_ne!(WaitState::Visible, WaitState::Hidden);
    }
}
