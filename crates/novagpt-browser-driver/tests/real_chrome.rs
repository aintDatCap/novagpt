//! Opt-in real-Chrome regression. Launches only an owned temporary profile.
//! Run: cargo test -p novagpt-browser-driver --features test-support --test real_chrome -- --ignored
//! No credentials, cookies, account data or remote service login are accessed.
#![cfg(feature = "test-support")]

use futures_util::future::join_all;
use novagpt_browser_driver::{
    BrowserDriver, DriverConfig, DriverError, WaitState, discover_chrome,
};
use std::{collections::HashSet, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};

const HTML: &str = r#"<!DOCTYPE html><html><head><title>NovaGPT browser fixture</title></head>
<body><h1 id="title">Browser fixture</h1>
<input id="entry" type="text" aria-label="task">
<button id="apply" onclick="document.querySelector('#output').textContent =
  document.querySelector('#entry').value">Apply</button>
<p id="output">Unchanged</p></body></html>"#;

async fn local_fixture() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind disposable HTTP fixture");
    let port = listener.local_addr().expect("fixture addr").port();
    let task = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = timeout(Duration::from_secs(5), socket.read(&mut request)).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{HTML}",
                    HTML.len(),
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (format!("http://127.0.0.1:{port}/"), task)
}

#[tokio::test]
#[ignore = "requires a visible desktop session and installed Google Chrome"]
async fn real_chrome_persistent_profile_pages_input_screenshot_restart_and_contention() {
    let chrome = discover_chrome(None).expect("real Chrome executable required");
    let profile_root = tempfile::tempdir().expect("disposable profile parent");
    let profile = profile_root.path().join("owned-browser");
    let driver = BrowserDriver::new(DriverConfig::new(&chrome, &profile).expect("config"))
        .expect("browser owner");
    let (fixture_url, fixture_task) = local_fixture().await;

    let result = async {
        let port = driver
            .ensure_started()
            .await
            .expect("visible Chrome launch");
        assert!(port != 0);
        assert_eq!(driver.status().await, "attached");

        // A second process/owner may not attach to an occupied profile.
        let other =
            BrowserDriver::new(DriverConfig::new(&chrome, &profile).expect("second config"))
                .expect("second owner");
        assert!(matches!(
            other.ensure_started().await,
            Err(DriverError::ProfileBusy)
        ));
        assert_eq!(driver.status().await, "attached");

        let pages = join_all((0..4).map(|_| driver.new_page())).await;
        let mut pages = pages
            .into_iter()
            .map(|r| r.expect("new page"))
            .collect::<Vec<_>>();
        assert_eq!(
            pages
                .iter()
                .map(|p| p.target_id())
                .collect::<HashSet<_>>()
                .len(),
            4
        );

        // Each tab must retain its own DOM and response irrespective of the
        // order in which Tokio schedules the tasks.
        let tasks = pages.iter().cloned().enumerate().map(|(i, page)| {
            let url = fixture_url.clone();
            async move {
                page.goto(&url, Duration::from_secs(20))
                    .await
                    .expect("fixture navigation");
                page.locator("#title")
                    .expect("title")
                    .wait_for(WaitState::Visible, Duration::from_secs(5))
                    .await
                    .expect("visible HTML");
                page.locator("#entry")
                    .expect("input")
                    .fill(&format!("worker-{i}"))
                    .await
                    .expect("CDP insertText");
                assert_eq!(
                    page.evaluate_trusted("document.querySelector('#entry').value")
                        .await
                        .expect("inserted value"),
                    format!("worker-{i}")
                );
                page.locator("#apply")
                    .expect("button")
                    .click()
                    .await
                    .expect("CDP click");
                let expected = format!("worker-{i}");
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                let actual = loop {
                    let actual = page
                        .evaluate_trusted("document.querySelector('#output').textContent")
                        .await
                        .expect("DOM text read");
                    if actual == expected || tokio::time::Instant::now() >= deadline {
                        break actual;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                };
                assert_eq!(actual, expected);
                let image = page.screenshot().await.expect("Chrome screenshot");
                assert!(image.starts_with("iVBORw0KGgo"), "real PNG prefix");
                (i, actual)
            }
        });
        let outputs = timeout(Duration::from_secs(90), join_all(tasks))
            .await
            .expect("all four workers complete");
        assert_eq!(outputs.len(), 4);
        for (i, reply) in outputs {
            assert_eq!(reply, format!("worker-{i}"));
        }

        // Closing a page must not close other tasks or the shared browser.
        let closed = pages.remove(0);
        driver
            .close_target(closed.target_id())
            .await
            .expect("user-closed tab");
        closed.close().await.expect("idempotent already-closed tab");
        closed
            .close()
            .await
            .expect("second close does not touch Chrome");
        assert!(!pages[0].is_stale());
        assert_eq!(
            pages[0]
                .evaluate_trusted("document.querySelector('#output').textContent")
                .await
                .expect("other tab alive"),
            "worker-1"
        );

        driver
            .simulate_owned_chrome_crash()
            .await
            .expect("kill only driver-owned child");
        driver
            .ensure_started()
            .await
            .expect("restart after owned Chrome crash");
        assert!(
            pages.iter().all(|page| page.is_stale()),
            "old CDP pages must be invalidated"
        );
        let reopened = driver.new_page().await.expect("fresh page");
        reopened
            .goto(&fixture_url, Duration::from_secs(20))
            .await
            .expect("post-crash navigation");
        assert_eq!(
            reopened
                .evaluate_trusted("document.querySelector('#title').textContent")
                .await
                .expect("recovered DOM"),
            "Browser fixture"
        );
        reopened.close().await.expect("close recovered tab");
    }
    .await;
    driver.shutdown().await;
    fixture_task.abort();
    result
}
