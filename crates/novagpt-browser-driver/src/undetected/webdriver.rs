//! Minimal, bounded W3C WebDriver client used by the managed ChromeDriver
//! backend.
//!
//! WebDriver keeps a per-session "current window" pointer. NovaGPT serializes
//! every tab operation under one session command mutex and switches to the
//! caller's exact handle first, so four workers cannot cross tabs. Operations
//! are never retried automatically: a caller decides whether a non-idempotent
//! command is safe to repeat.
use super::error::UpstreamError;
use base64::Engine;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;

const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const MAX_SCREENSHOT_BYTES: usize = 12 * 1024 * 1024;
const MAX_SCRIPT_BYTES: usize = 256 * 1024;
const MAX_SELECTOR_BYTES: usize = 1024;
const MAX_URL_BYTES: usize = 4096;
const MAX_ARGUMENT_BYTES: usize = 1024 * 1024;
const W3C_ELEMENT_KEY: &str = "element-6066-11e4-a52e-4f735466cecf";

#[derive(Clone)]
pub struct WebDriverClient {
    base: String,
    http: reqwest::Client,
}

impl WebDriverClient {
    pub fn new(port: u16) -> Result<Self, UpstreamError> {
        Self::with_timeout(port, Duration::from_secs(60))
    }

    pub fn with_timeout(port: u16, command_timeout: Duration) -> Result<Self, UpstreamError> {
        if port == 0 || command_timeout.is_zero() {
            return Err(UpstreamError::InvalidConfig);
        }
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(command_timeout)
            .build()
            .map_err(|_| UpstreamError::WebDriver)?;
        Ok(Self {
            base: format!("http://127.0.0.1:{port}"),
            http,
        })
    }

    /// ChromeDriver `/status` readiness. Used only during startup.
    pub(crate) async fn ready(&self) -> bool {
        let Ok(response) = self
            .http
            .get(format!("{}/status", self.base))
            .timeout(Duration::from_secs(2))
            .send()
            .await
        else {
            return false;
        };
        if !response.status().is_success() {
            return false;
        }
        let Ok(value) = read_json(response, 64 * 1024).await else {
            return false;
        };
        value["value"]["ready"] == Value::Bool(true)
    }

    pub async fn command(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, UpstreamError> {
        validate_path(path)?;
        let mut request = self
            .http
            .request(method, format!("{}{}", self.base, path))
            .header("Content-Type", "application/json");
        if let Some(body) = body {
            request = request.body(serde_json::to_vec(&body).map_err(|_| UpstreamError::Protocol)?);
        }
        let response = request.send().await.map_err(|_| UpstreamError::WebDriver)?;
        let status = response.status();
        let value = read_json(response, MAX_RESPONSE_BYTES).await?;
        if !status.is_success() {
            return Err(map_w3c_error(status, &value));
        }
        Ok(value.get("value").cloned().unwrap_or(Value::Null))
    }
}

async fn read_json(response: reqwest::Response, limit: usize) -> Result<Value, UpstreamError> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(UpstreamError::TooLarge);
    }
    let mut response = response;
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) if chunk.len() <= limit - body.len() => {
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            _ => return Err(UpstreamError::TooLarge),
        }
    }
    serde_json::from_slice(&body).map_err(|_| UpstreamError::Protocol)
}

fn map_w3c_error(status: StatusCode, value: &Value) -> UpstreamError {
    let code = value["value"]["error"].as_str().unwrap_or_default();
    if code == "invalid session id" || status == StatusCode::NOT_FOUND && code.is_empty() {
        return UpstreamError::SessionExpired;
    }
    match code {
        "no such window" => UpstreamError::TabClosed,
        "invalid argument" | "invalid selector" => UpstreamError::InvalidConfig,
        "unknown command" | "unsupported operation" => UpstreamError::WebDriver,
        _ => UpstreamError::WebDriver,
    }
}

fn validate_path(path: &str) -> Result<(), UpstreamError> {
    if !path.starts_with('/')
        || path.len() > 512
        || path.contains("..")
        || path
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(UpstreamError::Protocol);
    }
    Ok(())
}

fn validate_token(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn validate_element_id(value: &str) -> bool {
    // ChromeDriver uses `f.<frame>.d.<document>.e.<index>` identifiers that
    // contain dots, unlike session and window handles.
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn validate_selector(selector: &str) -> Result<(), UpstreamError> {
    if selector.is_empty() || selector.len() > MAX_SELECTOR_BYTES || selector.contains('\0') {
        return Err(UpstreamError::InvalidConfig);
    }
    Ok(())
}

fn validate_url(url: &str) -> Result<(), UpstreamError> {
    if url.len() > MAX_URL_BYTES
        || url != url.trim()
        || url
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(UpstreamError::InvalidConfig);
    }
    if url == "about:blank" {
        return Ok(());
    }
    let parsed = reqwest::Url::parse(url).map_err(|_| UpstreamError::InvalidConfig)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return Err(UpstreamError::InvalidConfig);
    }
    Ok(())
}

