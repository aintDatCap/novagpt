//! ChatGPT attachment import with pinned approved HTTPS hosts and bounded bytes.
//! No remote-origin URLs or tokens are ever returned to MCP clients.
use crate::{NovaError, NovaResult, WorkspaceRegistry};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

const MAX_ATTACHMENT: usize = 5 * 1024 * 1024;
const ALLOWED_HOSTS: &[&str] = &[
    "files.oaiusercontent.com",
    "oaiusercontent.com",
    "chatgpt.com",
    "cdn.openai.com",
];

#[derive(Clone, Debug, Deserialize)]
pub struct AttachmentRef {
    pub download_url: String,
    pub file_id: String,
    pub mime_type: Option<String>,
    pub file_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ImportRequest {
    pub workspace_id: Option<String>,
    pub file: AttachmentRef,
    pub destination: String,
    pub overwrite: bool,
    pub expected_sha256: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ImportResult {
    pub path: String,
    pub bytes: usize,
    pub sha256: String,
    pub file_id: String,
    pub file_name: Option<String>,
    pub declared_mime_type: Option<String>,
    pub detected_mime_type: Option<String>,
    pub verified: bool,
    pub overwritten: bool,
}

fn permitted_url(input: &str) -> NovaResult<reqwest::Url> {
    let url = reqwest::Url::parse(input)
        .map_err(|_| NovaError::InvalidInput("Invalid attachment URL".into()))?;
    if url.scheme() != "https"
        || url.port_or_known_default() != Some(443)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(NovaError::InvalidInput(
            "Only standard HTTPS attachment URLs without userinfo are supported".into(),
        ));
    }
    let host = url
        .host_str()
        .ok_or_else(|| NovaError::InvalidInput("Attachment URL missing host".into()))?;
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if !ALLOWED_HOSTS
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")))
    {
        return Err(NovaError::InvalidInput(
            "Attachment URL origin is not approved".into(),
        ));
    }
    Ok(url)
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || a >= 224
        || a == 169 && b == 254
        || a == 172 && (16..=31).contains(&b)
        || a == 192 && b == 168
        || a == 100 && (64..=127).contains(&b)
        || a == 192 && b == 0 && c == 0
        || a == 198 && (b == 18 || b == 19)
        || a == 192 && b == 0 && c == 2
        || a == 198 && b == 51 && c == 100
        || a == 203 && b == 0 && c == 113)
}

async fn download(url: &str) -> NovaResult<Vec<u8>> {
    let url = permitted_url(url)?;
    let host = url
        .host_str()
        .ok_or_else(|| NovaError::InvalidInput("Missing host".into()))?;
    let addresses: Vec<_> = tokio::net::lookup_host((host, 443))
        .await
        .map_err(|_| NovaError::InvalidInput("Attachment DNS lookup failed".into()))?
        .filter_map(|addr| match addr.ip() {
            IpAddr::V4(ip) if public_ipv4(ip) => Some(ip),
            _ => None,
        })
        .collect();
    if addresses.is_empty() {
        return Err(NovaError::InvalidInput(
            "Attachment origin has no permitted public IPv4 address".into(),
        ));
    }
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .resolve(host, SocketAddr::new(IpAddr::V4(addresses[0]), 443))
        .timeout(Duration::from_secs(45))
        .build()
        .map_err(|_| NovaError::InvalidInput("Unable to configure attachment download".into()))?;
    let mut response = client
        .get(url)
        .send()
        .await
        .map_err(|_| NovaError::InvalidInput("Attachment download failed".into()))?;
    if !response.status().is_success() {
        return Err(NovaError::InvalidInput(
            "Attachment download rejected by remote provider".into(),
        ));
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_ATTACHMENT as u64)
    {
        return Err(NovaError::TooLarge(MAX_ATTACHMENT));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| NovaError::InvalidInput("Attachment transfer failed".into()))?
    {
        if bytes.len().saturating_add(chunk.len()) > MAX_ATTACHMENT {
            return Err(NovaError::TooLarge(MAX_ATTACHMENT));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn safe_basename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.chars().any(|ch| {
            ch.is_control() || matches!(ch, '/' | '\\' | ':' | '<' | '>' | '|' | '*' | '?' | '"')
        })
}

fn commit_import(
    state: &WorkspaceRegistry,
    request: &ImportRequest,
    bytes: &[u8],
) -> NovaResult<ImportResult> {
    if request.destination.is_empty() || !safe_basename(&request.file.file_id) {
        return Err(NovaError::InvalidInput(
            "Missing destination or invalid attachment file ID".into(),
        ));
    }
    if let Some(name) = request.file.file_name.as_deref()
        && !safe_basename(name)
    {
        return Err(NovaError::InvalidInput(
            "Unsafe original attachment filename".into(),
        ));
    }
    let hash = hex::encode(Sha256::digest(bytes));
    if let Some(expected) = request.expected_sha256.as_deref()
        && (expected.len() != 64
            || !expected.bytes().all(|b| b.is_ascii_hexdigit())
            || !hash.eq_ignore_ascii_case(expected))
    {
        return Err(NovaError::InvalidInput(
            "Attachment SHA-256 mismatch".into(),
        ));
    }
    let path = state.resolve(request.workspace_id.as_deref(), &request.destination, true)?;
    let root = state.workspace(request.workspace_id.as_deref())?.root;
    let relative = path
        .strip_prefix(&root)
        .map_err(|_| NovaError::OutsideAllowedRoot)?
        .to_string_lossy()
        .into_owned();
    state.with_write_lock(&path, || {
        state.resolve(request.workspace_id.as_deref(), &request.destination, true)?;
        if path.exists() && !request.overwrite {
            return Err(NovaError::InvalidInput(
                "Attachment destination already exists".into(),
            ));
        }
        let parent = path.parent().ok_or(NovaError::OutsideAllowedRoot)?;
        if !parent.is_dir() {
            return Err(NovaError::InvalidInput(
                "Attachment destination parent does not exist".into(),
            ));
        }
        // Create a unique sibling and only rename after flushing. A failed hash check
        // never opens the destination; Windows atomic overwrite semantics remain unverified.
        let temp_path = unique_sibling(parent)?;
        let outcome = (|| {
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temp_path)?;
            output.write_all(bytes)?;
            output.sync_all()?;
            if path.exists() && !request.overwrite {
                return Err(NovaError::InvalidInput(
                    "Attachment destination exists".into(),
                ));
            }
            fs::rename(&temp_path, &path)?;
            Ok(())
        })();
        if outcome.is_err() {
            let _ = fs::remove_file(&temp_path);
        }
        outcome?;
        Ok(ImportResult {
            path: relative,
            bytes: bytes.len(),
            sha256: hash,
            file_id: request.file.file_id.clone(),
            file_name: request.file.file_name.clone(),
            declared_mime_type: request.file.mime_type.clone(),
            detected_mime_type: None,
            verified: request.expected_sha256.is_some(),
            overwritten: request.overwrite,
        })
    })
}

fn unique_sibling(parent: &Path) -> NovaResult<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..32 {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(".nova-import-{}-{n}", std::process::id()));
        if !path.exists() {
            return Ok(path);
        }
    }
    Err(NovaError::InvalidInput(
        "Unable to allocate temporary attachment path".into(),
    ))
}

