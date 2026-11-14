//! Discover or create the dedicated ChatGPT project through the visible browser.
//! Only the user-authenticated WebDriver tab interacts with ChatGPT. The cache
//! stores a validated project URL, never credentials or conversation content.
use crate::{BrowserError, project};
use novagpt_browser_driver::undetected::WebDriverTab;
use serde_json::{Value, json};
use std::path::Path;
use tokio::{
    fs,
    io::AsyncWriteExt,
    time::{Duration, sleep},
};

pub(crate) const NAME: &str = "NovaGPT - subagents";
const HOME: &str = "https://chatgpt.com/";
const CACHE: &str = "novagpt-subagent-project.json";
const MAX_CACHE: u64 = 4096;

// The evaluated script only looks at visible controls in the authenticated
// page. It marks one control for a subsequent real WebDriver click/keystroke;
// it does not submit text, extract account data or call private web endpoints.
const PROJECT_UI: &str = r#"
const action = arguments[0], name = arguments[1];
const marker = 'data-novagpt-subagent-project-target';
document.querySelectorAll('[' + marker + ']').forEach(e => e.removeAttribute(marker));
const visible = e => {
    if (!e || !e.isConnected) return false;
    const r = e.getBoundingClientRect(), s = getComputedStyle(e);
    return r.width > 0 && r.height > 0 && s.display !== 'none'
        && s.visibility !== 'hidden' && s.visibility !== 'collapse';
};
const label = e => (e.getAttribute('aria-label') || e.getAttribute('title')
    || e.innerText || e.textContent || '').trim();
const controls = () => [...document.querySelectorAll('button, [role="button"], a')]
    .filter(visible);
const mark = (e, kind, href) => {
    e.setAttribute(marker, '1');
    return {kind, ...(href ? {href} : {})};
};
const title = (document.title || '').toLowerCase();
const body = (document.body?.innerText || '').slice(0, 2500);
if (/just a moment|un momento|attention required|cloudflare/i.test(title)
    || /verify you are human|checking your browser|verifica che sei umano/i.test(body))
    return {kind: 'verification'};
if (controls().some(e => /^(log in|sign in|accedi|iniciar sesi[oó]n)$/i.test(label(e)))
    && !document.querySelector('#prompt-textarea, [data-testid="prompt-textarea"]'))
    return {kind: 'authentication'};
if (action === 'verify') {
    const matchesHeading = [...document.querySelectorAll('h1, h2, h3, [role="heading"]')]
        .some(e => visible(e) && label(e) === name);
    const matchesSidebar = [...document.querySelectorAll('aside *, nav *, [role="navigation"] *')]
        .some(e => visible(e) && label(e) === name && e.children.length < 3);
    return {kind: matchesHeading || matchesSidebar ? 'verified' : 'missing'};
}
if (action === 'existing') {
    const link = controls().find(e => e.tagName === 'A'
        && label(e) === name && (e.getAttribute('href') || '').includes('/g/g-p-'));
    if (link) return {kind: 'link', href: link.href};
    const newChatLabels = ['New chat in ' + name, 'Nuova chat in ' + name];
    const newChat = controls().find(e => newChatLabels.includes(label(e)));
    if (newChat) return mark(newChat, 'new-chat');
    const entry = [...document.querySelectorAll('aside div, nav div, [role="navigation"] div')]
        .find(e => visible(e) && e.children.length < 3 && label(e) === name);
    if (entry) return mark(entry, 'entry');
    return {kind: 'missing'};
}
if (action === 'new-project' || action === 'projects-header') {
    const names = action === 'new-project'
        ? /^(new project|add new project|nuovo progetto|aggiungi nuovo progetto)$/i
        : /^(projects|progetti)$/i;
    const button = controls().find(e => names.test(label(e)));
    return button ? mark(button, 'control') : {kind: 'missing'};
}
const dialog = [...document.querySelectorAll('[role="dialog"], dialog')]
    .find(visible);
