//! Opt-in real Chrome + managed ChromeDriver integration.
//!
//! Run:
//! `NOVAGPT_REAL_CHROMEDRIVER=1 cargo test -p novagpt-browser-driver --features test-support --test real_chromedriver -- --ignored --nocapture`
//!
//! It downloads the matching ChromeDriver from Chrome for Testing, patches a
//! disposable copy, launches a visible Chrome with a disposable dedicated
//! profile, and uses only a loopback HTML fixture. It never opens a real
//! account, reuses a personal profile, or reads cookies.
#![cfg(feature = "test-support")]

use novagpt_browser_driver::undetected::{
    ChromeDriverStore, DriverChoice, ManagedConfig, UndetectedChrome,
};
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};

const HTML: &str = r#"<!DOCTYPE html><html><head><title>NovaGPT driver fixture</title></head>
<body><h1 id="title">Managed driver fixture</h1>
<input id="entry" type="text" aria-label="task">
<button id="apply" onclick="document.querySelector('#output').textContent =
  document.querySelector('#entry').value; document.cookie = 'fixture=persisted; path=/; Max-Age=3600';">
Apply</button>
<p id="output">Unchanged</p>
<iframe srcdoc="<p id='nested'>nested fixture</p>"></iframe>
</body></html>"#;

async fn local_fixture() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind disposable fixture");
    let port = listener.local_addr().expect("fixture addr").port();
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = timeout(Duration::from_secs(5), socket.read(&mut request)).await;
                let body = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HTML}",
                    HTML.len()
                );
                let _ = socket.write_all(body.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}/"), task)
}

#[tokio::test]
#[ignore = "requires network access, a visible desktop session and installed Google Chrome"]
async fn real_managed_chromedriver_tabs_profile_reconnect_and_screenshot() {
    let chrome = novagpt_browser_driver::discover_chrome(None).expect("real Chrome");
    let root = tempfile::tempdir().expect("disposable parent");
    let profile = root.path().join("dedicated-profile");
    let store_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/novagpt-chromedriver-store");
    let store = ChromeDriverStore::new(&store_root).expect("driver store");
    let mut config = ManagedConfig::new(
        chrome,
        profile,
        DriverChoice::Install(std::sync::Arc::new(store)),
    );
    config.command_timeout = Duration::from_secs(60);
    let driver = UndetectedChrome::launch(config.clone())
        .await
        .expect("managed Chrome launch");
    assert_eq!(driver.status().await, "attached");
    assert!(driver.driver_path().is_file());

    let (fixture_url, fixture_task) = local_fixture().await;
    let result = async {
        let first = driver.first_tab().await.expect("first tab");
        first.goto(&fixture_url).await.expect("navigate");
        assert_eq!(
            first.evaluate("1 + 1").await.expect("native execute"),
            serde_json::json!(2)
        );
        let title = first
            .cdp_execute(
                "Runtime.evaluate",
                serde_json::json!({"expression":"document.title","returnByValue":true}),
            )
            .await
            .expect("CDP passthrough");
        assert_eq!(title["result"]["value"], "NovaGPT driver fixture");
        let entry = first.find("#entry").await.expect("entry");
        entry.send_keys("managed-input").await.expect("input");
        first
            .find("#apply")
            .await
            .expect("apply")
            .click()
            .await
            .expect("click");
        let output = first.find("#output").await.expect("output");
        assert_eq!(output.text().await.expect("text"), "managed-input");
        let screenshot = first.screenshot().await.expect("screenshot");
        assert!(screenshot.starts_with(b"\x89PNG"));

        // Four isolated tabs driven concurrently. Every command is switched to
        // the caller's handle under one session lock.
        let mut tabs = Vec::new();
        for index in 0..4 {
            let tab = driver.new_tab(&fixture_url).await.expect("isolated tab");
            let input = tab.find("#entry").await.expect("tab entry");
            input
                .send_keys(&format!("worker-{index}"))
                .await
                .expect("tab input");
            tab.find("#apply")
                .await
                .expect("tab apply")
                .click()
                .await
                .expect("tab click");
            tabs.push(tab);
        }
        for (index, tab) in tabs.iter().enumerate() {
            let output = tab.find("#output").await.expect("tab output");
            assert_eq!(
                output.text().await.expect("tab text"),
                format!("worker-{index}")
            );
        }

        // Reconnect stops/restarts ChromeDriver and creates a new session
        // against the same owned Chrome and profile. Old handles are invalid;
        // browser windows and page state remain.
        let epoch = driver.reconnect().await.expect("reconnect");
        assert!(epoch >= 2);
        assert_eq!(driver.status().await, "attached");
        let session = driver.session().await.expect("new session");
        let handles = session.handles().await.expect("surviving tabs");
        assert!(handles.len() >= 5, "tabs survive driver restart");
        let restored = driver.first_tab().await.expect("restored tab");
        let url = restored.current_url().await.expect("url");
        assert!(url.starts_with("http://127.0.0.1:"), "same fixture origin");
        let cookie = restored
            .execute("return document.cookie;", serde_json::json!([]))
            .await
            .expect("cookie");
        assert!(
            cookie
                .as_str()
                .is_some_and(|value| value.contains("fixture=persisted")),
            "profile data persisted across the session restart"
        );

        // Remove only worker tabs, then prove Chrome remains usable.
        for tab in tabs {
            tab.close().await.expect("close worker tab");
        }
        driver
            .new_tab(&fixture_url)
            .await
            .expect("browser still usable");

        // A Chrome crash is recoverable: reconnect launches a new Chrome with
        // the same dedicated profile, starts a new driver service/session, and
        // never replays prior tab actions.
        driver.simulate_chrome_crash().await.expect("crash Chrome");
        assert_eq!(driver.status().await, "stopped");
        let epoch = driver
            .reconnect()
            .await
            .expect("restart after Chrome crash");
        assert!(epoch >= 2);
        let recovered = driver
            .new_tab(&fixture_url)
            .await
            .expect("tab after Chrome restart");
        assert_eq!(
            recovered
                .evaluate("document.querySelector('#title').textContent")
                .await
                .expect("recovered DOM"),
            "Managed driver fixture"
        );
        recovered.close().await.expect("close recovered tab");
    }
    .await;

    driver.shutdown().await;
    assert_eq!(driver.status().await, "stopped");

    // A separate application launch reuses the same dedicated profile. The
    // persisted cookie proves profile storage survived process restart; no
    // credential, account or personal profile is involved.
    let restarted = UndetectedChrome::launch(config)
        .await
        .expect("restart against the same owned profile");
    let tab = restarted
        .new_tab(&fixture_url)
        .await
        .expect("tab after restart");
    let cookie = tab
        .execute("return document.cookie;", serde_json::json!([]))
        .await
        .expect("cookie after restart");
    assert!(
        cookie
            .as_str()
            .is_some_and(|value| value.contains("fixture=persisted")),
        "profile data persisted across application restart"
    );
    restarted.shutdown().await;

    fixture_task.abort();
    result
}