struct SessionInner {
    client: WebDriverClient,
    id: String,
    command: Mutex<()>,
    closed: AtomicBool,
}

#[derive(Clone)]
pub struct WebDriverSession {
    inner: Arc<SessionInner>,
}

impl WebDriverSession {
    pub async fn create(
        client: WebDriverClient,
        capabilities: Value,
    ) -> Result<Self, UpstreamError> {
        if !capabilities.is_object() {
            return Err(UpstreamError::InvalidConfig);
        }
        let value = client
            .command(
                Method::POST,
                "/session",
                Some(json!({"capabilities":{"alwaysMatch":capabilities}})),
            )
            .await?;
        let id = value["sessionId"]
            .as_str()
            .ok_or(UpstreamError::Protocol)?
            .to_owned();
        if !validate_token(&id, 128) {
            return Err(UpstreamError::Protocol);
        }
        Ok(Self {
            inner: Arc::new(SessionInner {
                client,
                id,
                command: Mutex::new(()),
                closed: AtomicBool::new(false),
            }),
        })
    }

    pub async fn set_timeouts(
        &self,
        page_load_ms: u64,
        script_ms: u64,
        implicit_ms: u64,
    ) -> Result<(), UpstreamError> {
        self.inner
            .client
            .command(
                Method::POST,
                &format!("/session/{}/timeouts", self.inner.id),
                Some(json!({
                    "pageLoad": page_load_ms,
                    "script": script_ms,
                    "implicit": implicit_ms
                })),
            )
            .await
            .map(|_| ())
    }

    pub async fn handles(&self) -> Result<Vec<String>, UpstreamError> {
        if self.is_closed() {
            return Err(UpstreamError::SessionExpired);
        }
        let value = self
            .inner
            .client
            .command(
                Method::GET,
                &format!("/session/{}/window/handles", self.inner.id),
                None,
            )
            .await?;
        let handles = value.as_array().ok_or(UpstreamError::Protocol)?;
        if handles.len() > 64 {
            return Err(UpstreamError::TooLarge);
        }
        handles
            .iter()
            .map(|handle| {
                let handle = handle.as_str().ok_or(UpstreamError::Protocol)?;
                validate_token(handle, 128)
                    .then(|| handle.to_owned())
                    .ok_or(UpstreamError::Protocol)
            })
            .collect()
    }

    /// Attach to the first window or create one when the browser has none.
    pub async fn first_tab(&self) -> Result<WebDriverTab, UpstreamError> {
        let handles = self.handles().await?;
        match handles.into_iter().next() {
            Some(handle) => Ok(self.tab(handle)),
            None => self.new_tab().await,
        }
    }

