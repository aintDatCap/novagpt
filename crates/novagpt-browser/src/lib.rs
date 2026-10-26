//! Dedicated visible Chrome CDP transport for ChatGPT subagents.
//! Never reads Chrome cookies/credentials; manual challenges are for the user.
mod bridge;
pub use bridge::{BridgeError, BrowserBridge, BrowserTab, BrowserTabText};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};
use thiserror::Error;
use tokio::{
    fs,
    process::{Child, Command},
    sync::Mutex,
    time::{Duration, Instant, sleep, timeout},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
const HOME: &str = "https://chatgpt.com/";
const BROWSE_TAB: &str = "browse-main";
const MAX_BROWSE_TEXT: usize = 20_000;
const COMPOSER: &str = "#prompt-textarea, [data-testid='prompt-textarea'], [contenteditable='true'][data-lexical-editor='true'], textarea";
const ASSISTANTS: &str = "[data-message-author-role='assistant']";
const MAX_PROMPT: usize = 80_000;
const MAX_REPLY: usize = 180_000;

#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("Dedicated Chrome cannot be launched or contacted; configure a Chrome executable")]
    NotRunning,
    #[error("ChatGPT requires manual {0} in the visible dedicated Chrome window")]
    Manual(&'static str),
    #[error("Browser operation timed out; inspect the visible ChatGPT tab")]
    Timeout,
    #[error("The ChatGPT page changed unexpectedly; no message was sent")]
    Unavailable,
    #[error("Browser response exceeded the size limit")]
    TooLarge,
    #[error("Browser worker was cancelled")]
    Cancelled,
    #[error("ChatGPT Browser protocol error")]
    Protocol,
    #[error("ChatGPT Browser profile is invalid")]
    InvalidProfile,
    #[error("Browser navigation requires a public HTTPS URL with no credentials or custom port")]
    UnsafeUrl,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserDiagnostics {
    pub browser_state: String,
    pub page_count: usize,
    pub queued_page_creations: usize,
    pub manual_check: Option<String>,
    pub page_ready: bool,
    pub automation_driver: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowsedPage {
    pub url: String,
    pub title: String,
    pub text: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserReply {
    pub content: String,
    pub conversation_url: Option<String>,
}

struct Page {
    target_id: String,
    socket: Mutex<Socket>,
    next_command: AtomicU64,
    cancelled: AtomicBool,
    pending_before: Mutex<Option<usize>>,
    manual: Mutex<Option<String>>,
}
impl Page {
    async fn command(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
        let id = self.next_command.fetch_add(1, Ordering::Relaxed);
        let mut socket = self.socket.lock().await;
        let request = json!({"id":id,"method":method,"params":params}).to_string();
        socket
            .send(Message::Text(request.into()))
            .await
            .map_err(|_| BrowserError::Protocol)?;
        let reply = timeout(Duration::from_secs(15), async {
            while let Some(message) = socket.next().await {
                let message = message.map_err(|_| BrowserError::Protocol)?;
                if let Message::Text(text) = message {
                    let response: Value =
                        serde_json::from_str(&text).map_err(|_| BrowserError::Protocol)?;
                    if response["id"].as_u64() == Some(id) {
                        if response.get("error").is_some() {
                            return Err(BrowserError::Protocol);
                        }
                        return Ok(response.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
            }
            Err(BrowserError::Protocol)
        })
        .await
        .map_err(|_| BrowserError::Timeout)??;
        Ok(reply)
    }
    async fn eval(&self, source: &str) -> Result<Value, BrowserError> {
        let result = self
            .command(
                "Runtime.evaluate",
                json!({
                    "expression": source, "returnByValue":true, "awaitPromise":true
                }),
            )
            .await?;
        if result.get("exceptionDetails").is_some() {
            return Err(BrowserError::Protocol);
        }
        Ok(result
            .pointer("/result/value")
            .cloned()
            .unwrap_or(Value::Null))
    }
}
struct Inner {
    executable: PathBuf,
    profile: PathBuf,
    process: Mutex<Option<Child>>,
    port: Mutex<Option<u16>>,
    startup: Mutex<()>,
    create_lock: Mutex<()>,
    pages: Mutex<HashMap<String, Arc<Page>>>,
    http: reqwest::Client,
    start_interval: Duration,
}
#[derive(Clone)]
pub struct ChatGptBrowser {
    inner: Arc<Inner>,
}

impl ChatGptBrowser {
    pub fn new(
        executable: impl AsRef<Path>,
        profile: impl AsRef<Path>,
    ) -> Result<Self, BrowserError> {
        let executable = executable.as_ref().to_path_buf();
        let profile = profile.as_ref().to_path_buf();
        if !executable.is_file()
            || profile.as_os_str().is_empty()
            || profile.exists() && profile.is_symlink()
        {
            return Err(BrowserError::InvalidProfile);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                executable,
                profile,
                process: Mutex::new(None),
                port: Mutex::new(None),
                startup: Mutex::new(()),
                create_lock: Mutex::new(()),
                pages: Mutex::new(HashMap::new()),
                http: reqwest::Client::new(),
                start_interval: Duration::from_millis(400),
            }),
        })
    }

    async fn ensure_started(&self) -> Result<u16, BrowserError> {
        let _start_guard = self.inner.startup.lock().await;
        if let Some(port) = *self.inner.port.lock().await {
            return Ok(port);
        }
        fs::create_dir_all(&self.inner.profile)
            .await
            .map_err(|_| BrowserError::InvalidProfile)?;
        if fs::symlink_metadata(&self.inner.profile)
            .await
            .map_err(|_| BrowserError::InvalidProfile)?
            .file_type()
            .is_symlink()
        {
            return Err(BrowserError::InvalidProfile);
        }
        let owner = self.inner.profile.join("novagpt-browser-owner");
        if let Ok(bytes) = fs::read(&owner).await {
            if bytes != b"NovaGPT dedicated ChatGPT Browser profile v1\n" {
                return Err(BrowserError::InvalidProfile);
            }
        } else {
            let mut entries = fs::read_dir(&self.inner.profile)
                .await
                .map_err(|_| BrowserError::InvalidProfile)?;
            if entries
                .next_entry()
                .await
                .map_err(|_| BrowserError::InvalidProfile)?
                .is_some()
            {
                // Never attach to an existing personal Chrome or CodexPro profile.
                return Err(BrowserError::InvalidProfile);
            }
            fs::write(&owner, b"NovaGPT dedicated ChatGPT Browser profile v1\n")
                .await
                .map_err(|_| BrowserError::InvalidProfile)?;
        }
        let _ = fs::remove_file(self.inner.profile.join("DevToolsActivePort")).await;
        let mut process = Command::new(&self.inner.executable)
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
                "--new-window",
            ])
            .arg(format!("--user-data-dir={}", self.inner.profile.display()))
            .arg(HOME)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| BrowserError::NotRunning)?;
        let port_file = self.inner.profile.join("DevToolsActivePort");
        let deadline = Instant::now() + Duration::from_secs(25);
        let port = loop {
            if let Ok(content) = fs::read_to_string(&port_file).await
                && let Some(first) = content.lines().next()
                && let Ok(port) = first.parse::<u16>()
                && port != 0
            {
                break port;
            }
            if process
                .try_wait()
                .map_err(|_| BrowserError::NotRunning)?
                .is_some()
                || Instant::now() >= deadline
            {
                let _ = process.start_kill();
                return Err(BrowserError::NotRunning);
            }
            sleep(Duration::from_millis(200)).await;
        };
        *self.inner.process.lock().await = Some(process);
        *self.inner.port.lock().await = Some(port);
        Ok(port)
    }

    pub async fn open(&self, id: &str) -> Result<(), BrowserError> {
        if !safe_id(id) {
            return Err(BrowserError::Protocol);
        }
        let _serial = self.inner.create_lock.lock().await;
        if self.inner.pages.lock().await.contains_key(id) {
            return Err(BrowserError::Unavailable);
        }
        let port = self.ensure_started().await?;
        // Chrome's remote debugging server permits target creation via PUT.
        let url = format!("http://127.0.0.1:{port}/json/new?{HOME}");
        let response: Value = self
            .inner
            .http
            .put(url)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|_| BrowserError::NotRunning)?
            .json()
            .await
            .map_err(|_| BrowserError::Protocol)?;
        let ws = response
            .get("webSocketDebuggerUrl")
            .and_then(Value::as_str)
            .ok_or(BrowserError::Protocol)?;
        if !ws.starts_with(&format!("ws://127.0.0.1:{port}/"))
            && !ws.starts_with(&format!("ws://localhost:{port}/"))
        {
            return Err(BrowserError::Protocol);
        }
        let target_id = response
            .get("id")
            .and_then(Value::as_str)
            .ok_or(BrowserError::Protocol)?;
        if target_id.is_empty()
            || target_id.len() > 64
            || !target_id.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(BrowserError::Protocol);
        }
        let (socket, _) = connect_async(ws)
            .await
            .map_err(|_| BrowserError::Protocol)?;
        let page = Arc::new(Page {
            target_id: target_id.to_owned(),
            socket: Mutex::new(socket),
            next_command: AtomicU64::new(1),
            cancelled: AtomicBool::new(false),
            pending_before: Mutex::new(None),
            manual: Mutex::new(None),
        });
        page.command("Runtime.enable", json!({})).await?;
        self.inner.pages.lock().await.insert(id.to_owned(), page);
        sleep(self.inner.start_interval).await;
        Ok(())
    }

    /// Open a visible, dedicated browsing tab, separate from all subagent tabs.
    /// This operation reuses the same owned Chrome process/profile.
    pub async fn browse_open(&self) -> Result<BrowserDiagnostics, BrowserError> {
        if self.page(BROWSE_TAB).await.is_err() {
            match self.open(BROWSE_TAB).await {
                Ok(()) | Err(BrowserError::Unavailable) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(self.diagnostics(Some(BROWSE_TAB)).await)
    }

    /// Navigate the dedicated browsing tab. No arbitrary JavaScript is accepted
    /// through MCP. Subagent pages and their conversations are unaffected.
    pub async fn browse_navigate(&self, url: &str) -> Result<BrowsedPage, BrowserError> {
        let target = public_https_url(url)?;
        let page = self.page(BROWSE_TAB).await?;
        let previous = page
            .eval("location.href")
            .await?
            .as_str()
            .unwrap_or("")
            .to_owned();
        page.command("Page.enable", json!({})).await?;
        let navigation = page
            .command("Page.navigate", json!({"url":target.as_str()}))
            .await?;
        if navigation.get("errorText").is_some() {
            return Err(BrowserError::Unavailable);
        }
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let state = page
                .eval("({url:location.href,ready:document.readyState})")
                .await;
            match state {
                Ok(state) => {
                    if let Some(current) = state["url"].as_str()
                        && (current != previous || current == target.as_str())
                        && state["ready"] != "loading"
                    {
                        public_https_url(current)?;
                        return self.browse_read().await;
                    }
                }
                Err(BrowserError::Protocol) => {} // transient navigation context change
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(BrowserError::Timeout);
            }
            sleep(Duration::from_millis(250)).await;
        }
    }

    /// Read only visible DOM text, title and URL from the dedicated tab.
    /// The current URL is rechecked after navigation/redirects.
    pub async fn browse_read(&self) -> Result<BrowsedPage, BrowserError> {
        let page = self.page(BROWSE_TAB).await?;
        let value = page
            .eval("(() => ({url:location.href,title:document.title,text:(document.body?.innerText || '').slice(0,20000)}))()")
            .await?;
        let url = value["url"].as_str().ok_or(BrowserError::Protocol)?;
        public_https_url(url)?;
        let title = value["title"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(300)
            .collect();
        let text = value["text"]
            .as_str()
            .unwrap_or("")
            .chars()
            .take(MAX_BROWSE_TEXT)
            .collect();
        Ok(BrowsedPage {
            url: url.to_owned(),
            title,
            text,
        })
    }

    pub async fn browse_close(&self) -> Result<(), BrowserError> {
        self.close(BROWSE_TAB).await
    }

    async fn page(&self, id: &str) -> Result<Arc<Page>, BrowserError> {
        self.inner
            .pages
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or(BrowserError::Unavailable)
    }

    pub async fn diagnostics(&self, id: Option<&str>) -> BrowserDiagnostics {
        let pages = self.inner.pages.lock().await;
        let target = id.and_then(|id| pages.get(id));
        let manual = if let Some(page) = target {
            page.manual.lock().await.clone()
        } else {
            None
        };
        BrowserDiagnostics {
            browser_state: if self.inner.port.lock().await.is_some() {
                "attached"
            } else {
                "stopped"
            }
            .into(),
            page_count: pages.len(),
            queued_page_creations: 0,
            manual_check: manual,
            page_ready: target.is_some(),
            automation_driver: "chrome-cdp",
        }
    }

    pub async fn send(&self, id: &str, prompt: &str) -> Result<BrowserReply, BrowserError> {
        if prompt.trim().is_empty() || prompt.len() > MAX_PROMPT {
            return Err(BrowserError::TooLarge);
        }
        let page = self.page(id).await?;
        page.cancelled.store(false, Ordering::Relaxed);
        // A new Chrome target may still be navigating when the first worker
        // request arrives. Give the real page time to render instead of
        // immediately reporting a false authentication challenge.
        let ready_deadline = Instant::now() + Duration::from_secs(20);
        let state = loop {
            if page.cancelled.load(Ordering::Relaxed) {
                return Err(BrowserError::Cancelled);
            }
            let state = self.dom_state(&page).await?;
            if state["composer"] == true
                || state["challenge"] == true
                || state["login"] == true
                || state["external"] == true
                || Instant::now() >= ready_deadline
            {
                break state;
            }
            sleep(Duration::from_millis(250)).await;
        };
        if let Some(manual) = manual_reason(&state) {
            *page.manual.lock().await = Some(manual.to_string());
            return Err(BrowserError::Manual(manual));
        }
        let pending = *page.pending_before.lock().await;
        let before = if let Some(before) = pending {
            before
        } else {
            let before = state["assistant_count"].as_u64().unwrap_or(0) as usize;
            // Focus the composer, then use CDP Input.insertText to produce a
            // trusted browser text input rather than injecting a synthetic event.
            let focussed = page.eval(&format!("(() => {{ const el = document.querySelector({}); if(!el || !el.getClientRects().length) return false; el.focus(); return true }})()", json!(COMPOSER))).await?;
            if focussed != Value::Bool(true) {
                return Err(BrowserError::Manual("authentication"));
            }
            page.command("Input.insertText", json!({"text": prompt}))
                .await?;
            // Check that the text is visible in the composer before sending.
            let has_text = page.eval(&format!("(() => {{ const el = document.querySelector({}); return !!el && (el.innerText || el.value || '').length > 0 }})()", json!(COMPOSER))).await?;
            if has_text != Value::Bool(true) {
                return Err(BrowserError::Unavailable);
            }
            let clicked = page.eval("(() => { const b=document.querySelector('[data-testid=send-button]'); if(b && !b.disabled) { b.click(); return true } return false })()").await?;
            if clicked != Value::Bool(true) {
                page.command("Input.dispatchKeyEvent", json!({"type":"keyDown","key":"Enter","code":"Enter","windowsVirtualKeyCode":13})).await?;
                page.command(
                    "Input.dispatchKeyEvent",
                    json!({"type":"keyUp","key":"Enter","code":"Enter","windowsVirtualKeyCode":13}),
                )
                .await?;
            }
            *page.pending_before.lock().await = Some(before);
            before
        };
        let deadline = Instant::now() + Duration::from_secs(180);
        let mut last = String::new();
        let mut stable = 0;
        loop {
            if page.cancelled.load(Ordering::Relaxed) {
                *page.pending_before.lock().await = None;
                return Err(BrowserError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(BrowserError::Timeout);
            }
            let state = self.dom_state(&page).await?;
            if let Some(manual) = manual_reason(&state) {
                *page.manual.lock().await = Some(manual.to_owned());
                // Keep pending_before, so resume does not send prompt twice.
                return Err(BrowserError::Manual(manual));
            }
            *page.manual.lock().await = None;
            let count = state["assistant_count"].as_u64().unwrap_or(0) as usize;
            if count > before {
                let reply = state["last_text"].as_str().unwrap_or("").trim();
                if reply.len() > MAX_REPLY {
                    return Err(BrowserError::TooLarge);
                }
                if !reply.is_empty() && reply == last && state["complete"] == true {
                    stable += 1;
                } else {
                    stable = 0;
                }
                last = reply.to_owned();
                if stable >= 3 {
                    *page.pending_before.lock().await = None;
                    let conversation_url = state["url"]
                        .as_str()
                        .filter(|url| url.starts_with("https://chatgpt.com/c/"))
                        .map(str::to_owned);
                    return Ok(BrowserReply {
                        content: last,
                        conversation_url,
                    });
                }
            }
            sleep(Duration::from_millis(300)).await;
        }
    }

    async fn dom_state(&self, page: &Page) -> Result<Value, BrowserError> {
        // Never reads cookies/localStorage. Inspect only visible UI controls.
        let expression = format!(
            r#"(() => {{
            if (!location.href.startsWith('https://chatgpt.com/') && !location.href.startsWith('https://chat.openai.com/')) {{
                return {{ external:true }};
            }}
            const editor = document.querySelector({});
            const challenge = !!document.querySelector('iframe[src*="challenges.cloudflare.com"], [class*="cf-turnstile"], #challenge-running, #challenge-form');
            const assistants = [...document.querySelectorAll({})];
            const last = assistants[assistants.length-1];
            const content = last?.querySelector('.markdown, [data-message-content], .prose') || last;
            const stop = !!document.querySelector('[data-testid="stop-button"]');
            const copy = !!last?.querySelector('[data-testid="copy-turn-action-button"]');
            return {{url:location.href, composer:!!editor && !!editor.getClientRects().length, challenge,
                login:!!document.querySelector('[data-testid="login-button"]'),
                assistant_count:assistants.length, last_text:content?.innerText || '',
                complete: copy || !stop}};
        }})()"#,
            json!(COMPOSER),
            json!(ASSISTANTS)
        );
        page.eval(&expression).await
    }

    pub async fn cancel(&self, id: &str) -> Result<(), BrowserError> {
        let page = self.page(id).await?;
        page.cancelled.store(true, Ordering::Relaxed);
        // Interrupt visible generation if possible; do not close the shared Chrome process.
        let _ = page.eval("(() => { const b=document.querySelector('[data-testid=stop-button]'); if(b) b.click(); return true })()").await;
        Ok(())
    }

    pub async fn close(&self, id: &str) -> Result<(), BrowserError> {
        let page = self.inner.pages.lock().await.remove(id);
        if let Some(page) = page
            && let Some(port) = *self.inner.port.lock().await
        {
            // Chrome supports /json/close/<target>; no credentials/cookies
            // are collected, and the shared browser process stays running.
            let url = format!("http://127.0.0.1:{port}/json/close/{}", page.target_id);
            let _ = self
                .inner
                .http
                .get(url)
                .timeout(Duration::from_secs(3))
                .send()
                .await;
        }
        Ok(())
    }
}
/// Restrict automated browsing to ordinary public HTTPS hostnames. Avoid
/// local services, raw IP addresses, embedded credentials and custom ports.
/// DNS names still require network-layer restrictions against rebinding.
fn public_https_url(input: &str) -> Result<reqwest::Url, BrowserError> {
    if input.len() > 2048 || input.bytes().any(|b| b.is_ascii_control()) {
        return Err(BrowserError::UnsafeUrl);
    }
    let url = reqwest::Url::parse(input).map_err(|_| BrowserError::UnsafeUrl)?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(BrowserError::UnsafeUrl);
    }
    let host = url.host_str().ok_or(BrowserError::UnsafeUrl)?;
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Err(BrowserError::UnsafeUrl);
    }
    let host = host.to_ascii_lowercase();
    if !host.contains('.')
        || host == "localhost"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
    {
        return Err(BrowserError::UnsafeUrl);
    }
    Ok(url)
}

