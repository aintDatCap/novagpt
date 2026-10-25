//! Explicit read-only Chrome CDP bridge for *manually opened* tabs.
//! Independent of ChatGptBrowser and its subagent-owned profile.
//! No arbitrary JS, cookies, storage, navigation, or input actions are exposed.
use futures_util::{SinkExt, StreamExt};
use reqwest::{Client, Url, redirect::Policy};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;
use thiserror::Error;
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};

const MAX_TEXT: usize = 32_000;
const MAX_TABS: usize = 32;
// Runtime.evaluate is intentionally a fixed program; never interpolate user input.
const VISIBLE_TEXT: &str = r#"(() => {
  const root = document.body;
  if (!root) return '';
  const skip = 'script,style,noscript,template,[hidden],[aria-hidden="true"],input,textarea';
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const lines = [];
  let length = 0;
  while (walker.nextNode() && length < 33000) {
    const node = walker.currentNode;
    const parent = node.parentElement;
    if (!parent || parent.closest(skip)) continue;
    const css = getComputedStyle(parent);
    if (css.display === 'none' || css.visibility === 'hidden') continue;
    if (parent.getClientRects().length === 0) continue;
    const value = (node.textContent || '').trim();
    if (!value) continue;
    const piece = value.slice(0, 33000 - length);
    lines.push(piece);
    length += piece.length + 1;
  }
  return lines.join('\n').slice(0, 32000);
})()"#;

#[derive(Debug, Error)]
pub enum BridgeError {
    #[error("Browser Bridge is disabled or the loopback CDP browser cannot be reached")]
    Unavailable,
    #[error("Only a single explicitly approved public HTTPS origin is allowed")]
    InvalidOrigin,
    #[error("Browser target is invalid or not on the approved origin")]
    NotApproved,
    #[error("Browser Bridge response was invalid")]
    Protocol,
    #[error("Browser Bridge operation timed out")]
    Timeout,
    #[error("Browser Bridge page text exceeds the allowed size")]
    TooLarge,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserTab {
    pub id: String,
    pub title: String,
    pub origin: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowserTabText {
    pub id: String,
    pub title: String,
    pub origin: String,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Deserialize)]
struct CdpTarget {
    id: String,
    #[serde(rename = "type")]
    target_type: String,
    title: String,
    url: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket: Option<String>,
}

#[derive(Clone)]
pub struct BrowserBridge {
    port: u16,
    origin: String,
    http: Client,
}

fn approved_origin(input: &str) -> Result<String, BridgeError> {
    let url = Url::parse(input).map_err(|_| BridgeError::InvalidOrigin)?;
    let host = url.host_str().ok_or(BridgeError::InvalidOrigin)?;
    if url.scheme() != "https"
        || url.username() != ""
        || url.password().is_some()
        || host.parse::<std::net::IpAddr>().is_ok()
        || host == "localhost"
        || !host.contains('.')
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(BridgeError::InvalidOrigin);
    }
    Ok(url.origin().ascii_serialization())
}

fn target_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.bytes().all(|ch| ch.is_ascii_hexdigit())
}

impl BrowserBridge {
    /// Caller must explicitly enable this adapter at server startup.
    /// The user starts their own *separate* Chrome remote-debugging instance.
    pub fn new(port: u16, origin: &str) -> Result<Self, BridgeError> {
        if port == 0 {
            return Err(BridgeError::Unavailable);
        }
        let origin = approved_origin(origin)?;
        let http = Client::builder()
            .no_proxy()
            .redirect(Policy::none())
            .timeout(Duration::from_secs(5))
            .build()
            .map_err(|_| BridgeError::Unavailable)?;
        Ok(Self { port, origin, http })
    }

    pub fn origin(&self) -> &str {
        &self.origin
    }

