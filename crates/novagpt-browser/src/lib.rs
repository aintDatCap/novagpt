//! Dedicated visible Chrome transport for ChatGPT subagents, backed by the
//! native ChromeDriver/WebDriver lifecycle. Never reads Chrome
//! cookies/credentials; manual challenges are for the user.
mod bridge;
mod interaction;
pub use bridge::{BridgeError, BrowserBridge, BrowserTab, BrowserTabText};
pub use interaction::InteractionPacing;
/// Native managed Chrome/ChromeDriver lifecycle. Exposed so the CLI and
/// embedders can install/inspect a verified driver without starting a browser.
pub use novagpt_browser_driver::undetected as managed;
use novagpt_browser_driver::undetected::{
    BackgroundTarget, ChromeDriverStore, DriverChoice, ManagedConfig, UndetectedChrome,
    UpstreamError, WebDriverTab,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use thiserror::Error;
use tokio::{
    sync::Mutex,
    time::{Duration, Instant, sleep},
};

const HOME: &str = "https://chatgpt.com/";
const BROWSE_TAB: &str = "browse-main";
const MAX_BROWSE_TEXT: usize = 20_000;
const MAX_SCREENSHOT_BASE64: usize = 8 * 1024 * 1024;
const MAX_SELECTOR: usize = 512;
const MAX_INPUT_TEXT: usize = 16_000;
const MAX_DEV_EVENTS: usize = 80;
// Match textbox-first discovery, but require a real visible,
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
    #[error("ChatGPT navigation did not complete; no prompt was submitted")]
    NavigationTimeout,
    #[error("ChatGPT tab could not be made visible and focused; no prompt was submitted")]
    InteractionBlocked,
    #[error("ChatGPT composer unavailable after waiting; no prompt was submitted")]
    ComposerUnavailable,
    #[error("Chrome disconnected after possible prompt submission; no automatic replay")]
    AmbiguousSubmission,
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
    #[error(
        "Browser navigation requires public HTTPS or a loopback HTTP(S) URL without credentials"
    )]
    UnsafeUrl,
    #[error("Browser control is invalid, hidden, disabled or unavailable")]
    InvalidControl,
    #[error("Development origin must be an explicit loopback HTTP origin with a port")]
    InvalidDevOrigin,
}