fn safe_id(id: &str) -> bool {
    (5..=48).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
fn manual_reason(state: &Value) -> Option<&'static str> {
    if state["external"] == true {
        return Some("verification");
    }
    if state["challenge"] == true {
        return Some("verification");
    }
    if state["composer"] != true || state["login"] == true {
        return Some("authentication");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manual_checks_are_never_bypassed() {
        assert_eq!(
            manual_reason(&json!({"challenge":true,"composer":true})),
            Some("verification")
        );
        assert_eq!(
            manual_reason(&json!({"composer":false})),
            Some("authentication")
        );
        assert_eq!(
            manual_reason(&json!({"composer":true,"challenge":false,"login":false})),
            None
        );
    }
    #[test]
    fn browser_only_accepts_public_https_urls_without_credentials() {
        for allowed in [
            "https://example.org/",
            "https://www.rust-lang.org/learn",
            "https://chatgpt.com/",
        ] {
            assert!(public_https_url(allowed).is_ok(), "{allowed}");
        }
        for denied in [
            "http://example.org",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://127.0.0.1/",
            "https://192.168.1.2/",
            "https://[::1]/",
            "https://localhost/",
            "https://host.local/",
            "https://services.internal/",
            "https://intranet/",
            "https://user:secret@example.org/",
            "https://example.org:444/",
            "https://example.org/\nheader",
        ] {
            assert!(public_https_url(denied).is_err(), "{denied}");
        }
    }
    #[test]
    fn id_validation_is_safe() {
        assert!(safe_id("agent-abcdef01"));
        assert!(!safe_id("../secret"));
    }
}
