//! Dedicated visible Chrome CDP transport for ChatGPT subagents.
//! Never reads Chrome cookies/credentials; manual challenges are for the user.
mod bridge;
mod profile;
pub use bridge::{BridgeError, BrowserBridge, BrowserTab, BrowserTabText};
use futures_util::{SinkExt, StreamExt};
use profile::ProfileLease;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
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
// Match CodexPro's textbox-first discovery, but require a real visible,
// editable element before typing. A matching wrapper is not a composer.
const COMPOSER_SELECTORS: &[&str] = &[
    "#prompt-textarea",
    "[data-testid='prompt-textarea']",
    "[contenteditable='true'][data-lexical-editor='true']",
    "[role='textbox'][contenteditable='true']",
    "textarea",
];
const UI_HELPERS: &str = r#"
    const isVisible = (el) => {
        if (!el) return false;
        const rect = el.getBoundingClientRect();
        if (rect.width <= 0 || rect.height <= 0) return false;
        const style = getComputedStyle(el);
        return style.visibility !== 'hidden' && style.visibility !== 'collapse'
            && style.display !== 'none';
    };
    const findEditor = () => {
        for (const selector of selectors) {
            for (const node of document.querySelectorAll(selector)) {
                // isContentEditable alone can be inherited by a non-editable
                // wrapper; require an explicitly editable node.
                const ownEditor = node.tagName === 'TEXTAREA' ||
                    (node.hasAttribute('contenteditable') && node.isContentEditable);
                const el = ownEditor ? node : node.querySelector('[contenteditable], textarea');
                if (!el || (el.tagName !== 'TEXTAREA' &&
                    !(el.hasAttribute('contenteditable') && el.isContentEditable))) continue;
                if (el.disabled || el.readOnly || el.getAttribute('aria-disabled') === 'true') continue;
                if (isVisible(el)) return el;
            }
        }
        return null;
    };
    const editorText = (el) => el?.tagName === 'TEXTAREA'
        ? el.value : (el?.innerText || '');
    const findSendButton = () => {
        const buttons = document.querySelectorAll('button, [role="button"]');
        for (const button of buttons) {
            if (!isVisible(button) || button.disabled ||
                button.getAttribute('aria-disabled') === 'true') continue;
            const label = (button.getAttribute('aria-label') ||
                button.getAttribute('title') || button.textContent || '').trim();
            if (button.getAttribute('data-testid') === 'send-button' ||
                /^(send|submit|invia)(?: (?:message|prompt|messaggio))?$/i.test(label)) {
                return button;
            }
        }
        return null;
    };
"#;

fn ui_expression(body: &str) -> String {
    let selectors = json!(COMPOSER_SELECTORS);
    format!("(() => {{ const selectors = {selectors}; {UI_HELPERS} {body} }})()")
}
const ASSISTANTS: &str = "[data-message-author-role='assistant']";
const MAX_PROMPT: usize = 80_000;
const MAX_REPLY: usize = 180_000;
const MAX_DIAGNOSTIC_EVENTS: usize = 24;
type DiagnosticReporter = Arc<dyn Fn(&str) + Send + Sync>;

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
    #[error("Dedicated NovaGPT Chrome profile is already in use")]
    ProfileBusy,
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
    /// Bounded metadata-only lifecycle events; never includes a prompt, URL or response.
    pub recent_events: Vec<String>,
    pub last_step: Option<String>,
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
    recent_events: Mutex<VecDeque<String>>,
    previous_state: Mutex<Option<String>>,
    reporter: Option<DiagnosticReporter>,
}
impl Page {
    async fn diagnostic(&self, stage: &'static str, flags: &str) {
        // Only fixed event names and locally generated boolean/count flags.
        // Never log the page's text, prompt, URL, cookies or CDP arguments.
        let record = if flags.is_empty() {
            stage.to_owned()
        } else {
            format!("{stage} {flags}")
        };
        {
            let mut events = self.recent_events.lock().await;
            if events.len() == MAX_DIAGNOSTIC_EVENTS {
                events.pop_front();
            }
            events.push_back(record.clone());
        }
        tracing::info!(target: "novagpt", event = %record, "subagent browser");
        if let Some(reporter) = &self.reporter {
            reporter(&record);
        }
        // Console output is best-effort: diagnostics must never change the
        // outcome of a browser action when navigation invalidates the context.
        let js = format!(
            "console.log({})",
            json!(format!("[NovaGPT/browser] {record}"))
        );
        let _ = self.eval(&js).await;
    }

