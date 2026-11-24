//! Browser adapter and provider contract.
use async_trait::async_trait;
use novagpt_browser::{BrowserError, ChatGptBrowser};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BackendFailure {
    #[error("Manual {0} required")]
    Manual(&'static str),
    #[error("{0}")]
    Blocked(&'static str),
    #[error("Backend unavailable")]
    Unavailable,
    /// The page may still be responding; no replay or implicit cancellation.
    #[error("Browser outcome uncertain after a possible submission")]
    Uncertain(&'static str),
}

#[async_trait]
pub trait AgentBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn model(&self) -> &'static str;
    async fn create(&self, id: &str) -> Result<(), BackendFailure>;
    async fn send(
        &self,
        id: &str,
        prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure>;
    /// Notify the manager when the initial foreground work is over. Backends
    /// without a shared interactive browser need not hold the startup gate.
    async fn send_with_submission(
        &self,
        id: &str,
        prompt: &str,
        submitted: tokio::sync::oneshot::Sender<()>,
    ) -> Result<(String, Option<String>), BackendFailure> {
        let _ = submitted.send(());
        self.send(id, prompt).await
    }
    async fn cancel(&self, id: &str);
    /// Stop awaiting an already acknowledged response without closing its tab.
    async fn complete(&self, _id: &str) {}
    async fn close(&self, _id: &str) {}
    async fn diagnostics(&self, id: &str) -> serde_json::Value;
}

pub struct ChatGptBackend {
    browser: ChatGptBrowser,
}
impl ChatGptBackend {
    pub fn new(browser: ChatGptBrowser) -> Self {
        Self { browser }
    }
}

#[async_trait]
impl AgentBackend for ChatGptBackend {
    fn name(&self) -> &'static str {
        "chatgpt-browser"
    }
    fn model(&self) -> &'static str {
        "chatgpt-web"
    }
    async fn create(&self, id: &str) -> Result<(), BackendFailure> {
        self.browser.open(id).await.map_err(map_browser)
    }
    async fn send(
        &self,
        id: &str,
        prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        let reply = self.browser.send(id, prompt).await.map_err(map_browser)?;
        Ok((reply.content, reply.conversation_url))
    }
    async fn send_with_submission(
        &self,
        id: &str,
        prompt: &str,
        submitted: tokio::sync::oneshot::Sender<()>,
    ) -> Result<(String, Option<String>), BackendFailure> {
        let reply = self
            .browser
            .send_with_submission(id, prompt, submitted)
            .await
            .map_err(map_browser)?;
        Ok((reply.content, reply.conversation_url))
    }
    async fn cancel(&self, id: &str) {
        let _ = self.browser.cancel(id).await;
    }
    async fn close(&self, id: &str) {
        let _ = self.browser.close(id).await;
    }
    async fn complete(&self, id: &str) {
        let _ = self.browser.complete(id).await;
    }
    async fn diagnostics(&self, id: &str) -> serde_json::Value {
        serde_json::to_value(self.browser.diagnostics(Some(id)).await).unwrap_or_default()
    }
}
pub(super) fn map_browser(error: BrowserError) -> BackendFailure {
    match error {
        BrowserError::Manual(kind) => BackendFailure::Manual(kind),
        BrowserError::NavigationTimeout => {
            BackendFailure::Blocked("ChatGPT navigation timed out before prompt submission")
        }
        BrowserError::ComposerUnavailable => {
            BackendFailure::Blocked("ChatGPT composer unavailable; no prompt submitted")
        }
        BrowserError::InteractionBlocked => BackendFailure::Blocked(
            "ChatGPT tab could not be made visible and focused; no prompt submitted",
        ),
        BrowserError::SendUnavailable => BackendFailure::Blocked(
            "ChatGPT send control stayed unavailable after previous submission; coding task NOT sent",
        ),
        BrowserError::AmbiguousSubmission => BackendFailure::Uncertain(
            "Chrome disconnected after possible submission; no prompt replay. Authenticated callbacks remain available",
        ),
        BrowserError::Timeout => BackendFailure::Uncertain(
            "Browser response timed out; worker may still be running. No prompt replay. Authenticated callbacks remain available",
        ),
        BrowserError::ProfileBusy => BackendFailure::Blocked("Dedicated Chrome profile is in use"),
        BrowserError::NotRunning => BackendFailure::Blocked("Dedicated Chrome is unavailable"),
        BrowserError::ProjectMismatch => BackendFailure::Blocked(
            "ChatGPT left the designated subagent project; no message was sent outside it",
        ),
        BrowserError::ProjectSetup => BackendFailure::Blocked(
            "Could not prepare the NovaGPT - subagents project; inspect the visible browser tab. No prompt was sent",
        ),
        _ => BackendFailure::Unavailable,
    }
}