impl WorkspaceRegistry {
    pub async fn import_attachment(&self, request: ImportRequest) -> NovaResult<ImportResult> {
        if request.destination.is_empty() || request.file.file_id.is_empty() {
            return Err(NovaError::InvalidInput(
                "Attachment reference and destination required".into(),
            ));
        }
        // Fail invalid target before transferring any bytes.
        self.resolve(request.workspace_id.as_deref(), &request.destination, true)?;
        let bytes = download(&request.file.download_url).await?;
        let registry = self.clone();
        tokio::task::spawn_blocking(move || commit_import(&registry, &request, &bytes))
            .await
            .map_err(|_| NovaError::StatePoisoned)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn url_policy_blocks_local_private_and_lookalikes() {
        for candidate in [
            "http://files.oaiusercontent.com/x",
            "https://127.0.0.1/x",
            "https://files.oaiusercontent.com.evil.test/x",
            "file:///etc/passwd",
            "https://user:password@files.oaiusercontent.com/x",
            "https://files.oaiusercontent.com:8443/x",
        ] {
            assert!(
                permitted_url(candidate).is_err(),
                "should reject: {candidate}"
            );
        }
        assert!(permitted_url("https://files.oaiusercontent.com/abc").is_ok());
        assert!(!public_ipv4(Ipv4Addr::new(127, 0, 0, 1)));
        assert!(!public_ipv4(Ipv4Addr::new(192, 168, 1, 1)));
    }

    #[test]
    fn import_validates_digest_and_does_not_overwrite_without_opt_in() {
        let temp = tempfile::tempdir().expect("temp");
        let registry = WorkspaceRegistry::new(temp.path(), &[]).expect("registry");
        let request = ImportRequest {
            workspace_id: None,
            destination: "artifact.txt".into(),
            overwrite: false,
            expected_sha256: None,
            file: AttachmentRef {
                download_url: String::new(),
                file_id: "file-test".into(),
                mime_type: Some("text/plain".into()),
                file_name: Some("artifact.txt".into()),
            },
        };
        let first = commit_import(&registry, &request, b"ok").expect("save");
        assert_eq!(first.bytes, 2);
        assert!(commit_import(&registry, &request, b"changed").is_err());
        assert_eq!(
            fs::read(temp.path().join("artifact.txt")).expect("read"),
            b"ok"
        );
        let request2 = ImportRequest {
            expected_sha256: Some("0".repeat(64)),
            destination: "second.txt".into(),
            ..request
        };
        assert!(commit_import(&registry, &request2, b"changed").is_err());
        assert!(!temp.path().join("second.txt").exists());
    }
}