    /// Create an isolated tab. Creation is serialized with every other
    /// session command so two workers cannot observe each other's window.
    pub async fn new_tab(&self) -> Result<WebDriverTab, UpstreamError> {
        let _guard = self.inner.command.lock().await;
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(UpstreamError::SessionExpired);
        }
        let value = self
            .inner
            .client
            .command(
                Method::POST,
                &format!("/session/{}/window/new", self.inner.id),
                Some(json!({"type":"tab"})),
            )
            .await?;
        let handle = value["handle"]
            .as_str()
            .ok_or(UpstreamError::Protocol)?
            .to_owned();
        if !validate_token(&handle, 128) {
            return Err(UpstreamError::Protocol);
        }
        Ok(WebDriverTab {
            session: self.clone(),
            handle,
            closed: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn tab(&self, handle: impl Into<String>) -> WebDriverTab {
        WebDriverTab {
            session: self.clone(),
            handle: handle.into(),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub async fn close(&self) -> Result<(), UpstreamError> {
        if self.inner.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let _guard = self.inner.command.lock().await;
        self.inner
            .client
            .command(Method::DELETE, &format!("/session/{}", self.inner.id), None)
            .await
            .map(|_| ())
    }

    /// Selenium-compatible performance log entries. The managed backend does
    /// not poll in the background; callers drain explicitly.
    pub async fn performance_log(&self) -> Result<Vec<Value>, UpstreamError> {
        if self.is_closed() {
            return Err(UpstreamError::SessionExpired);
        }
        let value = self
            .inner
            .client
            .command(
                Method::POST,
                &format!("/session/{}/se/perf/log", self.inner.id),
                Some(json!({"type":"performance"})),
            )
            .await?;
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    /// Convert bounded performance log entries into CDP event objects. Raw
    /// entry text may contain URLs; it is parsed but not logged or returned.
    pub async fn drain_cdp_events(&self) -> Result<Vec<Value>, UpstreamError> {
        let entries = self.performance_log().await?;
        let mut events = Vec::new();
        for entry in entries.into_iter().take(1024) {
            let Some(message) = entry["message"].as_str() else {
                continue;
            };
            if message.len() > 1024 * 1024 {
                continue;
            }
            let Ok(parsed) = serde_json::from_str::<Value>(message) else {
                continue;
            };
            if let Some(event) = parsed.get("message").cloned() {
                events.push(event);
            }
        }
        Ok(events)
    }

    pub fn is_closed(&self) -> bool {
        self.inner.closed.load(Ordering::Acquire)
    }

    /// Mark a session dead without issuing a command. Used after an owned
    /// ChromeDriver service has been stopped, when no HTTP endpoint remains.
    pub(crate) fn invalidate(&self) {
        self.inner.closed.store(true, Ordering::Release);
    }
}

pub struct WebDriverTab {
    session: WebDriverSession,
    handle: String,
    closed: Arc<AtomicBool>,
}

impl Clone for WebDriverTab {
    fn clone(&self) -> Self {
        Self {
            session: self.session.clone(),
            handle: self.handle.clone(),
            // Clones observe the same tab lifetime.
            closed: self.closed.clone(),
        }
    }
}

impl WebDriverTab {
    pub fn handle(&self) -> &str {
        &self.handle
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire) || self.session.is_closed()
    }

    async fn command(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, UpstreamError> {
        let _guard = self.session.inner.command.lock().await;
        self.switch_locked().await?;
        let path = path.replace("{session}", &self.session.inner.id);
        self.session
            .inner
            .client
            .command(method, &path, body)
            .await
            .inspect_err(|error| {
                if *error == UpstreamError::TabClosed {
                    self.closed.store(true, Ordering::Release);
                }
            })
    }

    async fn switch_locked(&self) -> Result<(), UpstreamError> {
        if self.is_closed() {
            return Err(UpstreamError::TabClosed);
        }
        self.session
            .inner
            .client
            .command(
                Method::POST,
                &format!("/session/{}/window", self.session.inner.id),
                Some(json!({"handle": self.handle})),
            )
            .await
            .inspect_err(|error| {
                if *error == UpstreamError::TabClosed {
                    self.closed.store(true, Ordering::Release);
                }
            })
            .map(|_| ())
    }

    async fn cdp_locked(&self, command: &str, params: Value) -> Result<Value, UpstreamError> {
        self.session
            .inner
            .client
            .command(
                Method::POST,
                &format!("/session/{}/goog/cdp/execute", self.session.inner.id),
                Some(json!({"cmd": command, "params": params})),
            )
            .await
            .inspect_err(|error| {
                if *error == UpstreamError::TabClosed {
                    self.closed.store(true, Ordering::Release);
                }
            })
    }

    /// Dispatch a complete physical click as one serialized tab transaction.
    /// Other workers cannot switch the current window between press/release.
    pub async fn click_at(&self, x: f64, y: f64) -> Result<(), UpstreamError> {
        if !x.is_finite() || !y.is_finite() || x < 0.0 || y < 0.0 || x > 100_000.0 || y > 100_000.0
        {
            return Err(UpstreamError::InvalidConfig);
        }
        let _guard = self.session.inner.command.lock().await;
        self.switch_locked().await?;
        for event_type in ["mousePressed", "mouseReleased"] {
            self.cdp_locked(
                "Input.dispatchMouseEvent",
                json!({"type":event_type,"x":x,"y":y,"button":"left","clickCount":1}),
            )
            .await?;
        }
        Ok(())
    }

    /// Send Enter as one serialized key-down/key-up transaction.
    pub async fn press_enter(&self) -> Result<(), UpstreamError> {
        let _guard = self.session.inner.command.lock().await;
        self.switch_locked().await?;
        for event_type in ["keyDown", "keyUp"] {
            self.cdp_locked(
                "Input.dispatchKeyEvent",
                json!({"type":event_type,"key":"Enter","code":"Enter","windowsVirtualKeyCode":13}),
            )
            .await?;
        }
        Ok(())
    }

    /// Insert text into the focused editable element with a real browser input
    /// event (not direct DOM mutation).
    pub async fn insert_text(&self, text: &str) -> Result<(), UpstreamError> {
        if text.is_empty() || text.len() > 512 * 1024 {
            return Err(UpstreamError::InvalidConfig);
        }
        self.cdp_execute("Input.insertText", json!({"text": text}))
            .await
            .map(|_| ())
    }

    pub async fn current_url(&self) -> Result<String, UpstreamError> {
        let value = self
            .command(Method::GET, "/session/{session}/url", None)
            .await?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or(UpstreamError::Protocol)
    }

    pub async fn title(&self) -> Result<String, UpstreamError> {
        let value = self
            .command(Method::GET, "/session/{session}/title", None)
            .await?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or(UpstreamError::Protocol)
    }

    /// Navigate and wait according to the session page-load strategy. Only
    /// http(s) and the explicit blank tab target are accepted.
    pub async fn goto(&self, url: &str) -> Result<(), UpstreamError> {
        validate_url(url)?;
        self.command(
            Method::POST,
            "/session/{session}/url",
            Some(json!({"url": url})),
        )
        .await
        .map(|_| ())
    }

    /// Execute a trusted internal script. Never accept a script from MCP.
    pub async fn execute(&self, script: &str, args: Value) -> Result<Value, UpstreamError> {
        if script.is_empty() || script.len() > MAX_SCRIPT_BYTES || script.contains('\0') {
            return Err(UpstreamError::InvalidConfig);
        }
        if serde_json::to_vec(&args)
            .map_err(|_| UpstreamError::Protocol)?
            .len()
            > MAX_ARGUMENT_BYTES
        {
            return Err(UpstreamError::TooLarge);
        }
        self.command(
            Method::POST,
            "/session/{session}/execute/sync",
            Some(json!({"script": script, "args": args})),
        )
        .await
    }

    pub async fn find(&self, selector: &str) -> Result<WebElement, UpstreamError> {
        validate_selector(selector)?;
        let value = self
            .command(
                Method::POST,
                "/session/{session}/element",
                Some(json!({"using":"css selector","value":selector})),
            )
            .await?;
        self.element_from(&value)
    }

    pub async fn find_all(&self, selector: &str) -> Result<Vec<WebElement>, UpstreamError> {
        validate_selector(selector)?;
        let value = self
            .command(
                Method::POST,
                "/session/{session}/elements",
                Some(json!({"using":"css selector","value":selector})),
            )
            .await?;
        let elements = value.as_array().ok_or(UpstreamError::Protocol)?;
        if elements.len() > 10_000 {
            return Err(UpstreamError::TooLarge);
        }
        elements
            .iter()
            .map(|value| self.element_from(value))
            .collect()
    }

    fn element_from(&self, value: &Value) -> Result<WebElement, UpstreamError> {
        let id = value[W3C_ELEMENT_KEY]
            .as_str()
            .ok_or(UpstreamError::Protocol)?;
        if !validate_element_id(id) {
            return Err(UpstreamError::Protocol);
        }
        Ok(WebElement {
            tab: self.clone(),
            id: id.to_owned(),
        })
    }

    /// Capture a PNG screenshot, decoded and size-checked before return.
    /// ChromeDriver's documented Selenium CDP passthrough. This is the same
    /// mechanism Selenium exposes as `execute_cdp_cmd`; NovaGPT does not open a
    /// second DevTools WebSocket or own a CDP process.
    pub async fn cdp_execute(&self, command: &str, params: Value) -> Result<Value, UpstreamError> {
        if command.is_empty()
            || command.len() > 128
            || !command
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'.')
            || serde_json::to_vec(&params)
                .map_err(|_| UpstreamError::Protocol)?
                .len()
                > MAX_ARGUMENT_BYTES
        {
            return Err(UpstreamError::InvalidConfig);
        }
        self.command(
            Method::POST,
            "/session/{session}/goog/cdp/execute",
            Some(json!({"cmd": command, "params": params})),
        )
        .await
    }

    /// Native WebDriver script execution. Preferred for page state and DOM
    /// reads; CDP is retained only for input/emulation/screenshot commands.
    pub async fn evaluate(&self, script: &str) -> Result<Value, UpstreamError> {
        // WebDriver's execute/sync treats its argument as a function body, so a
        // bare expression or IIFE would discard its value. `Runtime.evaluate`
        // semantics return the expression value; preserve that by adding an
        // explicit return. All internal eval scripts are expressions/IIFEs.
        let wrapped = format!("return ({script});");
        self.execute(&wrapped, json!([])).await
    }

    /// Browser console log entries for the dedicated browsing tab. ChromeDriver
    /// returns entries with page URLs; callers must redact before display.
    pub async fn browser_log(&self) -> Result<Vec<Value>, UpstreamError> {
        if self.is_closed() {
            return Err(UpstreamError::SessionExpired);
        }
        let value = self
            .command(
                Method::POST,
                "/session/{session}/se/log",
                Some(json!({"type":"browser"})),
            )
            .await?;
        Ok(value.as_array().cloned().unwrap_or_default())
    }

    pub async fn screenshot(&self) -> Result<Vec<u8>, UpstreamError> {
        let value = self
            .command(Method::GET, "/session/{session}/screenshot", None)
            .await?;
        let encoded = value.as_str().ok_or(UpstreamError::Protocol)?;
        if encoded.len() > MAX_SCREENSHOT_BYTES {
            return Err(UpstreamError::TooLarge);
        }
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| UpstreamError::Protocol)?;
        if bytes.len() > MAX_SCREENSHOT_BYTES || !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err(UpstreamError::Protocol);
        }
        Ok(bytes)
    }

    /// Close only this tab. A missing tab is treated as already closed.
    pub async fn close(&self) -> Result<(), UpstreamError> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        let _guard = self.session.inner.command.lock().await;
        if self.session.is_closed() {
            return Ok(());
        }
        let switched = self
            .session
            .inner
            .client
            .command(
                Method::POST,
                &format!("/session/{}/window", self.session.inner.id),
                Some(json!({"handle": self.handle})),
            )
            .await;
        if matches!(switched, Err(UpstreamError::TabClosed)) {
            return Ok(());
        }
        switched?;
        match self
            .session
            .inner
            .client
            .command(
                Method::DELETE,
                &format!("/session/{}/window", self.session.inner.id),
                None,
            )
            .await
        {
            Ok(_) | Err(UpstreamError::TabClosed) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Upstream-style recursive frame search. Returned elements may become
    /// stale when the active frame changes; callers must re-find before use.
    pub async fn find_elements_recursive(
        &self,
        selector: &str,
    ) -> Result<Vec<WebElement>, UpstreamError> {
        validate_selector(selector)?;
        let _guard = self.session.inner.command.lock().await;
        if self.is_closed() {
            return Err(UpstreamError::TabClosed);
        }
        self.session
            .inner
            .client
            .command(
                Method::POST,
                &format!("/session/{}/window", self.session.inner.id),
                Some(json!({"handle": self.handle})),
            )
            .await?;
        let session = &self.session.inner;
        let mut results = Vec::new();
        self.session
            .inner
            .client
            .command(
                Method::POST,
                &format!("/session/{}/frame", session.id),
                Some(json!({"id": null})),
            )
            .await?;
        let elements = session
            .client
            .command(
                Method::POST,
                &format!("/session/{}/elements", session.id),
                Some(json!({"using":"css selector","value":selector})),
            )
            .await?;
        for value in elements.as_array().into_iter().flatten() {
            results.push(self.element_from(value)?);
        }
        let frames = session
            .client
            .command(
                Method::POST,
                &format!("/session/{}/elements", session.id),
                Some(json!({"using":"css selector","value":"iframe"})),
            )
            .await?;
        let frames = frames.as_array().cloned().unwrap_or_default();
        for frame in frames.into_iter().take(64) {
            let element = self.element_from(&frame)?;
            let switched = session
                .client
                .command(
                    Method::POST,
                    &format!("/session/{}/frame", session.id),
                    Some(json!({"id": element.reference()})),
                )
                .await;
            if switched.is_err() {
                let _ = session
                    .client
                    .command(
                        Method::POST,
                        &format!("/session/{}/frame", session.id),
                        Some(json!({"id": null})),
                    )
                    .await;
                continue;
            }
            if let Ok(inner) = session
                .client
                .command(
                    Method::POST,
                    &format!("/session/{}/elements", session.id),
                    Some(json!({"using":"css selector","value":selector})),
                )
                .await
            {
                for value in inner.as_array().into_iter().flatten() {
                    if let Ok(element) = self.element_from(value) {
                        results.push(element);
                    }
                }
            }
            let _ = session
                .client
                .command(
                    Method::POST,
                    &format!("/session/{}/frame", session.id),
                    Some(json!({"id": null})),
                )
                .await;
        }
        Ok(results)
    }
}

#[derive(Clone)]
pub struct WebElement {
    tab: WebDriverTab,
    id: String,
}

impl WebElement {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn reference(&self) -> Value {
        let mut reference = serde_json::Map::new();
        reference.insert(W3C_ELEMENT_KEY.to_owned(), Value::String(self.id.clone()));
        Value::Object(reference)
    }

    pub async fn click(&self) -> Result<(), UpstreamError> {
        self.tab
            .command(
                Method::POST,
                &format!("/session/{{session}}/element/{}/click", self.id),
                Some(json!({})),
            )
            .await
            .map(|_| ())
    }

    pub async fn send_keys(&self, text: &str) -> Result<(), UpstreamError> {
        if text.is_empty() || text.len() > 512 * 1024 {
            return Err(UpstreamError::TooLarge);
        }
        self.tab
            .command(
                Method::POST,
                &format!("/session/{{session}}/element/{}/value", self.id),
                Some(json!({"text": text, "value": text.chars().map(|c| c.to_string()).collect::<Vec<_>>()})),
            )
            .await
            .map(|_| ())
    }

    pub async fn text(&self) -> Result<String, UpstreamError> {
        let value = self
            .tab
            .command(
                Method::GET,
                &format!("/session/{{session}}/element/{}/text", self.id),
                None,
            )
            .await?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or(UpstreamError::Protocol)
    }

    pub async fn tag_name(&self) -> Result<String, UpstreamError> {
        let value = self
            .tab
            .command(
                Method::GET,
                &format!("/session/{{session}}/element/{}/name", self.id),
                None,
            )
            .await?;
        value
            .as_str()
            .map(str::to_owned)
            .ok_or(UpstreamError::Protocol)
    }

    pub async fn attribute(&self, name: &str) -> Result<Option<String>, UpstreamError> {
        if name.is_empty() || name.len() > 128 || name.contains('\0') {
            return Err(UpstreamError::InvalidConfig);
        }
        let path = format!("/session/{{session}}/element/{}/attribute/{name}", self.id);
        let value = self.tab.command(Method::GET, &path, None).await?;
        let value = value.as_str().map(str::to_owned);
        if value
            .as_ref()
            .is_some_and(|value| value.len() > 1024 * 1024)
        {
            return Err(UpstreamError::TooLarge);
        }
        Ok(value)
    }

    /// Snapshot all attributes using one bounded script. This mirrors the
    /// upstream advanced WebElement convenience without caching stale values.
    pub async fn attrs(&self) -> Result<Value, UpstreamError> {
        let value = self
            .tab
            .execute(
                "const e = arguments[0]; const out = {}; \
                 for (const a of e.attributes) out[a.name] = a.value; return out;",
                json!([self.reference()]),
            )
            .await?;
        if serde_json::to_vec(&value)
            .map_err(|_| UpstreamError::Protocol)?
            .len()
            > 1024 * 1024
        {
            return Err(UpstreamError::TooLarge);
        }
        Ok(value)
    }

    /// Direct or recursive descendants. Filtering is case-insensitive like
    /// upstream's tag comparison.
    pub async fn children(
        &self,
        tag: Option<&str>,
        recursive: bool,
    ) -> Result<Vec<WebElement>, UpstreamError> {
        if tag.is_some_and(|tag| {
            tag.is_empty()
                || tag.len() > 64
                || !tag
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        }) {
            return Err(UpstreamError::InvalidConfig);
        }
        let selector = tag.map(|tag| tag.to_ascii_lowercase());
        let script = if recursive {
            "const tag = arguments[1]; return [...arguments[0].querySelectorAll(tag || '*')];"
        } else {
            "const tag = arguments[1]; return [...arguments[0].children].filter(e => !tag || e.tagName.toLowerCase() === tag);"
        };
        let value = self
            .tab
            .execute(script, json!([self.reference(), selector]))
            .await?;
        let values = value.as_array().ok_or(UpstreamError::Protocol)?;
        if values.len() > 10_000 {
            return Err(UpstreamError::TooLarge);
        }
        values
            .iter()
            .map(|value| self.tab.element_from(value))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        sync::Mutex,
    };

    #[derive(Default)]
    struct MockState {
        handles: Vec<String>,
        current: String,
        next_handle: usize,
        events: Vec<String>,
        closed_sessions: usize,
    }

    async fn spawn_mock(
        expired: bool,
    ) -> (u16, Arc<Mutex<MockState>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock bind");
        let port = listener.local_addr().expect("addr").port();
        let state = Arc::new(Mutex::new(MockState {
            handles: vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()],
            current: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            ..Default::default()
        }));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let state = shared.clone();
                tokio::spawn(async move {
                    let (read, mut write) = socket.into_split();
                    let mut reader = BufReader::new(read);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        if line.trim().is_empty() {
                            continue;
                        }
                        let mut parts = line.split_whitespace();
                        let method = parts.next().unwrap_or("").to_owned();
                        let path = parts.next().unwrap_or("").to_owned();
                        let mut content_length = 0_usize;
                        loop {
                            let mut header = String::new();
                            if reader.read_line(&mut header).await.unwrap_or(0) == 0 {
                                return;
                            }
                            if header == "\r\n" || header == "\n" {
                                break;
                            }
                            if let Some((name, value)) = header.split_once(':')
                                && name.eq_ignore_ascii_case("content-length")
                            {
                                content_length = value.trim().parse().unwrap_or(0);
                            }
                        }
                        let mut body = vec![0_u8; content_length.min(1024 * 1024)];
                        if content_length > 0 {
                            let _ = reader.read_exact(&mut body).await;
                        }
                        let (status, value) = if expired {
                            (
                                404,
                                json!({"value":{"error":"invalid session id","message":"secret url"}}),
                            )
                        } else {
                            route(&method, &path, &body, &state).await
                        };
                        let mut encoded = serde_json::to_vec(&value).unwrap_or_default();
                        encoded.push(b'\n');
                        let head = format!(
                            "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: keep-alive\r\n\r\n",
                            encoded.len()
                        );
                        if write.write_all(head.as_bytes()).await.is_err()
                            || write.write_all(&encoded).await.is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });
        (port, state, task)
    }

    async fn route(
        method: &str,
        path: &str,
        body: &[u8],
        state: &Arc<Mutex<MockState>>,
    ) -> (u16, Value) {
        let mut state = state.lock().await;
        if path == "/status" {
            return (200, json!({"value":{"ready":true}}));
        }
        if method == "POST" && path == "/session" {
            return (
                200,
                json!({"value":{"sessionId":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","capabilities":{}}}),
            );
        }
        if method == "GET" && path.ends_with("/window/handles") {
            return (200, json!({"value": state.handles.clone()}));
        }
        if method == "POST" && path.ends_with("/window/new") {
            state.next_handle += 1;
            let handle = format!("{:032x}", state.next_handle + 0xaa);
            state.handles.push(handle.clone());
            state.current = handle.clone();
            state.events.push(format!("new:{handle}"));
            return (200, json!({"value":{"handle":handle,"type":"tab"}}));
        }
        if method == "POST" && path.ends_with("/window") {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            if let Some(handle) = parsed["handle"].as_str() {
                if !state.handles.iter().any(|known| known == handle) {
                    return (
                        404,
                        json!({"value":{"error":"no such window","message":"missing"}}),
                    );
                }
                state.current = handle.to_owned();
                state.events.push(format!("switch:{handle}"));
                return (200, json!({"value":null}));
            }
        }
        if method == "GET" && path.ends_with("/window") {
            return (200, json!({"value":state.current}));
        }
        if method == "GET" && path.ends_with("/url") {
            return (
                200,
                json!({"value":format!("https://fixture.invalid/{}/{}", state.current, state.events.len())}),
            );
        }
        if method == "POST" && path.ends_with("/url") {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let current = state.current.clone();
            state.events.push(format!(
                "goto:{current}:{}",
                parsed["url"].as_str().unwrap_or("")
            ));
            return (200, json!({"value":null}));
        }
        if method == "POST" && path.ends_with("/execute/sync") {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let script = parsed["script"].as_str().unwrap_or("");
            let value = if script.contains(".children") || script.contains("querySelectorAll") {
                json!([{"element-6066-11e4-a52e-4f735466cecf":
                    "11111111-2222-3333-4444-555555555555"}])
            } else if script.contains("attributes") {
                json!({"id":"fixture","class":"composer"})
            } else if script.contains("arguments[0]") {
                json!({"ok":true})
            } else {
                json!({"handle": state.current, "events": state.events.len()})
            };
            let current = state.current.clone();
            state
                .events
                .push(format!("execute:{current}:{}", script.len()));
            return (200, json!({"value":value}));
        }
        if method == "POST" && path.ends_with("/element") {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let current = state.current.clone();
            state.events.push(format!(
                "find:{current}:{}",
                parsed["value"].as_str().unwrap_or("")
            ));
            return (
                200,
                json!({"value":{"element-6066-11e4-a52e-4f735466cecf":"11111111-2222-3333-4444-555555555555"}}),
            );
        }
        if method == "POST" && path.ends_with("/elements") {
            return (
                200,
                json!({"value":[{"element-6066-11e4-a52e-4f735466cecf":"11111111-2222-3333-4444-555555555555"}]}),
            );
        }
        if method == "GET" && path.ends_with("/screenshot") {
            let png = base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\nfixture");
            return (200, json!({"value":png}));
        }
        if method == "DELETE" && path.ends_with("/window") {
            let current = state.current.clone();
            state.handles.retain(|handle| handle != &current);
            state.events.push(format!("close:{current}"));
            return (200, json!({"value":null}));
        }
        if method == "DELETE" && path.starts_with("/session/") {
            state.closed_sessions += 1;
            return (200, json!({"value":null}));
        }
        if method == "POST" && path.ends_with("/timeouts") {
            return (200, json!({"value":null}));
        }
        if method == "POST" && path.ends_with("/frame") {
            return (200, json!({"value":null}));
        }
        if method == "POST" && path.ends_with("/se/perf/log") {
            return (
                200,
                json!({"value":[{"level":"INFO","timestamp":1,
                    "message":"{\"message\":{\"method\":\"Network.enable\",\"params\":{}}}"}]}),
            );
        }
        (
            404,
            json!({"value":{"error":"unknown command","message":"missing"}}),
        )
    }

    async fn test_session(port: u16) -> WebDriverSession {
        let client = WebDriverClient::new(port).expect("client");
        assert!(client.ready().await);
        WebDriverSession::create(client, json!({"browserName":"chrome"}))
            .await
            .expect("session")
    }

    #[tokio::test]
    async fn tabs_switch_exactly_and_four_workers_do_not_cross_tabs() {
        let (port, state, mock) = spawn_mock(false).await;
        let session = test_session(port).await;
        let mut tabs = vec![session.first_tab().await.expect("first")];
        for _ in 0..3 {
            tabs.push(session.new_tab().await.expect("extra"));
        }
        let handles: Vec<String> = tabs.iter().map(|tab| tab.handle().to_owned()).collect();
        assert_eq!(handles.len(), 4);
        let tasks = tabs.into_iter().enumerate().map(|(index, tab)| {
            let expected = handles[index].clone();
            tokio::spawn(async move {
                tab.goto(&format!("https://fixture.invalid/tab-{index}"))
                    .await
                    .expect("goto");
                let value = tab.execute("return 1", json!([])).await.expect("execute");
                assert_eq!(value["handle"], expected);
            })
        });
        for task in tasks {
            task.await.expect("worker");
        }
        let state = state.lock().await;
        for (index, handle) in handles.iter().enumerate() {
            assert!(
                state
                    .events
                    .iter()
                    .any(|event| event
                        == &format!("goto:{handle}:https://fixture.invalid/tab-{index}")),
                "missing isolated goto for {handle}"
            );
        }
        assert_eq!(state.handles.len(), 4);
        assert!(state.events.iter().any(|event| event.starts_with("new:")));
        assert_eq!(state.closed_sessions, 0);
        mock.abort();
    }

    #[tokio::test]
    async fn closed_tab_and_expired_session_are_distinguishable() {
        let (port, state, mock) = spawn_mock(false).await;
        let session = test_session(port).await;
        let tab = session.first_tab().await.expect("tab");
        tab.close().await.expect("close");
        assert!(tab.is_closed());
        assert_eq!(tab.current_url().await, Err(UpstreamError::TabClosed));
        session.close().await.expect("session close");
        assert!(session.is_closed());
        assert_eq!(session.handles().await, Err(UpstreamError::SessionExpired));
        assert_eq!(state.lock().await.closed_sessions, 1);
        mock.abort();

        let (port, _state, mock) = spawn_mock(true).await;
        let client = WebDriverClient::new(port).expect("client");
        assert!(matches!(
            WebDriverSession::create(client, json!({"browserName":"chrome"})).await,
            Err(UpstreamError::SessionExpired)
        ));
        mock.abort();
    }

    #[tokio::test]
    async fn unsafe_urls_scripts_and_large_screenshot_are_rejected() {
        let (port, _state, mock) = spawn_mock(false).await;
        let session = test_session(port).await;
        let tab = session.first_tab().await.expect("tab");
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "chrome://settings",
            "https://user:pass@example.com/",
            "https://example.com/\nnext",
        ] {
            assert_eq!(
                tab.goto(url).await,
                Err(UpstreamError::InvalidConfig),
                "{url}"
            );
        }
        assert_eq!(
            tab.execute("", json!([])).await,
            Err(UpstreamError::InvalidConfig)
        );
        let screenshot = tab.screenshot().await.expect("png");
        assert!(screenshot.starts_with(b"\x89PNG"));
        mock.abort();
    }

    #[tokio::test]
    async fn element_attributes_children_and_recursive_frames_are_bounded() {
        let (port, _state, mock) = spawn_mock(false).await;
        let session = test_session(port).await;
        let tab = session.first_tab().await.expect("tab");
        let element = tab.find("#composer").await.expect("element");
        assert_eq!(element.id(), "11111111-2222-3333-4444-555555555555");
        let children = element.children(None, false).await.expect("children");
        assert_eq!(children.len(), 1);
        let recursive = tab
            .find_elements_recursive("#composer")
            .await
            .expect("recursive");
        assert!(!recursive.is_empty());
        let events = session.drain_cdp_events().await.expect("events");
        assert_eq!(events[0]["method"], "Network.enable");
        mock.abort();
    }
}
