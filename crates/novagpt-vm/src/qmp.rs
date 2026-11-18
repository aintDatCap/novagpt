//! Owned QMP desktop control. Only fixed, typed input and screenshot commands;
//! never expose arbitrary monitor commands or host paths to MCP callers.
use crate::{VmError, VmResult};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::{Value, json};
use std::{os::unix::fs::FileTypeExt, path::Path};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::{Duration, timeout},
};

const FRAME_BYTES: usize = 2 * 1024 * 1024;
const QMP_MAX_BYTES: usize = 256 * 1024;
const QMP_TIMEOUT: Duration = Duration::from_secs(12);

#[derive(Debug, Serialize)]
pub struct VmScreenshot {
    pub action: &'static str,
    pub id: String,
    pub mime_type: &'static str,
    pub width: u32,
    pub height: u32,
    pub data_base64: String,
}

#[derive(Debug, Serialize)]
pub struct VmDesktopAction {
    pub action: &'static str,
    pub id: String,
    pub sent: usize,
}

struct QmpConnection {
    socket: BufReader<UnixStream>,
}

impl QmpConnection {
    async fn connect(path: &Path) -> VmResult<Self> {
        if !path.is_absolute() || !path.symlink_metadata()?.file_type().is_socket() {
            return Err(VmError::NotOwned);
        }
        let stream = timeout(Duration::from_secs(3), UnixStream::connect(path))
            .await
            .map_err(|_| VmError::Timeout)??;
        let mut connection = Self {
            socket: BufReader::new(stream),
        };
        let greeting = connection.read_packet().await?;
        if greeting.get("QMP").is_none() {
            return Err(VmError::InvalidData);
        }
        connection
            .execute(json!({"execute":"qmp_capabilities"}))
            .await?;
        Ok(connection)
    }

    async fn read_packet(&mut self) -> VmResult<Value> {
        let mut buffer = Vec::new();
        timeout(QMP_TIMEOUT, async {
            loop {
                let available = self.socket.fill_buf().await?;
                if available.is_empty() {
                    return Err(VmError::CommandFailed);
                }
                let count = available
                    .iter()
                    .position(|&b| b == b'\n')
                    .map_or(available.len(), |index| index + 1);
                if count > QMP_MAX_BYTES.saturating_sub(buffer.len()) {
                    return Err(VmError::OutputLimit);
                }
                buffer.extend_from_slice(&available[..count]);
                let done = buffer.last() == Some(&b'\n');
                self.socket.consume(count);
                if done {
                    break;
                }
            }
            serde_json::from_slice::<Value>(&buffer).map_err(|_| VmError::InvalidData)
        })
        .await
        .map_err(|_| VmError::Timeout)?
    }

    async fn execute(&mut self, request: Value) -> VmResult<Value> {
        let mut data = serde_json::to_vec(&request).map_err(|_| VmError::InvalidData)?;
        if data.len() > 16 * 1024 {
            return Err(VmError::OutputLimit);
        }
        data.push(b'\n');
        timeout(QMP_TIMEOUT, self.socket.get_mut().write_all(&data))
            .await
            .map_err(|_| VmError::Timeout)??;
        for _ in 0..32 {
            let reply = self.read_packet().await?;
            if reply.get("error").is_some() {
                return Err(VmError::CommandFailed);
            }
            if let Some(result) = reply.get("return") {
                return Ok(result.clone());
            }
            // Asynchronous QMP events may precede an operation's reply.
            if reply.get("event").is_none() {
                return Err(VmError::InvalidData);
            }
        }
        Err(VmError::OutputLimit)
    }
}

fn png_dimensions(data: &[u8]) -> VmResult<(u32, u32)> {
    if data.len() < 24 || &data[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(VmError::InvalidData);
    }
    let width = u32::from_be_bytes(data[16..20].try_into().map_err(|_| VmError::InvalidData)?);
    let height = u32::from_be_bytes(data[20..24].try_into().map_err(|_| VmError::InvalidData)?);
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err(VmError::OutputLimit);
    }
    Ok((width, height))
}

/// File path is derived by VmManager, not provided by an MCP client.
pub async fn screenshot(id: &str, socket: &Path, frame: &Path) -> VmResult<VmScreenshot> {
    let mut qmp = QmpConnection::connect(socket).await?;
    // Never follow an unexpected file or symlink substituted by another process.
    if frame.symlink_metadata().is_ok() {
        return Err(VmError::NotOwned);
    }
    let name = frame.to_str().ok_or(VmError::InvalidData)?;
    timeout(
        QMP_TIMEOUT,
        qmp.execute(json!({
            "execute": "screendump",
            "arguments": { "filename": name, "format": "png" }
        })),
    )
    .await
    .map_err(|_| VmError::Timeout)??;
    let metadata = frame.symlink_metadata()?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > FRAME_BYTES as u64
    {
        return Err(VmError::OutputLimit);
    }
    let data = fs::read(frame).await?;
    let result = (|| {
        if data.len() > FRAME_BYTES {
            return Err(VmError::OutputLimit);
        }
        let (width, height) = png_dimensions(&data)?;
        Ok(VmScreenshot {
            action: "screenshot",
            id: id.to_owned(),
            mime_type: "image/png",
            width,
            height,
            data_base64: STANDARD.encode(data),
        })
    })();
    fs::remove_file(frame).await?;
    result
}