    async fn note_state(&self, state: &Value) {
        let flags = format!(
            "page_kind={} composer={} challenge={} login={} external={} assistants={} complete={}",
            match state["page_kind"].as_str() {
                Some("chatgpt") => "chatgpt",
                Some("initial_blank") => "initial_blank",
                Some("authentication") => "authentication",
                Some("external_other") => "external_other",
                _ => "unknown",
            },
            state["composer"] == true,
            state["challenge"] == true,
            state["login"] == true,
            state["external"] == true,
            state["assistant_count"].as_u64().unwrap_or(0).min(10_000),
            state["complete"] == true,
        );
        let mut previous = self.previous_state.lock().await;
        if previous.as_deref() != Some(flags.as_str()) {
            *previous = Some(flags.clone());
            drop(previous);
            self.diagnostic("page_state", &flags).await;
        }
    }

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
                            // Never print the CDP error payload or original
                            // command params: these may include user text.
                            tracing::warn!(target: "novagpt", method, "browser CDP command rejected");
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
            // The exception may quote a page's private DOM or the prompt.
            // Record only its presence, never its message or stack.
            tracing::warn!(target: "novagpt", "browser DOM evaluation failed");
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
    profile_lease: Mutex<Option<ProfileLease>>,
    startup: Mutex<()>,
    create_lock: Mutex<()>,
    pages: Mutex<HashMap<String, Arc<Page>>>,
    http: reqwest::Client,
    start_interval: Duration,
    reporter: Option<DiagnosticReporter>,
}
#[derive(Clone)]
pub struct ChatGptBrowser {
    inner: Arc<Inner>,
}

