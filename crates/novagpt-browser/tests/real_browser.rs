//! Opt-in real Chrome + ChromeDriver test of the ChatGPT browser adapter's
//! browsing transport. It uses only a disposable profile and a loopback HTML
//! fixture; no ChatGPT account, credential, cookie or remote page is touched.
//!
//! Run:
//! `cargo test -p novagpt-browser --test real_browser -- --ignored --nocapture`
use novagpt_browser::ChatGptBrowser;
use novagpt_browser::managed::{ChromeDriverStore, DriverChoice};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};

const HTML: &str = r#"<!DOCTYPE html><html><head><title>Adapter fixture</title></head>
<body><h1 id="title">Adapter fixture</h1>
<input id="entry" type="text" aria-label="task">
<button id="apply" onclick="document.querySelector('#output').textContent =
  document.querySelector('#entry').value; console.log('fixture console message');">
Apply</button>
<p id="output">Unchanged</p></body></html>"#;

async fn fixture() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("fixture");
    let port = listener.local_addr().expect("addr").port();
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = timeout(Duration::from_secs(5), socket.read(&mut request)).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HTML}",
                    HTML.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}/"), task)
}

#[tokio::test]
#[ignore = "requires network access, a visible desktop session and installed Google Chrome"]
async fn real_chatgpt_browser_adapter_uses_chromedriver_browsing_transport() {
    let chrome = novagpt_browser::managed::discover_chrome(None).expect("Chrome");
    let root = tempfile::tempdir().expect("parent");
    let profile = root.path().join("dedicated-profile");
    let store_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/novagpt-chromedriver-store");
    let store = ChromeDriverStore::new(store_root).expect("driver store");
    let browser =
        ChatGptBrowser::with_driver(chrome, profile, DriverChoice::Install(Arc::new(store)))
            .expect("browser");
    let (url, server) = fixture().await;
    let result = async {
        let diagnostics = browser.browse_open().await.expect("browse open");
        assert_eq!(diagnostics.automation_driver, "chrome-driver-webdriver");
        let page = browser.browse_navigate(&url).await.expect("navigate");
        assert_eq!(page.title, "Adapter fixture");
        assert!(
            browser
                .browse_type("#entry", "real-input")
                .await
                .expect("type")
        );
        assert!(browser.browse_click("#apply").await.expect("click"));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let page = browser.browse_read().await.expect("read");
            if page.text.contains("real-input") || tokio::time::Instant::now() >= deadline {
                assert!(page.text.contains("real-input"));
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let screenshot = browser.browse_screenshot().await.expect("screenshot");
        assert!(screenshot.png_base64.starts_with("iVBORw0KGgo"));
        assert_eq!(screenshot.url, url);
        // Console capture is best-effort across ChromeDriver versions; the
        // call must succeed and never expose a URL.
        let console = browser.browse_console().await.expect("console");
        assert!(console.entries.iter().all(|e| !e.message.contains("http")));
        browser.browse_close().await.expect("close");
    }
    .await;
    browser.shutdown().await;
    server.abort();
    result
}