fn qcodes(keys: &[String]) -> VmResult<Value> {
    if keys.is_empty() || keys.len() > 8 {
        return Err(VmError::InvalidData);
    }
    for key in keys {
        if key.is_empty()
            || key.len() > 24
            || !key
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err(VmError::InvalidData);
        }
    }
    let mut events: Vec<Value> = Vec::new();
    for key in keys {
        events.push(json!({"type":"key","data":{"down":true,"key":{"type":"qcode","data":key}}}));
    }
    for key in keys.iter().rev() {
        events.push(json!({"type":"key","data":{"down":false,"key":{"type":"qcode","data":key}}}));
    }
    Ok(Value::Array(events))
}

/// Request ACPI power-off. Caller must verify that QEMU actually exits;
/// the QMP acknowledgement alone is not evidence of a clean guest shutdown.
pub async fn request_powerdown(socket: &Path) -> VmResult<()> {
    let mut qmp = QmpConnection::connect(socket).await?;
    qmp.execute(json!({"execute":"system_powerdown"})).await?;
    Ok(())
}

pub async fn keys(id: &str, socket: &Path, keys: &[String]) -> VmResult<VmDesktopAction> {
    let events = qcodes(keys)?;
    let mut qmp = QmpConnection::connect(socket).await?;
    qmp.execute(json!({"execute":"input-send-event","arguments":{"events":events}}))
        .await?;
    Ok(VmDesktopAction {
        action: "keys",
        id: id.to_owned(),
        sent: keys.len(),
    })
}

pub async fn pointer(
    id: &str,
    socket: &Path,
    x: u16,
    y: u16,
    button: Option<&str>,
) -> VmResult<VmDesktopAction> {
    let button = match button {
        Some("left" | "right" | "middle") => button,
        None => None,
        _ => return Err(VmError::InvalidData),
    };
    // Coordinates are absolute tablet coordinates, 0..32767 on both axes.
    if x > 32767 || y > 32767 {
        return Err(VmError::InvalidData);
    }
    let mut events = vec![
        json!({"type":"abs","data":{"axis":"x","value":x}}),
        json!({"type":"abs","data":{"axis":"y","value":y}}),
    ];
    if let Some(button) = button {
        events.push(json!({"type":"btn","data":{"button":button,"down":true}}));
        events.push(json!({"type":"btn","data":{"button":button,"down":false}}));
    }
    let count = events.len();
    let mut qmp = QmpConnection::connect(socket).await?;
    qmp.execute(json!({"execute":"input-send-event","arguments":{"events":events}}))
        .await?;
    Ok(VmDesktopAction {
        action: "pointer",
        id: id.to_owned(),
        sent: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn real_disposable_qemu_screen_and_input() {
        use crate::{VmCreate, VmManager};
        let temp = tempfile::tempdir().expect("isolated VM test storage");
        let base = temp.path().join("disposable.qcow2");
        let img = tokio::process::Command::new("qemu-img")
            .args(["create", "-f", "qcow2"])
            .arg(&base)
            .arg("32M")
            .output()
            .await
            .expect("qemu-img executable");
        assert!(
            img.status.success(),
            "temporary empty qcow2 must be created"
        );
        let manager = VmManager::new(temp.path().join("managed")).expect("isolated VM manager");
        manager
            .import_image("blank-test", &base, 1, 512)
            .expect("approved temporary image");
        let created = manager
            .create(VmCreate {
                image: "blank-test".into(),
                cpus: None,
                memory_mb: None,
                linux_options: Some(crate::linux_qemu::LinuxVmOptions {
                    networks: vec![crate::linux_qemu::VmNetwork::Nat],
                    display: true,
                    guest_agent: true,
                }),
                installer_iso: None,
            })
            .await
            .expect("temporary QEMU fork");
        let id = created.instance.id;
        let result = async {
            let mut screenshot_result = None;
            for _ in 0..40 {
                match manager.desktop_screenshot(&id).await {
                    Ok(screenshot) => {
                        screenshot_result = Some(screenshot);
                        break;
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
                }
            }
            let screenshot = screenshot_result.ok_or(VmError::CommandFailed)?;
            assert_eq!(screenshot.mime_type, "image/png");
            assert!(screenshot.width >= 320 && screenshot.height >= 240);
            assert!(screenshot.data_base64.len() > 100);
            assert!(manager.desktop_keys(&id, &["a".to_owned()]).await?.sent > 0);
            assert!(
                manager
                    .desktop_pointer(&id, 8192, 8192, Some("left"))
                    .await?
                    .sent
                    > 0
            );
            let guest = manager.guest_status_live(&id).await?;
            assert!(!guest.available, "blank qcow2 has no installed guest agent");
            Ok::<(), VmError>(())
        }
        .await;
        let cleanup = manager.destroy(&id).await;
        assert!(result.is_ok(), "real QMP desktop behavior: {result:?}");
        assert!(cleanup.is_ok(), "QEMU disposable cleanup: {cleanup:?}");
    }

    #[test]
    fn rejects_bad_keyboard_events_and_malformed_png() {
        assert!(qcodes(&[]).is_err());
        assert!(qcodes(&["a;rm".into()]).is_err());
        assert!(qcodes(&["ctrl".into(), "a".into()]).is_ok());
        assert!(png_dimensions(b"not-a-png").is_err());
    }
}