if (!dialog) return {kind: 'missing'};
if (action === 'name-input') {
    const input = [...dialog.querySelectorAll('input:not([type="hidden"]), textarea, [role="textbox"]')]
        .find(e => visible(e) && !e.disabled && !e.readOnly);
    if (!input || (input.value || '').trim()) return {kind: 'missing'};
    return mark(input, 'control');
}
if (action === 'create-project') {
    const button = [...dialog.querySelectorAll('button, [role="button"]')]
        .find(e => visible(e) && !e.disabled
            && /^(create|create project|crea|crea progetto)$/i.test(label(e)));
    return button ? mark(button, 'control') : {kind: 'missing'};
}
return {kind: 'missing'};
"#;

fn kind(result: &Value) -> &str {
    result
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("missing")
}

async fn inspect(tab: &WebDriverTab, action: &str) -> Result<Value, BrowserError> {
    tab.execute(PROJECT_UI, json!([action, NAME]))
        .await
        .map_err(BrowserError::from)
}

fn check_manual(value: &Value) -> Result<(), BrowserError> {
    match kind(value) {
        "verification" => Err(BrowserError::Manual("verification")),
        "authentication" => Err(BrowserError::Manual("authentication")),
        _ => Ok(()),
    }
}

async fn click_marked(tab: &WebDriverTab) -> Result<(), BrowserError> {
    tab.find("[data-novagpt-subagent-project-target='1']")
        .await
        .map_err(|_| BrowserError::ProjectSetup)?
        .click()
        .await
        .map_err(|_| BrowserError::ProjectSetup)
}

async fn press_marked_button(tab: &WebDriverTab) -> Result<(), BrowserError> {
    // The sidebar Add new project button can be hover-only and overlap the
    // content pane. Keyboard activation avoids an intercepted mouse click.
    tab.find("button[data-novagpt-subagent-project-target='1']")
        .await
        .map_err(|_| BrowserError::ProjectSetup)?
        .send_keys("\u{E007}")
        .await
        .map_err(|_| BrowserError::ProjectSetup)
}

async fn verified(tab: &WebDriverTab, expected: &str) -> Result<bool, BrowserError> {
    for _ in 0..16 {
        let current = tab.current_url().await.map_err(BrowserError::from)?;
        let result = inspect(tab, "verify").await?;
        check_manual(&result)?;
        if project::project_url(&current).as_deref() == Some(expected)
            && kind(&result) == "verified"
        {
            return Ok(true);
        }
        sleep(Duration::from_millis(250)).await;
    }
    Ok(false)
}

async fn opened_project(tab: &WebDriverTab) -> Result<Option<String>, BrowserError> {
    for _ in 0..16 {
        let current = tab.current_url().await.map_err(BrowserError::from)?;
        if let Some(url) = project::project_url(&current)
            && verified(tab, &url).await?
        {
            return Ok(Some(url));
        }
        let result = inspect(tab, "verify").await?;
        check_manual(&result)?;
        sleep(Duration::from_millis(250)).await;
    }
    Ok(None)
}

pub(crate) async fn cached(profile: &Path) -> Option<String> {
    let path = profile.join(CACHE);
    let info = fs::symlink_metadata(&path).await.ok()?;
    if !info.is_file() || info.file_type().is_symlink() || info.len() > MAX_CACHE {
        return None;
    }
    let bytes = fs::read(&path).await.ok()?;
    if bytes.len() as u64 > MAX_CACHE {
        return None;
    }
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    if value["version"] != 1 || value["name"] != NAME {
        return None;
    }
    project::project_url(value["url"].as_str()?)
}

async fn save(profile: &Path, url: &str) {
    // Replace the cache atomically without following a pre-existing symlink.
    // The file is auxiliary: a write failure never relaxes the project guard.
    let destination = profile.join(CACHE);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temporary = profile.join(format!(".{CACHE}.{}-{nonce}.tmp", std::process::id()));
    let data = json!({"version":1,"name":NAME,"url":url}).to_string();
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let result = async {
        let mut file = options.open(&temporary).await?;
        file.write_all(data.as_bytes()).await?;
        file.sync_all().await?;
        drop(file);
        fs::rename(&temporary, &destination).await
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_file(&temporary).await;
        tracing::debug!("subagent project URL cache could not be updated");
    }
}