impl From<UpstreamError> for BrowserError {
    fn from(value: UpstreamError) -> Self {
        match value {
            UpstreamError::Chrome
            | UpstreamError::ChromeStartup
            | UpstreamError::UnsupportedPlatform
            | UpstreamError::VersionMismatch => Self::NotRunning,
            UpstreamError::InvalidProfile => Self::InvalidProfile,
            UpstreamError::ProfileBusy => Self::ProfileBusy,
            UpstreamError::Timeout | UpstreamError::ServiceTimeout => Self::Timeout,
            UpstreamError::Cancelled => Self::Cancelled,
            UpstreamError::TooLarge => Self::TooLarge,
            UpstreamError::SessionExpired | UpstreamError::TabClosed => Self::Unavailable,
            UpstreamError::InvalidConfig
            | UpstreamError::StoreNotOwned
            | UpstreamError::WebDriver
            | UpstreamError::Protocol
            | UpstreamError::InvalidArchive
            | UpstreamError::Integrity
            | UpstreamError::Manifest
            | UpstreamError::Download
            | UpstreamError::Unpatchable
            | UpstreamError::ForeignPatch
            | UpstreamError::Service
            | UpstreamError::Io => Self::Protocol,
        }
    }
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

/// Screenshot data is only returned as an MCP image block, never as a text log.
#[derive(Clone, Debug)]
pub struct BrowserScreenshot {
    pub png_base64: String,
    pub url: String,
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct BrowserControl {
    pub tag: String,
    pub label: String,
    pub selector: Option<String>,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct BrowserInspection {
    pub url: String,
    pub title: String,
    pub viewport_width: u32,
    pub viewport_height: u32,
    pub document_width: u32,
    pub broken_images: usize,
    pub headings: Vec<String>,
    pub controls: Vec<BrowserControl>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserConsoleEntry {
    pub kind: String,
    pub message: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserConsole {
    pub url: String,
    pub entries: Vec<BrowserConsoleEntry>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserViewport {
    pub width: u32,
    pub height: u32,
    pub device_scale_factor: f64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserReply {
    pub content: String,
    pub conversation_url: Option<String>,
}

struct Page {
    transport: WebDriverTab,
    background: Option<BackgroundTarget>,
    pacing: InteractionPacing,
    cancelled: AtomicBool,
    // A crashed tab must never cause automatic replay after a send event.
    submission_attempted: AtomicBool,
    send_lock: Mutex<()>,
    pending_before: Mutex<Option<usize>>,
    manual: Mutex<Option<String>>,
    recent_events: Mutex<VecDeque<String>>,
    previous_state: Mutex<Option<String>>,
    console: Mutex<VecDeque<BrowserConsoleEntry>>,
    console_seen: Mutex<VecDeque<String>>,
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
    }

    async fn note_state(&self, state: &Value) {
        let flags = format!(
            "phase={} page_kind={} composer={} challenge={} login={} external={} assistants={} complete={}",
            page_phase(state).as_str(),
            match state["page_kind"].as_str() {
                Some("chatgpt") => "chatgpt",
                Some("initial_blank") => "initial_blank",
                Some("navigation") => "navigation",
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

    async fn refresh_console(&self) {
        let Ok(entries) = self.transport.browser_log().await else {
            return;
        };
        let mut seen = self.console_seen.lock().await;
        let mut console = self.console.lock().await;
        for entry in entries.into_iter().take(MAX_DEV_EVENTS) {
            if let Some(entry) = browser_log_entry(&entry) {
                let key = format!("{}\u{0}{}", entry.kind, entry.message);
                if seen.iter().any(|known| known == &key) {
                    continue;
                }
                if seen.len() == MAX_DEV_EVENTS {
                    seen.pop_front();
                }
                seen.push_back(key);
                if console.len() == MAX_DEV_EVENTS {
                    console.pop_front();
                }
                console.push_back(entry);
            }
        }
    }

    async fn command(&self, method: &str, params: Value) -> Result<Value, BrowserError> {
        if let Some(target) = &self.background {
            target
                .command(method, params)
                .await
                .map_err(BrowserError::from)
        } else {
            // Only disposable WebDriver fixtures have no background CDP port.
            self.transport
                .cdp_execute(method, params)
                .await
                .map_err(BrowserError::from)
        }
    }

    async fn eval(&self, source: &str) -> Result<Value, BrowserError> {
        if let Some(target) = &self.background {
            target.evaluate(source).await.map_err(BrowserError::from)
        } else {
            self.transport
                .evaluate(source)
                .await
                .map_err(BrowserError::from)
        }
    }

    async fn insert_text(&self, text: &str) -> Result<(), BrowserError> {
        if let Some(target) = &self.background {
            target
                .command("Input.insertText", json!({"text":text}))
                .await
                .map(|_| ())
                .map_err(BrowserError::from)
        } else {
            self.transport
                .insert_text(text)
                .await
                .map_err(BrowserError::from)
        }
    }

    async fn click_at(&self, x: f64, y: f64) -> Result<(), BrowserError> {
        if !x.is_finite()
            || !y.is_finite()
            || !(0.0..=100_000.0).contains(&x)
            || !(0.0..=100_000.0).contains(&y)
        {
            return Err(BrowserError::Protocol);
        }
        // A person moves the pointer before pressing a button. Emit a short,
        // jittered path so the press is not the first mouse event the page
        // ever sees at that point.
        let steps = self.pacing.pointer_steps;
        if steps > 0 {
            let start_x = (x - 120.0).max(1.0);
            let start_y = (y + 60.0).max(1.0);
            for step in 1..=steps {
                let progress = f64::from(step) / f64::from(steps);
                let point_x = start_x + (x - start_x) * progress + interaction::wobble(1.5);
                let point_y = start_y + (y - start_y) * progress + interaction::wobble(1.5);
                self.command(
                    "Input.dispatchMouseEvent",
                    json!({"type":"mouseMoved","x":point_x,"y":point_y,"buttons":0}),
                )
                .await?;
                sleep(interaction::jittered(
                    self.pacing.pointer_delay,
                    self.pacing.pointer_jitter,
                ))
                .await;
            }
        }
        if let Some(target) = &self.background {
            target
                .commands(&[
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
                .map(|_| ())
                .map_err(BrowserError::from)
        } else {
            self.transport
                .click_at(x, y)
                .await
                .map_err(BrowserError::from)
        }
    }

    async fn press_enter(&self) -> Result<(), BrowserError> {
        if let Some(target) = &self.background {
            target.commands(&[
                ("Input.dispatchKeyEvent", json!({"type":"keyDown","key":"Enter","code":"Enter","windowsVirtualKeyCode":13})),
                ("Input.dispatchKeyEvent", json!({"type":"keyUp","key":"Enter","code":"Enter","windowsVirtualKeyCode":13})),
            ]).await.map(|_| ()).map_err(BrowserError::from)
        } else {
            self.transport
                .press_enter()
                .await
                .map_err(BrowserError::from)
        }
    }

    /// Run a small ordered batch of input commands. The background transport
    /// keeps them on one socket; the WebDriver fixture sends them in order.
    async fn input_batch(&self, requests: &[(&str, Value)]) -> Result<(), BrowserError> {
        if let Some(target) = &self.background {
            target
                .commands(requests)
                .await
                .map(|_| ())
                .map_err(BrowserError::from)
        } else {
            for (method, params) in requests {
                self.transport
                    .cdp_execute(method, params.clone())
                    .await
                    .map_err(BrowserError::from)?;
            }
            Ok(())
        }
    }

    /// Insert the prompt the way a person would. Short ASCII prompts are sent
    /// as individual key presses (with Shift+Enter for newlines); longer or
    /// non-ASCII prompts use one trusted text insertion because Chrome does
    /// not expose a user-permitted clipboard paste to an automation client.
    /// Returns `true` when keystrokes were used.
    async fn type_prompt(&self, text: &str) -> Result<bool, BrowserError> {
        let normalized = text.replace("\r\n", "\n");
        let characters = normalized.chars().count();
        if !normalized.is_ascii() || characters > self.pacing.max_typed_chars {
            self.insert_text(&normalized).await?;
            return Ok(false);
        }
        let mut batch: Vec<(&str, Value)> = Vec::with_capacity(4);
        for character in normalized.chars() {
            if character == '\n' {
                if !batch.is_empty() {
                    self.input_batch(&batch).await?;
                    batch.clear();
                }
                // Enter alone sends a message in ChatGPT; Shift+Enter inserts
                // the newline a person would type.
                self.input_batch(&[
                    (
                        "Input.dispatchKeyEvent",
                        json!({"type":"keyDown","key":"Enter","code":"Enter","windowsVirtualKeyCode":13,"modifiers":8,"text":"\r","unmodifiedText":"\r"}),
                    ),
                    (
                        "Input.dispatchKeyEvent",
                        json!({"type":"keyUp","key":"Enter","code":"Enter","windowsVirtualKeyCode":13,"modifiers":8}),
                    ),
                ])
                .await?;
            } else {
                let key = character.to_string();
                batch.push((
                    "Input.dispatchKeyEvent",
                    json!({"type":"keyDown","key":key,"text":key,"unmodifiedText":key}),
                ));
                batch.push(("Input.dispatchKeyEvent", json!({"type":"keyUp","key":key})));
                if batch.len() == 4 {
                    self.input_batch(&batch).await?;
                    batch.clear();
                }
            }
            sleep(interaction::jittered(
                self.pacing.key_delay,
                self.pacing.key_jitter,
            ))
            .await;
        }
        if !batch.is_empty() {
            self.input_batch(&batch).await?;
        }
        Ok(true)
    }

    /// Raise this tab and require the page to report itself visible and
    /// focused before input. A hidden or unfocused document cannot receive
    /// human input, so dispatching keystrokes there is both ineffective and
    /// an automation tell.
    async fn ensure_interactive(&self) -> Result<(), BrowserError> {
        if let Some(target) = &self.background {
            target.bring_to_front().await.map_err(BrowserError::from)?;
        } else {
            self.transport
                .cdp_execute("Page.bringToFront", json!({}))
                .await
                .map_err(BrowserError::from)?;
        }
        let deadline = Instant::now() + self.pacing.reveal_timeout;
        loop {
            let state = self
                .eval("({visible:document.visibilityState === 'visible', focused:document.hasFocus()})")
                .await?;
            if state["visible"] == true && state["focused"] == true {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(BrowserError::InteractionBlocked);
            }
            sleep(Duration::from_millis(100)).await;
        }
    }
}
struct Inner {
    config: ManagedConfig,
    runtime: Mutex<Option<UndetectedChrome>>,
    dev_origin: Option<reqwest::Url>,
    create_lock: Mutex<()>,
    pages: Mutex<HashMap<String, Arc<Page>>>,
    reporter: Option<DiagnosticReporter>,
    pacing: InteractionPacing,
    last_open: Mutex<Option<Instant>>,
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
        let root = ChromeDriverStore::default_root().map_err(|_| BrowserError::NotRunning)?;
        let store = ChromeDriverStore::new(root).map_err(BrowserError::from)?;
        Self::with_driver(executable, profile, DriverChoice::Install(Arc::new(store)))
    }

    /// Construct with an explicit managed-driver source. The Chrome process is
    /// launched lazily on the first tab operation, matching the previous
    /// lifecycle and avoiding work when subagents are never used.
    pub fn with_driver(
        executable: impl AsRef<Path>,
        profile: impl AsRef<Path>,
        driver: DriverChoice,
    ) -> Result<Self, BrowserError> {
        let executable = executable.as_ref();
        let profile = profile.as_ref();
        if !executable.is_file() || !profile.is_absolute() {
            return Err(BrowserError::InvalidProfile);
        }
        if profile.exists() && profile.is_symlink() {
            return Err(BrowserError::InvalidProfile);
        }
        let config = ManagedConfig::new(executable, profile, driver);
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                runtime: Mutex::new(None),
                dev_origin: None,
                create_lock: Mutex::new(()),
                pages: Mutex::new(HashMap::new()),
                reporter: None,
                pacing: InteractionPacing::human(),
                last_open: Mutex::new(None),
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_test_session(
        session: novagpt_browser_driver::undetected::WebDriverSession,
    ) -> Self {
        use std::path::PathBuf;
        let placeholder = PathBuf::from("/nonexistent/novagpt-test");
        let config = ManagedConfig::new(
            &placeholder,
            "/nonexistent/novagpt-test-profile",
            DriverChoice::Existing(placeholder.clone()),
        );
        Self {
            inner: Arc::new(Inner {
                config,
                runtime: Mutex::new(Some(UndetectedChrome::from_session_for_test(session))),
                dev_origin: None,
                create_lock: Mutex::new(()),
                pages: Mutex::new(HashMap::new()),
                reporter: None,
                pacing: InteractionPacing::instant(),
                last_open: Mutex::new(None),
            }),
        }
    }

    async fn chrome(&self) -> Result<UndetectedChrome, BrowserError> {
        let mut runtime = self.inner.runtime.lock().await;
        if let Some(chrome) = runtime.as_ref() {
            return Ok(chrome.clone());
        }
        let chrome = UndetectedChrome::launch(self.inner.config.clone())
            .await
            .map_err(BrowserError::from)?;
        *runtime = Some(chrome.clone());
        Ok(chrome)
    }

    /// Explicitly close the WebDriver session, ChromeDriver child, and owned
    /// Chrome. Idempotent; normally the process is terminated when the last
    /// handle is dropped.
    pub async fn shutdown(&self) {
        let chrome = self.inner.runtime.lock().await.take();
        self.inner.pages.lock().await.clear();
        if let Some(chrome) = chrome {
            chrome.shutdown().await;
        }
    }

    /// Optionally restrict local browsing to one exact HTTP development origin.
    /// Without this setting, standard loopback hosts are available by default.
    pub fn with_dev_origin(mut self, origin: &str) -> Result<Self, BrowserError> {
        let approved = local_dev_origin(origin)?;
        let inner = Arc::get_mut(&mut self.inner).ok_or(BrowserError::Protocol)?;
        inner.dev_origin = Some(approved);
        Ok(self)
    }

    /// Replace the human interaction pacing. Production uses
    /// [`InteractionPacing::human`]; deterministic tests and embedders that
    /// drive their own page can use [`InteractionPacing::instant`].
    pub fn with_pacing(mut self, pacing: InteractionPacing) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.pacing = pacing;
        }
        self
    }

    fn approved_url(&self, input: &str) -> Result<reqwest::Url, BrowserError> {
        if let Ok(url) = public_https_url(input) {
            return Ok(url);
        }
        let url = loopback_url(input)?;
        if let Some(origin) = &self.inner.dev_origin
            && url.origin() != origin.origin()
        {
            return Err(BrowserError::UnsafeUrl);
        }
        Ok(url)
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
    /// this window, then starts NovaGPT's automation-capable server separately.
    pub async fn manual_login(
        executable: impl AsRef<Path>,
        profile: impl AsRef<Path>,
    ) -> Result<(), BrowserError> {
        novagpt_browser_driver::undetected::manual_login(
            executable.as_ref(),
            profile.as_ref(),
            HOME,
        )
        .await
        .map_err(BrowserError::from)
    }

    async fn create_tab(
        &self,
        chrome: &UndetectedChrome,
        url: &str,
    ) -> Result<WebDriverTab, BrowserError> {
        match chrome.new_tab(url).await {
            Ok(tab) => Ok(tab),
            Err(
                UpstreamError::SessionExpired
                | UpstreamError::TabClosed
                | UpstreamError::WebDriver
                | UpstreamError::Chrome
                | UpstreamError::ChromeStartup,
            ) => {
                chrome.reconnect().await.map_err(BrowserError::from)?;
                chrome.new_tab(url).await.map_err(BrowserError::from)
            }
            Err(error) => Err(BrowserError::from(error)),
        }
    }

    pub async fn open(&self, id: &str) -> Result<(), BrowserError> {
        if !safe_id(id) {
            return Err(BrowserError::Protocol);
        }
        let _serial = self.inner.create_lock.lock().await;
        // Tabs are created one at a time. A short randomized gap keeps six
        // concurrent workers from starting six top-level navigations in the
        // same instant, without requiring every caller to pace itself.
        let gap =
            interaction::jittered(self.inner.pacing.tab_gap, self.inner.pacing.tab_gap_jitter);
        let previous = *self.inner.last_open.lock().await;
        if let Some(previous) = previous {
            let elapsed = previous.elapsed();
            if elapsed < gap {
                sleep(gap - elapsed).await;
            }
        }
        *self.inner.last_open.lock().await = Some(Instant::now());
        let chrome = self.chrome().await?;
        {
            let mut pages = self.inner.pages.lock().await;
            // Preserve other agents' stale handles until their send state can
            // determine whether a pre-submission recovery is safe.
            if pages
                .get(id)
                .is_some_and(|page| !page.transport.is_closed())
            {
                return Err(BrowserError::Unavailable);
            }
            pages.remove(id);
        }
        let transport = self
            .create_tab(&chrome, if id == BROWSE_TAB { "" } else { HOME })
            .await?;
        let background = if chrome.debug_port() == 0 {
            // WebDriver-only test fixture.
            None
        } else {
            Some(
                BackgroundTarget::new(chrome.debug_port(), transport.handle())
                    .map_err(BrowserError::from)?,
            )
        };
        let page = Arc::new(Page {
            transport,
            background,
            pacing: self.inner.pacing,
            cancelled: AtomicBool::new(false),
            submission_attempted: AtomicBool::new(false),
            send_lock: Mutex::new(()),
            pending_before: Mutex::new(None),
            manual: Mutex::new(None),
            recent_events: Mutex::new(VecDeque::new()),
            previous_state: Mutex::new(None),
            console: Mutex::new(VecDeque::new()),
            console_seen: Mutex::new(VecDeque::new()),
            reporter: self.inner.reporter.clone(),
        });
        page.diagnostic("tab_opened", "").await;
        self.inner.pages.lock().await.insert(id.to_owned(), page);
        Ok(())
    }

    /// Open a visible, dedicated browsing tab, separate from all subagent tabs.
    /// This operation reuses the same owned Chrome process/profile.
    pub async fn browse_open(&self) -> Result<BrowserDiagnostics, BrowserError> {
        // Validate the owned Chrome process before reusing a remembered tab.
        let _ = self.chrome().await?;
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
        let target = self.approved_url(url)?;
        let page = self.page(BROWSE_TAB).await?;
        let previous = page
            .eval("location.href")
            .await?
            .as_str()
            .unwrap_or("")
            .to_owned();
        // Diagnostics from a previous browsing destination must not be
        // mistaken for errors produced by the next website.
        page.console.lock().await.clear();
        page.console_seen.lock().await.clear();
        page.transport
            .goto(target.as_str())
            .await
            .map_err(BrowserError::from)?;
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
                        self.approved_url(current)?;
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
        self.approved_url(url)?;
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

    /// Inspect a bounded set of visible elements and common layout problems.
    /// This returns page data, not executable JavaScript or hidden DOM.
    pub async fn browse_inspect(&self) -> Result<BrowserInspection, BrowserError> {
        let page = self.page(BROWSE_TAB).await?;
        let value = page.eval(r#"(() => {
            const visible = el => {
                const r = el.getBoundingClientRect();
                const s = getComputedStyle(el);
                return r.width > 0 && r.height > 0 &&
                    s.display !== 'none' && s.visibility !== 'hidden';
            };
            const short = s => String(s || '').trim().slice(0, 160);
            const controls = [...document.querySelectorAll('button, a, input, textarea, select, [role="button"]')]
                .filter(visible).slice(0, 60).map(el => {
                    const testid = el.getAttribute('data-testid');
                    const selector = el.id ? '#' + CSS.escape(el.id) :
                        testid ? '[data-testid=' + JSON.stringify(testid) + ']' : null;
                    return {
                        tag: el.tagName.toLowerCase(),
                        label: short(el.getAttribute('aria-label') || el.innerText ||
                            el.getAttribute('placeholder') || el.getAttribute('type')),
                        selector
                    };
                });
            return {
                url: location.href,
                title: short(document.title),
                viewport_width: innerWidth,
                viewport_height: innerHeight,
                document_width: document.documentElement.scrollWidth,
                broken_images: [...document.images].filter(img => !img.complete || img.naturalWidth === 0).length,
                headings: [...document.querySelectorAll('h1,h2,h3')]
                    .filter(visible).slice(0, 40).map(el => short(el.innerText)),
                controls
            };
        })()"#).await?;
        let url = value["url"].as_str().ok_or(BrowserError::Protocol)?;
        self.approved_url(url)?;
        serde_json::from_value(value).map_err(|_| BrowserError::Protocol)
    }

    /// Capture the visible viewport only (not a full-page or private offscreen dump).
    /// The MCP adapter renders the returned PNG as an actual image content block.
    pub async fn browse_screenshot(&self) -> Result<BrowserScreenshot, BrowserError> {
        let page = self.page(BROWSE_TAB).await?;
        let before = page
            .eval("({url:location.href,width:innerWidth,height:innerHeight})")
            .await?;
        let url = before["url"].as_str().ok_or(BrowserError::Protocol)?;
        self.approved_url(url)?;
        page.command("Page.enable", json!({})).await?;
        let screenshot = page
            .command(
                "Page.captureScreenshot",
                json!({"format":"png","captureBeyondViewport":false,"fromSurface":true}),
            )
            .await?;
        let data = screenshot["data"].as_str().ok_or(BrowserError::Protocol)?;
        if data.len() > MAX_SCREENSHOT_BASE64 {
            return Err(BrowserError::TooLarge);
        }
        // A page can navigate during capture: never return content from an
        // unapproved redirect, even when the original page was approved.
        let after = page.eval("location.href").await?;
        let final_url = after.as_str().ok_or(BrowserError::Protocol)?;
        self.approved_url(final_url)?;
        if url != final_url {
            return Err(BrowserError::Unavailable);
        }
        Ok(BrowserScreenshot {
            png_base64: data.to_owned(),
            url: url.to_owned(),
            width: before["width"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32,
            height: before["height"].as_u64().unwrap_or(0).min(u32::MAX as u64) as u32,
        })
    }

    /// Emulate a desktop viewport to test responsive layouts. Dimensions and
    /// scale are bounded to avoid excessive image/memory allocations.
    pub async fn browse_viewport(
        &self,
        width: u32,
        height: u32,
        device_scale_factor: f64,
    ) -> Result<BrowserViewport, BrowserError> {
        if !(320..=2560).contains(&width)
            || !(240..=1800).contains(&height)
            || !device_scale_factor.is_finite()
            || !(1.0..=3.0).contains(&device_scale_factor)
            || width as f64 * height as f64 * device_scale_factor.powi(2) > 10_000_000.0
        {
            return Err(BrowserError::InvalidControl);
        }
        let page = self.page(BROWSE_TAB).await?;
        let location = page.eval("location.href").await?;
        self.approved_url(location.as_str().ok_or(BrowserError::Protocol)?)?;
        page.command(
            "Emulation.setDeviceMetricsOverride",
            json!({
                "width":width, "height":height,
                "deviceScaleFactor":device_scale_factor, "mobile":false
            }),
        )
        .await?;
        Ok(BrowserViewport {
            width,
            height,
            device_scale_factor,
        })
    }

    /// Click a visible, unobscured element using a physical browser mouse event.
    pub async fn browse_click(&self, selector: &str) -> Result<bool, BrowserError> {
        validate_selector(selector)?;
        let page = self.page(BROWSE_TAB).await?;
        let location = page.eval("location.href").await?;
        self.approved_url(location.as_str().ok_or(BrowserError::Protocol)?)?;
        let expression = format!(
            r#"(() => {{
            let el;
            try {{ el = document.querySelector({selector}); }} catch {{ return null; }}
            if (!el || el.disabled || el.getAttribute('aria-disabled') === 'true') return null;
            const r = el.getBoundingClientRect(), s = getComputedStyle(el);
            if (r.width <= 0 || r.height <= 0 || s.display === 'none' ||
                s.visibility === 'hidden') return null;
            const x = Math.max(0, Math.min(innerWidth - 1, r.left + r.width / 2));
            const y = Math.max(0, Math.min(innerHeight - 1, r.top + r.height / 2));
            if (x < r.left || x > r.right || y < r.top || y > r.bottom) return null;
            const hit = document.elementFromPoint(x, y);
            if (!hit || !(el === hit || el.contains(hit))) return null;
            return {{x,y,href: el.closest('a[href]')?.href || null}};
        }})()"#,
            selector = json!(selector)
        );
        let point = page.eval(&expression).await?;
        if let Some(href) = point["href"].as_str() {
            self.approved_url(href)?;
        }
        let (Some(x), Some(y)) = (point["x"].as_f64(), point["y"].as_f64()) else {
            return Err(BrowserError::InvalidControl);
        };
        page.transport
            .click_at(x, y)
            .await
            .map_err(BrowserError::from)?;
        Ok(true)
    }

    /// Type into a visible editable web form field; never fill passwords or
    /// file inputs. Authentication and verification remain entirely manual.
    pub async fn browse_type(&self, selector: &str, text: &str) -> Result<bool, BrowserError> {
        validate_selector(selector)?;
        if text.is_empty() || text.len() > MAX_INPUT_TEXT {
            return Err(BrowserError::InvalidControl);
        }
        let page = self.page(BROWSE_TAB).await?;
        let location = page.eval("location.href").await?;
        self.approved_url(location.as_str().ok_or(BrowserError::Protocol)?)?;
        let expression = format!(
            r#"(() => {{
            let el;
            try {{ el = document.querySelector({selector}); }} catch {{ return false; }}
            if (!el || el.disabled || el.readOnly || el.getAttribute('aria-disabled') === 'true') return false;
            const r = el.getBoundingClientRect(), s = getComputedStyle(el);
            if (r.width <= 0 || r.height <= 0 || s.display === 'none' || s.visibility === 'hidden')
                return false;
            const tag = el.tagName.toLowerCase();
            if (tag === 'input' && !['text','search','email','url','tel','number'].includes(el.type))
                return false;
            if (tag !== 'input' && tag !== 'textarea' && !el.isContentEditable) return false;
            el.focus();
            if (tag === 'input' || tag === 'textarea') el.select();
            else {{
                const selection = getSelection(), range = document.createRange();
                range.selectNodeContents(el);
                selection.removeAllRanges();
                selection.addRange(range);
            }}
            return document.activeElement === el;
        }})()"#,
            selector = json!(selector)
        );
        if page.eval(&expression).await? != Value::Bool(true) {
            return Err(BrowserError::InvalidControl);
        }
        page.transport
            .insert_text(text)
            .await
            .map_err(BrowserError::from)?;
        Ok(true)
    }

    /// Read recent console messages, JavaScript exceptions and HTTP/network
    /// failures captured from the browsing tab. Stored only in process memory.
    pub async fn browse_console(&self) -> Result<BrowserConsole, BrowserError> {
        let page = self.page(BROWSE_TAB).await?;
        // Refresh ChromeDriver's browser log after validating the origin;
        // entries are redacted and bounded before they leave the process.
        let location = page.eval("location.href").await?;
        let url = location.as_str().ok_or(BrowserError::Protocol)?;
        self.approved_url(url)?;
        page.refresh_console().await;
        let entries = page.console.lock().await.iter().cloned().collect();
        Ok(BrowserConsole {
            url: url.to_owned(),
            entries,
        })
    }

    pub async fn browse_close(&self) -> Result<(), BrowserError> {
        self.close(BROWSE_TAB).await
    }

    async fn page(&self, id: &str) -> Result<Arc<Page>, BrowserError> {
        let page = self
            .inner
            .pages
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or(BrowserError::Unavailable)?;
        if page.transport.is_closed() {
            return Err(BrowserError::Unavailable);
        }
        Ok(page)
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
        let page_ready = target.is_some();
        let page_count = pages.len();
        drop(pages);
        let browser_state = {
            let runtime = self.inner.runtime.lock().await;
            match runtime.as_ref() {
                Some(chrome) => chrome.status().await.to_string(),
                None => "stopped".to_owned(),
            }
        };
        let last_step = recent_events.last().cloned();
        BrowserDiagnostics {
            browser_state,
            page_count,
            queued_page_creations: 0,
            manual_check: manual,
            page_ready,
            automation_driver: "chrome-driver-webdriver",
            recent_events,
            last_step,
        }
    }

    pub async fn send(&self, id: &str, prompt: &str) -> Result<BrowserReply, BrowserError> {
        if prompt.trim().is_empty() || prompt.len() > MAX_PROMPT {
            return Err(BrowserError::TooLarge);
        }
        let mut outcome = self.send_inner(id, prompt).await;
        if matches!(
            outcome,
            Err(BrowserError::Unavailable
                | BrowserError::Protocol
                | BrowserError::Timeout
                | BrowserError::NotRunning)
        ) {
            let attempted = self
                .inner
                .pages
                .lock()
                .await
                .get(id)
                .is_some_and(|page| page.submission_attempted.load(Ordering::Acquire));
            if attempted {
                return Err(BrowserError::AmbiguousSubmission);
            }
            // No possible submission occurred. Recover the driver/session and
            // retry the unsent prompt exactly once; this is the only automatic
            // replay path, and it is gated on submission_attempted == false.
            if self.reconnect_runtime().await.is_ok() {
                outcome = self.send_inner(id, prompt).await;
                let attempted = self
                    .inner
                    .pages
                    .lock()
                    .await
                    .get(id)
                    .is_some_and(|page| page.submission_attempted.load(Ordering::Acquire));
                if attempted && outcome.is_err() {
                    return Err(BrowserError::AmbiguousSubmission);
                }
            }
        }
        outcome
    }

    async fn reconnect_runtime(&self) -> Result<(), BrowserError> {
        let chrome = self.inner.runtime.lock().await.clone();
        let chrome = chrome.ok_or(BrowserError::NotRunning)?;
        chrome
            .reconnect()
            .await
            .map(|_| ())
            .map_err(BrowserError::from)
    }

    async fn send_inner(&self, id: &str, prompt: &str) -> Result<BrowserReply, BrowserError> {
        let page = match self.page(id).await {
            Ok(page) => page,
            Err(error) => {
                let stale = self.inner.pages.lock().await.get(id).cloned();
                if let Some(stale) = stale {
                    if stale.submission_attempted.load(Ordering::Acquire) {
                        stale.diagnostic("recovery_blocked_after_send", "").await;
                        return Err(BrowserError::AmbiguousSubmission);
                    }
                    // Only recover a page that never attempted submission.
                    // Keep the same owned profile; never replay a sent prompt.
                    self.open(id).await?;
                    let recovered = self.page(id).await?;
                    recovered.diagnostic("tab_recovered_before_send", "").await;
                    recovered
                } else {
                    return Err(error);
                }
            }
        };
        // The previous send poller must leave before a new prompt resets its
        // cancellation flag or reuses the per-tab pending response state.
        let _send_guard = page.send_lock.lock().await;
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
                // Navigation may switch execution contexts while the newly created
                // tab navigates. Keep the same bounded wait as the original adapter.
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
            match page_phase(&state) {
                PagePhase::Ready | PagePhase::Authentication | PagePhase::Verification => {
                    break state;
                }
                PagePhase::Navigation | PagePhase::Unavailable => {
                    if Instant::now() >= ready_deadline {
                        if page_phase(&state) == PagePhase::Navigation {
                            page.diagnostic("navigation_timeout", "").await;
                            return Err(BrowserError::NavigationTimeout);
                        }
                        page.diagnostic("composer_unavailable", "").await;
                        return Err(BrowserError::ComposerUnavailable);
                    }
                }
            }
            sleep(Duration::from_millis(250)).await;
        };
        if let Some(manual) = manual_reason(&state) {
            *page.manual.lock().await = Some(manual.to_string());
            page.diagnostic("manual_check_required", manual).await;
            return Err(BrowserError::Manual(manual));
        }
        *page.manual.lock().await = None;
        page.diagnostic("composer_ready", "").await;
        let pending = *page.pending_before.lock().await;
        let before = if let Some(before) = pending {
            before
        } else {
            let before = state["assistant_count"].as_u64().unwrap_or(0) as usize;
            // A human does not start typing the instant a composer renders.
            sleep(interaction::jittered(
                page.pacing.settle_after_ready,
                page.pacing.settle_jitter,
            ))
            .await;
            // Input into a hidden or unfocused tab is not something a person
            // can do. Raise this tab and refuse to type unless the document
            // confirms it is visible and focused.
            page.ensure_interactive().await?;
            page.diagnostic("tab_interactive", "").await;
            // Select the actual visible, editable textbox (not its wrapper).
            // Replacing a selection via browser input generates real input events,
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
            let typed = page.type_prompt(prompt).await?;
            page.diagnostic(
                if typed {
                    "text_input_typed"
                } else {
                    "text_input_inserted"
                },
                "",
            )
            .await;
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
            // A verification interstitial can appear after the composer was
            // classified as ready, including over an already-rendered page.
            // Re-check the live document immediately before submitting and
            // keep the unsent prompt for the manual-verification flow.
            let pre_submit = self.dom_state(&page).await?;
            page.note_state(&pre_submit).await;
            if let Some(manual) = manual_reason(&pre_submit) {
                *page.manual.lock().await = Some(manual.to_owned());
                page.diagnostic("manual_before_submit", manual).await;
                return Err(BrowserError::Manual(manual));
            }
            let editable = page
                .eval(&ui_expression(
                    r#"const el = findEditor();
                    return !!el && document.activeElement === el;"#,
                ))
                .await?;
            if editable != Value::Bool(true) {
                page.diagnostic("editor_not_focused", "").await;
                return Err(BrowserError::InteractionBlocked);
            }
            sleep(interaction::jittered(
                page.pacing.submit_delay,
                page.pacing.submit_jitter,
            ))
            .await;
            // Physical mouse events, rather than DOM .click(), match the
            // physical input that a browser automation click uses. The click
            // is only sent when the button is the topmost element at its own
            // centre, so a challenge overlay or dialog cannot be clicked
            // blindly.
            // Retry briefly for React to enable the button after typing.
            let mut button = Value::Null;
            let mut obscured = false;
            for _ in 0..8 {
                button = page
                    .eval(&ui_expression(
                        r#"const el = findSendButton();
                        if (!el) return null;
                        const rect = el.getBoundingClientRect();
                        const x = rect.left + rect.width/2;
                        const y = rect.top + rect.height/2;
                        const hit = document.elementFromPoint(x, y);
                        return {x, y, clear: !!hit && (hit === el || el.contains(hit))};"#,
                    ))
                    .await?;
                if button.is_object() {
                    obscured = button["clear"] != true;
                    break;
                }
                sleep(Duration::from_millis(125)).await;
            }
            if obscured {
                page.diagnostic("send_button_obscured", "").await;
                return Err(BrowserError::InteractionBlocked);
            }
            if let (Some(x), Some(y)) = (button["x"].as_f64(), button["y"].as_f64()) {
                page.diagnostic("send_button_found", "").await;
                // ChromeDriver can fail after the first event with unknown outcome.
                page.submission_attempted.store(true, Ordering::Release);
                page.click_at(x, y).await?;
                page.diagnostic("send_mouse_dispatched", "").await;
            } else {
                page.diagnostic("send_button_missing_enter_fallback", "")
                    .await;
                // The send button is absent in some ChatGPT layouts; Enter is
                // the fallback after filling the editor.
                page.submission_attempted.store(true, Ordering::Release);
                page.press_enter().await?;
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
                    page.submission_attempted.store(false, Ordering::Release);
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
            r#"// A newly created tab can briefly be about:blank
            // before Chrome commits its requested ChatGPT navigation.
            if (location.href === 'about:blank' ||
                location.href === 'chrome://newtab/' ||
                location.href === 'chrome://new-tab-page/') {{
                return {{external:true, transient:true, page_kind:'initial_blank'}};
            }}
            // Title is bounded metadata, never included in diagnostic records.
            const title = (document.title || '').slice(0, 96);
            if (document.readyState === 'loading') {{
                return {{transient:true, page_kind:'navigation', title}};
            }}
            if (location.origin !== 'https://chatgpt.com' &&
                location.origin !== 'https://chat.openai.com') {{
                const authentication = location.origin === 'https://auth.openai.com' ||
                    location.origin === 'https://login.openai.com';
                return {{external:true, transient:false, title,
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
            return {{url:location.href, title, page_kind:'chatgpt',
                composer:!!editor, challenge, login,
                assistant_count:assistants.length, last_text:content?.innerText || '',
                complete:copy || !stop}};"#,
            json!(ASSISTANTS)
        ));
        page.eval(&expression).await
    }

    /// A completion callback already contains the trusted-by-transport but
    /// unverified agent result. Stop the old poller without clicking ChatGPT's
    /// Stop button or destroying its visible conversation tab.
    pub async fn complete(&self, id: &str) -> Result<(), BrowserError> {
        let page = self.page(id).await?;
        page.cancelled.store(true, Ordering::Relaxed);
        *page.pending_before.lock().await = None;
        page.submission_attempted.store(false, Ordering::Release);
        Ok(())
    }

    pub async fn cancel(&self, id: &str) -> Result<(), BrowserError> {
        let page = self.page(id).await?;
        page.cancelled.store(true, Ordering::Relaxed);
        // Interrupt visible generation if possible; do not close the shared Chrome process.
        let _ = page.eval("(() => { const b=document.querySelector('[data-testid=stop-button]'); if(b) b.click(); return true })()").await;
        Ok(())
    }

    pub async fn close(&self, id: &str) -> Result<(), BrowserError> {
        if let Some(page) = self.inner.pages.lock().await.remove(id) {
            page.cancelled.store(true, Ordering::Release);
            page.transport.close().await.map_err(BrowserError::from)?;
        }
        Ok(())
    }
}
/// Restrict optional development scopes to an exact loopback HTTP origin.
/// Localhost, 127.0.0.1 and [::1] are otherwise supported by default;
/// LAN/private IPs, aliases and other intranet addresses remain forbidden.
fn local_dev_origin(input: &str) -> Result<reqwest::Url, BrowserError> {
    if input.len() > 200 || input.bytes().any(|b| b.is_ascii_control()) {
        return Err(BrowserError::InvalidDevOrigin);
    }
    let url = loopback_url(input).map_err(|_| BrowserError::InvalidDevOrigin)?;
    if url.scheme() != "http"
        || url.port().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(BrowserError::InvalidDevOrigin);
    }
    Ok(url)
}

/// Permit ordinary development servers bound to the local machine without a
/// startup flag. Match only canonical loopback host spellings; never infer
/// trust from private networks, DNS aliases or an unvalidated redirect.
fn loopback_url(input: &str) -> Result<reqwest::Url, BrowserError> {
    if input.len() > 2048 || input.bytes().any(|b| b.is_ascii_control()) {
        return Err(BrowserError::UnsafeUrl);
    }
    let url = reqwest::Url::parse(input).map_err(|_| BrowserError::UnsafeUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(BrowserError::UnsafeUrl);
    }
    Ok(url)
}

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

/// Map one ChromeDriver browser-log entry to a bounded console record.
/// Chrome includes the page URL in log text; redact URL-shaped tokens before
/// the value can reach MCP output.
fn browser_log_entry(entry: &Value) -> Option<BrowserConsoleEntry> {
    let kind = entry["level"].as_str().unwrap_or("log");
    let message = entry["message"].as_str()?;
    if message.is_empty() {
        return None;
    }
    let message = message
        .split_whitespace()
        .map(|token| {
            if token.starts_with("http://")
                || token.starts_with("https://")
                || token.starts_with("ws://")
                || token.starts_with("wss://")
            {
                "[url]"
            } else {
                token
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if message.is_empty() {
        return None;
    }
    Some(BrowserConsoleEntry {
        kind: kind.chars().take(32).collect(),
        message: message.chars().take(300).collect(),
    })
}

fn validate_selector(selector: &str) -> Result<(), BrowserError> {
    if selector.trim().is_empty()
        || selector.len() > MAX_SELECTOR
        || selector.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(BrowserError::InvalidControl);
    }
    Ok(())
}

fn safe_id(id: &str) -> bool {
    (5..=48).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PagePhase {
    Ready,
    Navigation,
    Authentication,
    Verification,
    Unavailable,
}

impl PagePhase {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Navigation => "navigation",
            Self::Authentication => "authentication",
            Self::Verification => "verification",
            Self::Unavailable => "unavailable",
        }
    }
}

fn verification_title(title: &str) -> bool {
    // Some Cloudflare interstitials show no visible Turnstile element.
    let title = title.trim().to_ascii_lowercase();
    matches!(
        title.as_str(),
        "just a moment..." | "just a moment…" | "attention required! | cloudflare"
    )
}

fn page_phase(state: &Value) -> PagePhase {
    // A verification interstitial can also appear on an auth redirect.
    if state["challenge"] == true
        || (state["composer"] != true && state["title"].as_str().is_some_and(verification_title))
    {
        return PagePhase::Verification;
    }
    if state["page_kind"] == "authentication" {
        return PagePhase::Authentication;
    }
    if state["external"] == true && state["transient"] != true {
        return PagePhase::Unavailable;
    }
    if state["composer"] == true {
        // A login link can coexist with a functioning composer.
        return PagePhase::Ready;
    }
    if state["login"] == true {
        return PagePhase::Authentication;
    }
    if state["transient"] == true {
        return PagePhase::Navigation;
    }
    PagePhase::Unavailable
}

fn manual_reason(state: &Value) -> Option<&'static str> {
    match page_phase(state) {
        PagePhase::Authentication => Some("authentication"),
        PagePhase::Verification => Some("verification"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use novagpt_browser_driver::undetected::{WebDriverClient, WebDriverSession};
    use std::{sync::Arc, time::Duration};
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
        sync::Mutex,
        time::timeout,
    };

    #[test]
    fn manual_checks_are_never_bypassed() {
        assert_eq!(
            manual_reason(&json!({"challenge":true,"composer":true})),
            Some("verification")
        );
        assert_eq!(
            manual_reason(&json!({"composer":false,"login":true})),
            Some("authentication")
        );
        assert_eq!(
            manual_reason(&json!({"composer":true,"challenge":false,"login":false})),
            None
        );
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
            None
        );
        assert_eq!(
            manual_reason(&json!({"composer":true,"challenge":true,"login":false})),
            Some("verification")
        );
    }

    #[test]
    fn interstitial_authentication_and_navigation_are_distinct() {
        for title in [
            "Just a moment...",
            "Just a moment…",
            " ATTENTION REQUIRED! | CLOUDFLARE ",
        ] {
            let state = json!({"page_kind":"chatgpt", "title":title, "composer":false});
            assert_eq!(page_phase(&state), PagePhase::Verification);
            assert_eq!(manual_reason(&state), Some("verification"));
        }
        assert_eq!(
            page_phase(
                &json!({"page_kind":"chatgpt", "title":"Just a moment...", "composer":true})
            ),
            PagePhase::Ready,
            "a conversation title alone must not block a working composer"
        );
        assert_eq!(
            page_phase(&json!({"page_kind":"authentication", "external":true})),
            PagePhase::Authentication
        );
        assert_eq!(
            page_phase(&json!({"page_kind":"initial_blank", "external":true, "transient":true})),
            PagePhase::Navigation
        );
        assert_eq!(
            page_phase(&json!({"page_kind":"navigation", "transient":true})),
            PagePhase::Navigation
        );
        assert_eq!(
            page_phase(&json!({"page_kind":"chatgpt", "composer":false, "login":false})),
            PagePhase::Unavailable,
            "a missing composer is not proof of logout"
        );
        assert_eq!(
            page_phase(&json!({"page_kind":"external_other", "external":true})),
            PagePhase::Unavailable,
            "an unfamiliar external URL is not evidence of verification"
        );
        assert_eq!(
            page_phase(&json!({"page_kind":"chatgpt", "challenge":true, "composer":true})),
            PagePhase::Verification,
            "a visible challenge takes priority over an editor"
        );
    }

    #[test]
    fn ui_adapter_discovers_editable_textboxes_and_requires_visible_controls() {
        let expression = ui_expression("return !!findEditor() && !!findSendButton();");
        assert!(expression.contains("[role='textbox'][contenteditable='true']"));
        assert!(expression.contains("isContentEditable"));
        assert!(expression.contains("data-testid") && expression.contains("send-button"));
        assert!(expression.contains("rect.width <= 0"));
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

    fn public_browser() -> ChatGptBrowser {
        let executable = tempfile::NamedTempFile::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        ChatGptBrowser::new(executable.path(), root.path().join("profile")).unwrap()
    }

    #[test]
    fn loopback_navigation_is_available_by_default_and_other_local_hosts_are_denied() {
        let browser = public_browser();
        for allowed in [
            "https://example.org/app",
            "http://localhost:5173/",
            "http://localhost:5173/dashboard?tab=1",
            "http://127.0.0.1:3000/",
            "http://[::1]:5173/",
            "http://localhost/",
            "https://localhost:8443/",
            "https://[::1]:8443/",
        ] {
            assert!(browser.approved_url(allowed).is_ok(), "{allowed}");
        }
        for denied in [
            "http://192.168.1.5:3000/",
            "http://127.0.0.2:3000/",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1:8080/",
            "http://localhost.evil.example:5173/",
            "http://dev.localhost:5173/",
            "http://user:password@localhost:5173/",
            "http://localhost:5173/\nheader",
            "ftp://localhost:5173/",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "https://intranet/",
            "https://private.local/",
        ] {
            assert!(browser.approved_url(denied).is_err(), "{denied}");
        }
    }

    #[test]
    fn optional_dev_origin_restricts_to_exact_scheme_host_and_port() {
        let browser = public_browser();
        for invalid in [
            "http://192.168.1.5:3000",
            "http://localhost",
            "https://localhost:3000",
            "http://localhost:3000/secret",
            "http://localhost:3000/?key=secret",
            "http://user:password@localhost:3000",
            "http://localhost:3000#fragment",
        ] {
            assert!(local_dev_origin(invalid).is_err(), "{invalid}");
        }
        let browser = browser.with_dev_origin("http://127.0.0.1:3000").unwrap();
        for allowed in [
            "https://example.org/",
            "http://127.0.0.1:3000/",
            "http://127.0.0.1:3000/dashboard?tab=1",
        ] {
            assert!(browser.approved_url(allowed).is_ok(), "{allowed}");
        }
        for denied in [
            "http://127.0.0.1:3001/",
            "http://localhost:3000/",
            "https://127.0.0.1:3000/",
            "http://[::1]:3000/",
            "http://127.0.0.2:3000/",
            "http://169.254.169.254:3000/",
            "http://user:secret@127.0.0.1:3000/",
            "file:///etc/passwd",
        ] {
            assert!(browser.approved_url(denied).is_err(), "{denied}");
        }
        for invalid in ["", "   ", "\n", &"a".repeat(513)] {
            assert!(validate_selector(invalid).is_err());
        }
    }

    #[test]
    fn console_entries_are_bounded_and_redact_urls() {
        let entry = browser_log_entry(&json!({
            "level":"SEVERE",
            "message":"https://example.org/api?token=sensitive fixture failure"
        }))
        .unwrap();
        assert_eq!(entry.kind, "SEVERE");
        assert!(entry.message.contains("fixture failure"));
        assert!(!entry.message.contains("token=sensitive"));
        assert!(!entry.message.contains("https://"));
        let long = "x".repeat(1000);
        let entry = browser_log_entry(&json!({"level":"INFO","message":long})).unwrap();
        assert_eq!(entry.message.len(), 300);
        assert!(browser_log_entry(&json!({"level":"INFO"})).is_none());
        assert!(browser_log_entry(&json!({"level":"INFO","message":""})).is_none());
    }

    #[test]
    fn id_validation_is_safe() {
        assert!(safe_id("agent-abcdef01"));
        assert!(!safe_id("../secret"));
    }

    #[derive(Default)]
    struct MockState {
        handles: Vec<String>,
        current: String,
        next_handle: usize,
        polls: usize,
        initial_blank: usize,
        verified: bool,
        reply_ready: bool,
        inserted: usize,
        inserted_text: String,
        keys: usize,
        shift_enters: usize,
        not_visible: bool,
        not_focused: bool,
        obscured: bool,
        pressed: bool,
        released: bool,
        clicks: usize,
        url: String,
        logs: Vec<Value>,
        cdp: Vec<String>,
    }

    async fn spawn_mock() -> (u16, Arc<Mutex<MockState>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(MockState {
            handles: vec!["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()],
            current: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            url: "about:blank".into(),
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
                        let (status, value) = route(&method, &path, &body, &state).await;
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
        let session = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        if method == "POST" && path == "/session" {
            return (
                200,
                json!({"value":{"sessionId":session,"capabilities":{}}}),
            );
        }
        if method == "GET" && path.ends_with("/window/handles") {
            return (200, json!({"value":state.handles.clone()}));
        }
        if method == "POST" && path.ends_with("/window/new") {
            state.next_handle += 1;
            let handle = format!("{:032x}", state.next_handle + 0xaa);
            state.handles.push(handle.clone());
            state.current = handle;
            return (200, json!({"value":{"handle":state.current,"type":"tab"}}));
        }
        if method == "POST" && path.ends_with("/window") {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let Some(handle) = parsed["handle"].as_str() else {
                return (400, json!({"value":{"error":"invalid argument"}}));
            };
            if !state.handles.iter().any(|known| known == handle) {
                return (404, json!({"value":{"error":"no such window"}}));
            }
            state.current = handle.to_owned();
            return (200, json!({"value":null}));
        }
        if method == "GET" && path.ends_with("/window") {
            return (200, json!({"value":state.current}));
        }
        if method == "GET" && path.ends_with("/url") {
            return (200, json!({"value":state.url}));
        }
        if method == "POST" && path.ends_with("/url") {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            state.url = parsed["url"].as_str().unwrap_or("about:blank").to_owned();
            return (200, json!({"value":null}));
        }
        if method == "POST" && path.ends_with("/timeouts") {
            return (200, json!({"value":null}));
        }
        if method == "POST" && path.ends_with("/execute/sync") {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let script = parsed["script"].as_str().unwrap_or("");
            let value = if script.contains("assistant_count:assistants.length") {
                state.polls += 1;
                if state.polls <= state.initial_blank {
                    json!({"external":true,"transient":true,"page_kind":"initial_blank"})
                } else if !state.verified {
                    json!({"page_kind":"chatgpt","title":"Just a moment...",
                        "composer":false,"challenge":false,"login":false})
                } else if state.reply_ready {
                    json!({"url":"https://chatgpt.com/c/mock","page_kind":"chatgpt",
                        "composer":true,"challenge":false,"login":false,
                        "assistant_count":1,"last_text":"Fixture answer","complete":true})
                } else {
                    json!({"url":"https://chatgpt.com/","page_kind":"chatgpt",
                        "composer":true,"challenge":false,"login":false,
                        "assistant_count":0,"last_text":"","complete":true})
                }
            } else if script.contains("document.activeElement === el") {
                json!(true)
            } else if script.contains("return editorText(findEditor())") {
                json!(state.inserted_text)
            } else if script.contains("elementFromPoint") {
                json!({"x":100.0,"y":40.0,"clear":!state.obscured})
            } else if script.contains("document.visibilityState") {
                json!({"visible":!state.not_visible,"focused":!state.not_focused})
            } else if script.contains("ready:document.readyState") {
                json!({"url":state.url,"ready":"complete"})
            } else if script.contains("document.body?.innerText") {
                json!({"url":"http://localhost:5173/","title":"Ace Blocker",
                    "text":"Configure once. Hold the line."})
            } else if script.contains("broken_images:") {
                json!({"url":"http://localhost:5173/","title":"Fixture",
                    "viewport_width":800,"viewport_height":600,
                    "document_width":801,"broken_images":1,
                    "headings":["Fixture heading"],
                    "controls":[{"tag":"button","label":"Save","selector":"#save"}]})
            } else if script.contains("url:location.href,width:innerWidth") {
                json!({"url":state.url,"width":800,"height":600})
            } else if script.contains("location.href") {
                json!(state.url)
            } else {
                Value::Null
            };
            return (200, json!({"value":value}));
        }
        if method == "POST" && path.ends_with("/goog/cdp/execute") {
            let parsed: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let command = parsed["cmd"].as_str().unwrap_or("");
            state.cdp.push(command.to_owned());
            let params = &parsed["params"];
            return match command {
                "Input.insertText" => {
                    state.inserted += 1;
                    state.inserted_text = params["text"].as_str().unwrap_or("").to_owned();
                    (200, json!({"value":{}}))
                }
                "Input.dispatchMouseEvent" => {
                    if params["type"] == "mousePressed" {
                        state.pressed = true;
                    } else if params["type"] == "mouseReleased" {
                        state.released = state.pressed;
                        state.reply_ready = state.released;
                    }
                    state.clicks += 1;
                    (200, json!({"value":{}}))
                }
                "Input.dispatchKeyEvent" => {
                    if params["type"] == "keyDown" {
                        state.keys += 1;
                        if params["key"] == "Enter" {
                            if params["modifiers"].as_u64() == Some(8) {
                                state.shift_enters += 1;
                                state.inserted_text.push('\n');
                            }
                        } else if let Some(text) = params["text"].as_str() {
                            state.inserted_text.push_str(text);
                        }
                    }
                    (200, json!({"value":{}}))
                }
                "Page.navigate" => {
                    state.url = params["url"].as_str().unwrap_or("about:blank").to_owned();
                    (200, json!({"value":{}}))
                }
                "Page.captureScreenshot" => (
                    200,
                    json!({"value":{"data":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4//8/AwAI/AL+XvCuXwAAAABJRU5ErkJggg=="}}),
                ),
                "Page.enable"
                | "Page.bringToFront"
                | "Log.enable"
                | "Network.enable"
                | "Emulation.setDeviceMetricsOverride" => (200, json!({"value":{}})),
                "Runtime.evaluate" => (200, json!({"value":{"result":{"value":Value::Null}}})),
                _ => (404, json!({"value":{"error":"unknown command"}})),
            };
        }
        if method == "POST" && path.ends_with("/se/log") {
            return (200, json!({"value":state.logs.clone()}));
        }
        if method == "GET" && path.ends_with("/screenshot") {
            return (
                200,
                json!({"value":"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4//8/AwAI/AL+XvCuXwAAAABJRU5ErkJggg=="}),
            );
        }
        if method == "DELETE" && path.ends_with("/window") {
            let current = state.current.clone();
            state.handles.retain(|handle| handle != &current);
            return (200, json!({"value":null}));
        }
        if method == "DELETE" && path.starts_with("/session/") {
            return (200, json!({"value":null}));
        }
        (404, json!({"value":{"error":"unknown command"}}))
    }

    async fn test_session(port: u16) -> WebDriverSession {
        let client = WebDriverClient::new(port).expect("client");
        WebDriverSession::create(client, json!({"browserName":"chrome"}))
            .await
            .expect("session")
    }

    #[tokio::test]
    async fn subagent_send_uses_native_state_and_cdp_input() {
        let (port, state, server) = spawn_mock().await;
        {
            let mut state = state.lock().await;
            state.initial_blank = 2;
            state.verified = true;
        }
        let browser = ChatGptBrowser::with_test_session(test_session(port).await);
        browser.open("agent-test123").await.expect("open");
        let reply = timeout(
            Duration::from_secs(8),
            browser.send("agent-test123", "Fixture prompt"),
        )
        .await
        .expect("send timeout")
        .expect("send result");
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
                .any(|e| e == "composer_ready")
        );
        assert!(
            diagnostics
                .recent_events
                .iter()
                .any(|e| e == "text_verified")
        );
        assert!(
            diagnostics
                .recent_events
                .iter()
                .any(|e| e == "tab_interactive")
        );
        assert!(
            diagnostics
                .recent_events
                .iter()
                .any(|e| e == "text_input_typed")
        );
        assert!(
            diagnostics
                .recent_events
                .iter()
                .any(|e| e == "send_mouse_dispatched")
        );
        let serialized = serde_json::to_string(&diagnostics).expect("json");
        assert!(!serialized.contains("Fixture prompt"));
        assert!(!serialized.contains("Fixture answer"));
        assert!(!serialized.contains("https://chatgpt.com/c/mock"));
        let state = state.lock().await;
        assert_eq!(state.inserted, 0, "short ASCII prompts are typed");
        assert!(
            state.keys >= 14,
            "one key-down per character: {}",
            state.keys
        );
        assert!(state.released);
        assert!(!state.cdp.iter().any(|cmd| cmd == "Input.insertText"));
        assert!(state.cdp.iter().any(|cmd| cmd == "Input.dispatchKeyEvent"));
        server.abort();
    }

    #[tokio::test]
    async fn interstitial_waits_for_manual_verification_then_resumes_same_unsent_prompt() {
        let (port, state, server) = spawn_mock().await;
        let browser = ChatGptBrowser::with_test_session(test_session(port).await);
        browser.open("agent-verified").await.expect("open");
        let first = timeout(
            Duration::from_secs(3),
            browser.send("agent-verified", "Retained fixture prompt"),
        )
        .await
        .expect("first probe")
        .expect_err("manual verification");
        assert!(matches!(first, BrowserError::Manual("verification")));
        assert_eq!(
            browser
                .diagnostics(Some("agent-verified"))
                .await
                .manual_check,
            Some("verification".to_owned())
        );
        {
            let mut state = state.lock().await;
            state.verified = true;
            state.inserted = 0;
        }
        let reply = timeout(
            Duration::from_secs(6),
            browser.send("agent-verified", "Retained fixture prompt"),
        )
        .await
        .expect("resume")
        .expect("reply");
        assert_eq!(reply.content, "Fixture answer");
        assert_eq!(
            state.lock().await.inserted_text,
            "Retained fixture prompt",
            "the prompt is typed exactly once after verification"
        );
        assert!(
            browser
                .diagnostics(Some("agent-verified"))
                .await
                .manual_check
                .is_none()
        );
        server.abort();
    }

    #[tokio::test]
    async fn a_stale_submitted_tab_cannot_silently_replay_a_prompt() {
        let (port, state, server) = spawn_mock().await;
        let session = test_session(port).await;
        let tab = session.first_tab().await.expect("tab");
        let browser = ChatGptBrowser::with_test_session(session.clone());
        browser.inner.pages.lock().await.insert(
            "agent-crashed".into(),
            Arc::new(Page {
                transport: tab,
                background: None,
                pacing: InteractionPacing::instant(),
                cancelled: AtomicBool::new(false),
                submission_attempted: AtomicBool::new(true),
                send_lock: Mutex::new(()),
                pending_before: Mutex::new(Some(0)),
                manual: Mutex::new(None),
                recent_events: Mutex::new(VecDeque::new()),
                previous_state: Mutex::new(None),
                console: Mutex::new(VecDeque::new()),
                console_seen: Mutex::new(VecDeque::new()),
                reporter: None,
            }),
        );
        session.close().await.expect("close session");
        assert!(matches!(
            browser.send("agent-crashed", "Never replay").await,
            Err(BrowserError::AmbiguousSubmission)
        ));
        let diagnostic = browser.diagnostics(Some("agent-crashed")).await;
        assert!(
            diagnostic
                .recent_events
                .iter()
                .any(|event| event == "recovery_blocked_after_send")
        );
        assert_eq!(diagnostic.page_count, 1);
        assert_eq!(state.lock().await.inserted, 0);
        server.abort();
    }

    #[tokio::test]
    async fn send_aborts_without_input_when_the_tab_cannot_be_focused() {
        let (port, state, server) = spawn_mock().await;
        {
            let mut state = state.lock().await;
            state.verified = true;
            state.not_focused = true;
        }
        let browser = ChatGptBrowser::with_test_session(test_session(port).await);
        browser.open("agent-hidden").await.expect("open");
        let result = timeout(
            Duration::from_secs(4),
            browser.send("agent-hidden", "Must not be typed"),
        )
        .await
        .expect("no hang")
        .expect_err("unfocused tab");
        assert!(matches!(result, BrowserError::InteractionBlocked));
        let state = state.lock().await;
        assert_eq!(state.keys, 0, "no keystroke may be dispatched");
        assert_eq!(state.inserted, 0, "no text insertion may be dispatched");
        assert!(!state.pressed, "no submit click may be dispatched");
        assert!(!state.cdp.iter().any(|cmd| cmd == "Input.insertText"));
        assert!(!state.cdp.iter().any(|cmd| cmd == "Input.dispatchKeyEvent"));
        server.abort();
    }

    #[tokio::test]
    async fn newlines_are_typed_as_shift_enter_without_early_submission() {
        let (port, state, server) = spawn_mock().await;
        {
            let mut state = state.lock().await;
            state.verified = true;
        }
        let browser = ChatGptBrowser::with_test_session(test_session(port).await);
        browser.open("agent-newline").await.expect("open");
        let reply = timeout(
            Duration::from_secs(8),
            browser.send("agent-newline", "first\nsecond"),
        )
        .await
        .expect("send timeout")
        .expect("reply");
        assert_eq!(reply.content, "Fixture answer");
        let state = state.lock().await;
        assert_eq!(state.shift_enters, 1);
        assert_eq!(state.inserted, 0);
        assert_eq!(state.inserted_text, "first\nsecond");
        assert!(state.released, "the only submission is the send click");
        server.abort();
    }

    #[tokio::test]
    async fn an_obscured_send_button_is_not_clicked_blindly() {
        let (port, state, server) = spawn_mock().await;
        {
            let mut state = state.lock().await;
            state.verified = true;
            state.obscured = true;
        }
        let browser = ChatGptBrowser::with_test_session(test_session(port).await);
        browser.open("agent-covered").await.expect("open");
        let result = timeout(
            Duration::from_secs(8),
            browser.send("agent-covered", "Do not click an overlay"),
        )
        .await
        .expect("no hang")
        .expect_err("obscured button");
        assert!(matches!(result, BrowserError::InteractionBlocked));
        let diagnostics = browser.diagnostics(Some("agent-covered")).await;
        assert!(
            diagnostics
                .recent_events
                .iter()
                .any(|event| event == "send_button_obscured")
        );
        let state = state.lock().await;
        assert!(!state.pressed, "a covered button must not be clicked");
        assert_eq!(state.inserted, 0);
        server.abort();
    }

    #[tokio::test]
    async fn browsing_tools_use_webdriver_cdp_and_return_viewable_image() {
        let (port, state, server) = spawn_mock().await;
        state.lock().await.logs = vec![json!({
            "level":"WARNING",
            "message":"https://secret.example/path fixture warning"
        })];
        let browser = ChatGptBrowser::with_test_session(test_session(port).await);
        browser.browse_open().await.expect("browse open");
        let page = browser
            .browse_navigate("http://localhost:5173/")
            .await
            .expect("navigate");
        assert_eq!(page.title, "Ace Blocker");
        assert!(page.text.contains("Configure once."));
        assert!(matches!(
            browser.browse_viewport(100, 600, 1.0).await,
            Err(BrowserError::InvalidControl)
        ));
        assert!(matches!(
            browser.browse_viewport(2560, 1800, 3.0).await,
            Err(BrowserError::InvalidControl)
        ));
        assert!(matches!(
            browser.browse_viewport(800, 600, f64::NAN).await,
            Err(BrowserError::InvalidControl)
        ));
        assert_eq!(
            browser
                .browse_viewport(800, 600, 2.0)
                .await
                .expect("viewport")
                .width,
            800
        );
        let inspection = browser.browse_inspect().await.expect("inspect");
        assert_eq!(inspection.broken_images, 1);
        assert_eq!(inspection.controls[0].selector.as_deref(), Some("#save"));
        assert_eq!(inspection.document_width, 801);
        assert!(browser.browse_click("#save").await.expect("click"));
        assert!(
            browser
                .browse_type("#search", "fixture text")
                .await
                .expect("type")
        );
        let screenshot = browser.browse_screenshot().await.expect("screenshot");
        assert!(screenshot.png_base64.starts_with("iVBORw0KGgo"));
        assert_eq!(screenshot.width, 800);
        assert_eq!(screenshot.url, "http://localhost:5173/");
        let console = browser.browse_console().await.expect("console");
        assert!(
            console
                .entries
                .iter()
                .any(|entry| entry.message.contains("fixture warning"))
        );
        assert!(
            console
                .entries
                .iter()
                .all(|entry| !entry.message.contains("secret.example"))
        );
        browser.browse_close().await.expect("browse close");
        let state = state.lock().await;
        assert_eq!(state.inserted, 1);
        assert!(state.released);
        assert!(state.cdp.iter().any(|cmd| cmd == "Page.captureScreenshot"));
        assert!(
            state
                .cdp
                .iter()
                .any(|cmd| cmd == "Emulation.setDeviceMetricsOverride")
        );
        server.abort();
    }
}