/// This launch deliberately does not opt into remote debugging, WebDriver,
/// headless mode or automation. It is *only* for interactive user login.
fn manual_login_command(executable: &Path, profile: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .args([
            "--no-first-run",
            "--no-default-browser-check",
            "--new-window",
        ])
        .arg(format!("--user-data-dir={}", profile.display()))
        .arg(HOME)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
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
                profile_lease: Mutex::new(None),
                startup: Mutex::new(()),
                create_lock: Mutex::new(()),
                pages: Mutex::new(HashMap::new()),
                http: reqwest::Client::builder()
                    .no_proxy()
                    .build()
                    .map_err(|_| BrowserError::Protocol)?,
                start_interval: Duration::from_millis(400),
                reporter: None,
            }),
        })
    }

    /// Forward redacted browser lifecycle steps to an embedding UI (e.g. the
    /// TUI Activity tab). Install before cloning the browser handle.
    pub fn with_diagnostic_reporter(
        mut self,
        reporter: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.reporter = Some(Arc::new(reporter));
        }
        self
    }

    /// Starts ordinary Chrome *without* any remote debugging or automation
    /// switches. The user performs all authentication steps visibly, closes
    /// this window, then starts NovaGPT's CDP-capable server separately.
    pub async fn manual_login(
        executable: impl AsRef<Path>,
        profile: impl AsRef<Path>,
    ) -> Result<(), BrowserError> {
        let browser = Self::new(executable, profile)?;
        let owned_profile = browser.inner.profile.clone();
        let _lease = tokio::task::spawn_blocking(move || profile::acquire(&owned_profile))
            .await
            .map_err(|_| BrowserError::InvalidProfile)??;
        let mut child = manual_login_command(&browser.inner.executable, &browser.inner.profile)
            .kill_on_drop(true)
            .spawn()
            .map_err(|_| BrowserError::NotRunning)?;
        let status = child.wait().await.map_err(|_| BrowserError::NotRunning)?;
        if !status.success() {
            return Err(BrowserError::NotRunning);
        }
        Ok(())
    }

    async fn ensure_started(&self) -> Result<u16, BrowserError> {
        let _start_guard = self.inner.startup.lock().await;
        let current_port = *self.inner.port.lock().await;
        if let Some(port) = current_port {
            let alive = {
                let mut process = self.inner.process.lock().await;
                process
                    .as_mut()
                    .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
            };
            let reachable = if alive {
                self.inner
                    .http
                    .get(format!("http://127.0.0.1:{port}/json/version"))
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                    .is_ok_and(|response| response.status().is_success())
            } else {
                false
            };
            if alive && reachable {
                return Ok(port);
            }
            // Stale port/targets must never be reused after Chrome exits.
            self.inner.port.lock().await.take();
            self.inner.pages.lock().await.clear();
            if let Some(mut child) = self.inner.process.lock().await.take() {
                let _ = child.start_kill();
                let _ = timeout(Duration::from_secs(3), child.wait()).await;
            }
            self.inner.profile_lease.lock().await.take();
        }

        let profile_path = self.inner.profile.clone();
        let lease = tokio::task::spawn_blocking(move || profile::acquire(&profile_path))
            .await
            .map_err(|_| BrowserError::InvalidProfile)??;

        // A prior session can leave this file behind after a crash.
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
                let _ = timeout(Duration::from_secs(3), process.wait()).await;
                return Err(BrowserError::NotRunning);
            }
            sleep(Duration::from_millis(200)).await;
        };
        // Confirm that the endpoint actually belongs to a live CDP server,
        // not merely to a stale DevToolsActivePort file.
        let healthy = self
            .inner
            .http
            .get(format!("http://127.0.0.1:{port}/json/version"))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success());
        if !healthy {
            let _ = process.start_kill();
            let _ = timeout(Duration::from_secs(3), process.wait()).await;
            return Err(BrowserError::NotRunning);
        }
        *self.inner.profile_lease.lock().await = Some(lease);
        *self.inner.process.lock().await = Some(process);
        *self.inner.port.lock().await = Some(port);
        Ok(port)
    }

    pub async fn open(&self, id: &str) -> Result<(), BrowserError> {
        if !safe_id(id) {
            return Err(BrowserError::Protocol);
        }
        let _serial = self.inner.create_lock.lock().await;
        let port = self.ensure_started().await?;
        if self.inner.pages.lock().await.contains_key(id) {
            return Err(BrowserError::Unavailable);
        }
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
            recent_events: Mutex::new(VecDeque::new()),
            previous_state: Mutex::new(None),
            reporter: self.inner.reporter.clone(),
        });
        page.diagnostic("tab_opened", "").await;
        page.command("Runtime.enable", json!({})).await?;
        self.inner.pages.lock().await.insert(id.to_owned(), page);
        sleep(self.inner.start_interval).await;
        Ok(())
    }

    /// Open a visible, dedicated browsing tab, separate from all subagent tabs.
    /// This operation reuses the same owned Chrome process/profile.
    pub async fn browse_open(&self) -> Result<BrowserDiagnostics, BrowserError> {
        // Validate the owned Chrome process before reusing a remembered tab.
        self.ensure_started().await?;
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
        let (manual, recent_events) = if let Some(page) = target {
            (
                page.manual.lock().await.clone(),
                page.recent_events
                    .lock()
                    .await
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        } else {
            (None, Vec::new())
        };
        let last_step = recent_events.last().cloned();
        BrowserDiagnostics {
            browser_state: {
                let running = self
                    .inner
                    .process
                    .lock()
                    .await
                    .as_mut()
                    .is_some_and(|child| matches!(child.try_wait(), Ok(None)));
                if running && self.inner.port.lock().await.is_some() {
                    "attached"
                } else if self.inner.port.lock().await.is_some() {
                    "disconnected"
                } else {
                    "stopped"
                }
                .into()
            },
            page_count: pages.len(),
            queued_page_creations: 0,
            manual_check: manual,
            page_ready: target.is_some(),
            automation_driver: "chrome-cdp",
            recent_events,
            last_step,
        }
    }

    pub async fn send(&self, id: &str, prompt: &str) -> Result<BrowserReply, BrowserError> {
        if prompt.trim().is_empty() || prompt.len() > MAX_PROMPT {
            return Err(BrowserError::TooLarge);
        }
        let page = self.page(id).await?;
        page.cancelled.store(false, Ordering::Relaxed);
        page.diagnostic("send_started", "").await;
        // A new Chrome target may still be navigating when the first worker
        // request arrives. Give the real page time to render instead of
        // immediately reporting a false authentication challenge.
        let ready_deadline = Instant::now() + Duration::from_secs(20);
        let mut navigation_retry_logged = false;
        let state = loop {
            if page.cancelled.load(Ordering::Relaxed) {
                return Err(BrowserError::Cancelled);
            }
            let state = match self.dom_state(&page).await {
                Ok(state) => state,
                // CDP may switch execution contexts while the newly created
                // tab navigates. Keep the same bounded wait as CodexPro.
                Err(BrowserError::Protocol) if Instant::now() < ready_deadline => {
                    if !navigation_retry_logged {
                        page.diagnostic("navigation_context_retry", "").await;
                        navigation_retry_logged = true;
                    }
                    sleep(Duration::from_millis(250)).await;
                    continue;
                }
                Err(error) => {
                    page.diagnostic("page_probe_failed", "").await;
                    return Err(error);
                }
            };
            page.note_state(&state).await;
            if state["transient"] == true {
                if Instant::now() >= ready_deadline {
                    page.diagnostic("initial_navigation_timeout", "").await;
                    return Err(BrowserError::Timeout);
                }
                sleep(Duration::from_millis(250)).await;
                continue;
            }
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
            page.diagnostic("manual_check_required", manual).await;
            return Err(BrowserError::Manual(manual));
        }
        page.diagnostic("composer_ready", "").await;
        let pending = *page.pending_before.lock().await;
        let before = if let Some(before) = pending {
            before
        } else {
            let before = state["assistant_count"].as_u64().unwrap_or(0) as usize;
            // Select the actual visible, editable textbox (not its wrapper).
            // Replacing a selection via CDP generates browser input events,
            // unlike writing directly to the DOM or dispatching fake events.
            let focused = page
                .eval(&ui_expression(
                    r#"const el = findEditor();
                    if (!el) return false;
                    el.focus();
                    if (el.tagName === 'TEXTAREA') {
                        el.select();
                    } else {
                        const selection = window.getSelection();
                        const range = document.createRange();
                        range.selectNodeContents(el);
                        selection.removeAllRanges();
                        selection.addRange(range);
                    }
                    return document.activeElement === el;"#,
                ))
                .await?;
            if focused != Value::Bool(true) {
                page.diagnostic("composer_focus_failed", "").await;
                return Err(BrowserError::Unavailable);
            }
            page.diagnostic("composer_focused", "").await;
            page.command("Input.insertText", json!({"text": prompt}))
                .await?;
            page.diagnostic("text_input_dispatched", "").await;
            // Check an actual prefix of the inserted prompt, not merely that
            // some unrelated element had nonempty text.
            let inserted = page
                .eval(&ui_expression("return editorText(findEditor());"))
                .await?;
            let prefix: String = prompt
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or(prompt)
                .chars()
                .take(24)
                .collect();
            if !inserted.as_str().is_some_and(|text| text.contains(&prefix)) {
                page.diagnostic("text_check_failed", "").await;
                return Err(BrowserError::Unavailable);
            }
            page.diagnostic("text_verified", "").await;
            // Native CDP mouse events, rather than DOM .click(), match the
            // physical input that CodexPro's Playwright locator.click uses.
            // Retry briefly for React to enable the button after typing.
            let mut button = Value::Null;
            for _ in 0..8 {
                button = page
                    .eval(&ui_expression(
                        r#"const el = findSendButton();
                        if (!el) return null;
                        const rect = el.getBoundingClientRect();
                        return {x:rect.left + rect.width/2, y:rect.top + rect.height/2};"#,
                    ))
                    .await?;
                if button.is_object() {
                    break;
                }
                sleep(Duration::from_millis(125)).await;
            }
            if let (Some(x), Some(y)) = (button["x"].as_f64(), button["y"].as_f64()) {
                page.diagnostic("send_button_found", "").await;
                page.command(
                    "Input.dispatchMouseEvent",
                    json!({"type":"mousePressed","x":x,"y":y,"button":"left","clickCount":1}),
                )
                .await?;
                page.command(
                    "Input.dispatchMouseEvent",
                    json!({"type":"mouseReleased","x":x,"y":y,"button":"left","clickCount":1}),
                )
                .await?;
                page.diagnostic("send_mouse_dispatched", "").await;
            } else {
                page.diagnostic("send_button_missing_enter_fallback", "")
                    .await;
                // The send button is absent in some ChatGPT layouts; Enter is
                // CodexPro's fallback after filling the editor.
                page.command(
                    "Input.dispatchKeyEvent",
                    json!({"type":"keyDown","key":"Enter","code":"Enter","windowsVirtualKeyCode":13}),
                )
                .await?;
                page.command(
                    "Input.dispatchKeyEvent",
                    json!({"type":"keyUp","key":"Enter","code":"Enter","windowsVirtualKeyCode":13}),
                )
                .await?;
                page.diagnostic("send_enter_dispatched", "").await;
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
                page.diagnostic("reply_timeout", "").await;
                return Err(BrowserError::Timeout);
            }
            let state = self.dom_state(&page).await?;
            page.note_state(&state).await;
            if let Some(manual) = manual_reason(&state) {
                *page.manual.lock().await = Some(manual.to_owned());
                page.diagnostic("manual_check_after_send", manual).await;
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
                    page.diagnostic("reply_completed", "").await;
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
        let expression = ui_expression(&format!(
            r#"// A newly created CDP target can briefly be about:blank
            // before Chrome commits its requested ChatGPT navigation.
            if (location.href === 'about:blank' ||
                location.href === 'chrome://newtab/' ||
                location.href === 'chrome://new-tab-page/') {{
                return {{external:true, transient:true, page_kind:'initial_blank'}};
            }}
            if (location.origin !== 'https://chatgpt.com' &&
                location.origin !== 'https://chat.openai.com') {{
                const authentication = location.origin === 'https://auth.openai.com' ||
                    location.origin === 'https://login.openai.com';
                return {{external:true, transient:false,
                    page_kind:authentication ? 'authentication' : 'external_other'}};
            }}
            const editor = findEditor();
            // Hidden verification/login widgets are common in an authenticated
            // session. Only a *visible* challenge requires human intervention.
            const challenge = [...document.querySelectorAll(
                'iframe[src*="challenges.cloudflare.com"], [class*="cf-turnstile"], #challenge-running, #challenge-form'
            )].some(isVisible);
            const login = [...document.querySelectorAll(
                '[data-testid="login-button"], a[href*="/auth/login"]'
            )].some(isVisible);
            const assistants = [...document.querySelectorAll({})];
            const last = assistants[assistants.length - 1];
            const content = last?.querySelector('.markdown, [data-message-content], .prose') || last;
            const stop = !!document.querySelector('[data-testid="stop-button"]');
            const copy = !!last?.querySelector('[data-testid="copy-turn-action-button"]');
            return {{url:location.href, page_kind:'chatgpt',
                composer:!!editor, challenge, login,
                assistant_count:assistants.length, last_text:content?.innerText || '',
                complete:copy || !stop}};"#,
            json!(ASSISTANTS)
        ));
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
        return Some(if state["page_kind"] == "authentication" {
            "authentication"
        } else {
            "verification"
        });
    }
    if state["challenge"] == true {
        return Some("verification");
    }
    // An optional login control can coexist with a functioning textbox.
    // CodexPro accepts the visible composer as evidence the user can type.
    if state["composer"] != true {
        return Some("authentication");
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn manual_login_launch_has_no_automation_or_debugging_arguments() {
        let command =
            manual_login_command(Path::new("chrome.exe"), Path::new("C:/NovaGPT/browser"));
        let args: Vec<String> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert!(args.iter().any(|arg| arg.starts_with("--user-data-dir=")));
        assert!(args.iter().any(|arg| arg == HOME));
        for disallowed in [
            "--remote-debugging-port",
            "--remote-debugging-pipe",
            "--enable-automation",
            "--headless",
            "--disable-web-security",
            "--ignore-certificate-errors",
        ] {
            assert!(
                !args.iter().any(|arg| arg.starts_with(disallowed)),
                "{disallowed}"
            );
        }
    }
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
        // The login link may be present while the usable composer is visible.
        assert_eq!(
            manual_reason(&json!({"composer":true,"challenge":false,"login":true})),
            None
        );
        assert_eq!(
            manual_reason(&json!({"external":true,"page_kind":"authentication"})),
            Some("authentication")
        );
        assert_eq!(
            manual_reason(&json!({"external":true,"page_kind":"external_other"})),
            Some("verification")
        );
        // Visible verification always needs a person, even with an editor.
        assert_eq!(
            manual_reason(&json!({"composer":true,"challenge":true,"login":false})),
            Some("verification")
        );
    }
    #[test]
    fn ui_adapter_discovers_editable_textboxes_and_requires_visible_controls() {
        let expression = ui_expression("return !!findEditor() && !!findSendButton();");
        assert!(expression.contains("[role='textbox'][contenteditable='true']"));
        assert!(expression.contains("getBoundingClientRect()"));
        assert!(expression.contains("el.disabled || el.readOnly"));
        assert!(expression.contains("aria-disabled"));
        assert!(expression.contains("findSendButton()"));
    }
    #[tokio::test]
    async fn subagent_send_uses_cdp_text_and_physical_send_button() {
        // A local, disposable CDP websocket stub proves the command sequence
        // without contacting ChatGPT, using a real Chrome profile or sending data.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local CDP fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("fixture client");
            let mut socket = tokio_tungstenite::accept_async(stream)
                .await
                .expect("fixture websocket");
            let mut inserted = false;
            let mut pressed = false;
            let mut released = false;
            let mut console_logged = false;
            let mut reply_logged = false;
            let mut state_polls = 0;
            while let Some(Ok(Message::Text(raw))) = socket.next().await {
                let request: Value = serde_json::from_str(&raw).expect("CDP request");
                let method = request["method"].as_str().expect("CDP method");
                let result = match method {
                    "Runtime.evaluate" => {
                        let expression = request["params"]["expression"]
                            .as_str()
                            .expect("expression");
                        if expression.contains("console.log(")
                            && expression.contains("[NovaGPT/browser]")
                        {
                            console_logged = true;
                            if expression.contains("reply_completed") {
                                reply_logged = true;
                            }
                        }
                        let value = if expression.contains("assistant_count:assistants.length") {
                            state_polls += 1;
                            if state_polls <= 2 {
                                json!({
                                    "external":true,
                                    "transient":true,
                                    "page_kind":"initial_blank"
                                })
                            } else {
                                json!({
                                    "url":"https://chatgpt.com/c/mock",
                                    "page_kind":"chatgpt",
                                    "composer":true,"challenge":false,"login":false,
                                    "assistant_count":if released { 1 } else { 0 },
                                    "last_text":if released { "Fixture answer" } else { "" },
                                    "complete":true
                                })
                            }
                        } else if expression.contains("document.activeElement === el") {
                            json!(true)
                        } else if expression.contains("return editorText(findEditor())") {
                            json!(if inserted { "Fixture prompt" } else { "" })
                        } else if expression.contains("rect.left + rect.width/2") {
                            json!({"x": 100.0,"y": 40.0})
                        } else {
                            Value::Null
                        };
                        if expression.contains("assistant_count:assistants.length")
                            && (3..=4).contains(&state_polls)
                        {
                            json!({"exceptionDetails":{"text":"navigation context replaced"}})
                        } else {
                            json!({"result":{"value":value}})
                        }
                    }
                    "Input.insertText" => {
                        inserted = request["params"]["text"] == "Fixture prompt";
                        json!({})
                    }
                    "Input.dispatchMouseEvent" => {
                        if request["params"]["type"] == "mousePressed" {
                            pressed = true;
                        } else if request["params"]["type"] == "mouseReleased" {
                            released = pressed;
                        }
                        json!({})
                    }
                    "Input.dispatchKeyEvent" => panic!("send button available; Enter not needed"),
                    _ => panic!("unexpected CDP method: {method}"),
                };
                socket
                    .send(Message::Text(
                        json!({"id":request["id"],"result":result})
                            .to_string()
                            .into(),
                    ))
                    .await
                    .expect("CDP fixture response");
                if released && reply_logged {
                    return inserted && pressed && released && console_logged;
                }
            }
            false
        });
        let (socket, _) = connect_async(format!("ws://127.0.0.1:{port}/"))
            .await
            .expect("fixture websocket client");
        let executable = tempfile::NamedTempFile::new().expect("fixture executable");
        let profile = tempfile::tempdir().expect("fixture profile");
        let browser = ChatGptBrowser::new(executable.path(), profile.path().join("owned"))
            .expect("fixture browser");
        browser.inner.pages.lock().await.insert(
            "agent-test123".into(),
            Arc::new(Page {
                target_id: "abcdef".into(),
                socket: Mutex::new(socket),
                next_command: AtomicU64::new(1),
                cancelled: AtomicBool::new(false),
                pending_before: Mutex::new(None),
                manual: Mutex::new(None),
                recent_events: Mutex::new(VecDeque::new()),
                previous_state: Mutex::new(None),
                reporter: None,
            }),
        );
        let reply = timeout(
            Duration::from_secs(8),
            browser.send("agent-test123", "Fixture prompt"),
        )
        .await
        .expect("mock send timeout")
        .expect("mock send result");
        assert_eq!(reply.content, "Fixture answer");
        assert_eq!(
            reply.conversation_url.as_deref(),
            Some("https://chatgpt.com/c/mock")
        );
        let diagnostics = browser.diagnostics(Some("agent-test123")).await;
        assert_eq!(diagnostics.last_step.as_deref(), Some("reply_completed"));
        assert!(
            diagnostics
                .recent_events
                .iter()
                .any(|step| step.contains("page_kind=initial_blank"))
        );
        assert!(
            diagnostics
                .recent_events
                .iter()
                .any(|step| step.contains("page_kind=chatgpt"))
        );
        assert!(
            diagnostics
                .recent_events
                .iter()
                .any(|step| step == "composer_ready")
        );
        assert!(
            diagnostics
                .recent_events
                .iter()
                .any(|step| step == "text_verified")
        );
        assert!(
            diagnostics
                .recent_events
                .iter()
                .any(|step| step == "send_mouse_dispatched")
        );
        assert!(diagnostics.recent_events.len() <= MAX_DIAGNOSTIC_EVENTS);
        let serialized = serde_json::to_string(&diagnostics).expect("diagnostic json");
        assert!(!serialized.contains("Fixture prompt"));
        assert!(!serialized.contains("Fixture answer"));
        assert!(!serialized.contains("https://chatgpt.com/c/mock"));
        assert!(
            timeout(Duration::from_secs(2), server)
                .await
                .expect("fixture completion")
                .expect("fixture join")
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