pub(crate) async fn ensure(tab: &WebDriverTab, profile: &Path) -> Result<String, BrowserError> {
    if let Some(url) = cached(profile).await {
        // The worker tab may already be loading the cached project; do not
        // navigate again if it is on that URL.
        if tab.current_url().await.map_err(BrowserError::from)? != url {
            tab.goto(&url).await.map_err(BrowserError::from)?;
        }
        if verified(tab, &url).await? {
            return Ok(url);
        }
    }
    if tab.current_url().await.map_err(BrowserError::from)? != HOME {
        tab.goto(HOME).await.map_err(BrowserError::from)?;
    }
    // Wait for client-side hydration and check existing projects first.
    for _ in 0..8 {
        let existing = inspect(tab, "existing").await?;
        check_manual(&existing)?;
        match kind(&existing) {
            "link" => {
                let url = existing
                    .get("href")
                    .and_then(Value::as_str)
                    .and_then(project::project_url)
                    .ok_or(BrowserError::ProjectSetup)?;
                tab.goto(&url).await.map_err(BrowserError::from)?;
                if !verified(tab, &url).await? {
                    return Err(BrowserError::ProjectSetup);
                }
                save(profile, &url).await;
                return Ok(url);
            }
            "new-chat" | "entry" => {
                click_marked(tab).await?;
                if let Some(url) = opened_project(tab).await? {
                    save(profile, &url).await;
                    return Ok(url);
                }
                // An entry can expand on click. The next pass can find its
                // newly visible "New chat in ..." button.
            }
            _ => {}
        }
        sleep(Duration::from_millis(300)).await;
    }
    let mut start = inspect(tab, "new-project").await?;
    check_manual(&start)?;
    if kind(&start) != "control" {
        let header = inspect(tab, "projects-header").await?;
        check_manual(&header)?;
        if kind(&header) == "control" {
            click_marked(tab).await?;
        }
        start = inspect(tab, "new-project").await?;
        check_manual(&start)?;
    }
    if kind(&start) != "control" {
        return Err(BrowserError::ProjectSetup);
    }
    press_marked_button(tab).await?;
    let mut input_ready = false;
    for _ in 0..20 {
        let input = inspect(tab, "name-input").await?;
        check_manual(&input)?;
        if kind(&input) == "control" {
            input_ready = true;
            break;
        }
        sleep(Duration::from_millis(250)).await;
    }
    if !input_ready {
        return Err(BrowserError::ProjectSetup);
    }
    let field = tab
        .find("[data-novagpt-subagent-project-target='1']")
        .await
        .map_err(|_| BrowserError::ProjectSetup)?;
    field
        .click()
        .await
        .map_err(|_| BrowserError::ProjectSetup)?;
    field
        .send_keys(NAME)
        .await
        .map_err(|_| BrowserError::ProjectSetup)?;
    let finish = inspect(tab, "create-project").await?;
    check_manual(&finish)?;
    if kind(&finish) != "control" {
        return Err(BrowserError::ProjectSetup);
    }
    click_marked(tab).await?;
    let url = opened_project(tab)
        .await?
        .ok_or(BrowserError::ProjectSetup)?;
    save(profile, &url).await;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_name_and_ui_actions_are_narrowly_scoped() {
        assert_eq!(NAME, "NovaGPT - subagents");
        assert!(PROJECT_UI.contains("New chat in "));
        assert!(PROJECT_UI.contains("add new project"));
        assert!(PROJECT_UI.contains("document.querySelectorAll"));
        assert!(!PROJECT_UI.contains("fetch("));
    }

    #[tokio::test]
    async fn cache_rejects_stale_names_and_external_urls() {
        let tmp = tempfile::tempdir().expect("temporary directory");
        let path = tmp.path().join(CACHE);
        fs::write(
            &path,
            json!({"version":1,"name":NAME,"url":"https://evil.test/"}).to_string(),
        )
        .await
        .expect("write");
        assert!(cached(tmp.path()).await.is_none());
        fs::write(&path, json!({"version":1,"name":"CodexPro - Subagenti","url":"https://chatgpt.com/g/g-p-abc/project"}).to_string())
            .await.expect("write");
        assert!(cached(tmp.path()).await.is_none());
        let valid = "https://chatgpt.com/g/g-p-abc/project";
        save(tmp.path(), valid).await;
        assert_eq!(cached(tmp.path()).await.as_deref(), Some(valid));
    }
}