    async fn targets(&self) -> Result<Vec<CdpTarget>, BridgeError> {
        let url = format!("http://127.0.0.1:{}/json/list", self.port);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|_| BridgeError::Unavailable)?;
        if !response.status().is_success() {
            return Err(BridgeError::Unavailable);
        }
        if response
            .content_length()
            .is_some_and(|len| len > 128 * 1024)
        {
            return Err(BridgeError::TooLarge);
        }
        let body = response.bytes().await.map_err(|_| BridgeError::Protocol)?;
        if body.len() > 128 * 1024 {
            return Err(BridgeError::TooLarge);
        }
        serde_json::from_slice::<Vec<CdpTarget>>(&body).map_err(|_| BridgeError::Protocol)
    }

    fn approved(&self, target: &CdpTarget) -> bool {
        target.target_type == "page"
            && target_id(&target.id)
            && Url::parse(&target.url).ok().is_some_and(|url| {
                url.origin().ascii_serialization() == self.origin && url.scheme() == "https"
            })
    }

    pub async fn tabs(&self) -> Result<Vec<BrowserTab>, BridgeError> {
        let mut result = Vec::new();
        for target in self.targets().await? {
            if self.approved(&target) && result.len() < MAX_TABS {
                result.push(BrowserTab {
                    id: target.id,
                    title: target.title.chars().take(160).collect(),
                    origin: self.origin.clone(),
                });
            }
        }
        Ok(result)
    }

    pub async fn read_tab(&self, id: &str) -> Result<BrowserTabText, BridgeError> {
        if !target_id(id) {
            return Err(BridgeError::NotApproved);
        }
        let target = self
            .targets()
            .await?
            .into_iter()
            .find(|tab| tab.id == id && self.approved(tab))
            .ok_or(BridgeError::NotApproved)?;
        let ws = target.websocket.as_deref().ok_or(BridgeError::Protocol)?;
        let prefix = format!("ws://127.0.0.1:{}/devtools/page/", self.port);
        let localhost = format!("ws://localhost:{}/devtools/page/", self.port);
        if ws != format!("{prefix}{id}") && ws != format!("{localhost}{id}") {
            return Err(BridgeError::Protocol);
        }
        let (mut stream, _) = timeout(Duration::from_secs(5), connect_async(ws))
            .await
            .map_err(|_| BridgeError::Timeout)?
            .map_err(|_| BridgeError::Protocol)?;
        // Guard against a navigation to a different origin after /json/list.
        let guarded_expression = format!(
            "(() => {{ if (location.origin !== {}) return {{ denied: true }}; return {{ denied: false, text: ({VISIBLE_TEXT}) }}; }})()",
            json!(self.origin)
        );
        let command = json!({
            "id": 1,
            "method": "Runtime.evaluate",
            "params": { "expression": guarded_expression, "returnByValue": true, "awaitPromise": true }
        });
        stream
            .send(Message::Text(command.to_string().into()))
            .await
            .map_err(|_| BridgeError::Protocol)?;
        let reply = timeout(Duration::from_secs(8), async {
            while let Some(frame) = stream.next().await {
                let frame = frame.map_err(|_| BridgeError::Protocol)?;
                if let Message::Text(text) = frame {
                    let value: Value =
                        serde_json::from_str(&text).map_err(|_| BridgeError::Protocol)?;
                    if value["id"] == 1 {
                        if value.get("error").is_some()
                            || value["result"].get("exceptionDetails").is_some()
                        {
                            return Err(BridgeError::Protocol);
                        }
                        return Ok(value);
                    }
                }
            }
            Err(BridgeError::Protocol)
        })
        .await
        .map_err(|_| BridgeError::Timeout)??;
        if reply
            .pointer("/result/result/value/denied")
            .and_then(Value::as_bool)
            != Some(false)
        {
            return Err(BridgeError::NotApproved);
        }
        let text = reply
            .pointer("/result/result/value/text")
            .and_then(Value::as_str)
            .ok_or(BridgeError::Protocol)?;
        let truncated = text.chars().count() > MAX_TEXT;
        Ok(BrowserTabText {
            id: id.to_owned(),
            title: target.title.chars().take(160).collect(),
            origin: self.origin.clone(),
            text: text.chars().take(MAX_TEXT).collect(),
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approves_explicit_https_origin_only() {
        assert!(BrowserBridge::new(0, "https://www.instagram.com").is_err());
        for host in [
            "http://www.instagram.com",
            "https://localhost",
            "https://127.0.0.1",
            "file:///tmp/private",
            "https://user:password@instagram.com",
            "https://instagram.com/path",
            "https://instagram.com/?token=abc",
        ] {
            assert!(BrowserBridge::new(9222, host).is_err(), "{host}");
        }
        let bridge = BrowserBridge::new(9222, "https://www.instagram.com").expect("bridge");
        assert_eq!(bridge.origin(), "https://www.instagram.com");
        let valid = CdpTarget {
            id: "abcd12".into(),
            target_type: "page".into(),
            title: "Private".into(),
            url: "https://www.instagram.com/example/".into(),
            websocket: None,
        };
        assert!(bridge.approved(&valid));
        let different = CdpTarget {
            url: "https://accounts.google.com/".into(),
            ..valid
        };
        assert!(!bridge.approved(&different));
    }

    #[tokio::test]
    async fn local_cdp_read_is_scoped_and_does_not_expose_other_tabs() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        use tokio_tungstenite::accept_async;
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock CDP");
        let port = listener.local_addr().expect("address").port();
        let fixture = tokio::spawn(async move {
            for index in 0..4 {
                let (mut stream, _) = listener.accept().await.expect("connection");
                if index == 3 {
                    let mut socket = accept_async(stream).await.expect("CDP websocket");
                    let frame = socket
                        .next()
                        .await
                        .expect("CDP command")
                        .expect("valid WS frame");
                    let payload: Value =
                        serde_json::from_str(frame.to_text().expect("CDP text")).expect("CDP JSON");
                    assert_eq!(payload["method"], "Runtime.evaluate");
                    let expression = payload["params"]["expression"].as_str().expect("fixed JS");
                    assert!(expression.contains("location.origin"));
                    assert!(expression.contains("www.instagram.com"));
                    assert!(!expression.contains("document.cookie"));
                    socket.send(Message::Text(json!({"id":1,"result":{"result":{"value":{"denied":false,"text":"Visible post text"}}}}).to_string().into())).await.expect("CDP reply");
                    break;
                }
                let mut first = [0; 4096];
                let count = stream.read(&mut first).await.expect("request");
                let request = String::from_utf8_lossy(&first[..count]);
                assert!(
                    request.starts_with("GET /json/list"),
                    "only tab listing is allowed"
                );
                let body = serde_json::json!([
                    {"id":"abcd12","type":"page","title":"Post Instagram","url":"https://www.instagram.com/p/fixture/?sensitive=query",
                     "webSocketDebuggerUrl":format!("ws://127.0.0.1:{port}/devtools/page/abcd12")},
                    {"id":"bbcc33","type":"page","title":"Other private tab","url":"https://bank.example/",
                     "webSocketDebuggerUrl":format!("ws://127.0.0.1:{port}/devtools/page/bbcc33")}
                ]).to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("response");
            }
        });
        // This mock tests the privacy filtering even before a WebSocket is opened.
        let bridge = BrowserBridge::new(port, "https://www.instagram.com").expect("bridge");
        let tabs = bridge.tabs().await.expect("approved tabs");
        assert_eq!(tabs.len(), 1);
        assert_eq!(tabs[0].id, "abcd12");
        let encoded = serde_json::to_string(&tabs).expect("serialize");
        assert!(!encoded.contains("bank.example"));
        assert!(!encoded.contains("sensitive=query"));
        assert!(matches!(
            bridge.read_tab("bbcc33").await,
            Err(BridgeError::NotApproved)
        ));
        let read = bridge
            .read_tab("abcd12")
            .await
            .expect("read visible text over mocked CDP");
        assert_eq!(read.text, "Visible post text");
        assert_eq!(read.origin, "https://www.instagram.com");
        fixture.await.expect("mock complete");
    }

    #[tokio::test]
    async fn never_connects_to_unapproved_or_unavailable_targets() {
        let bridge = BrowserBridge::new(1, "https://www.instagram.com").expect("bridge");
        assert!(matches!(
            bridge.read_tab("../private").await,
            Err(BridgeError::NotApproved)
        ));
        assert!(matches!(bridge.tabs().await, Err(BridgeError::Unavailable)));
    }
}
