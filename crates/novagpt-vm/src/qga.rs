//! Bounded QEMU Guest Agent client for NovaGPT-owned Linux-hosted QEMU guests.
//!
//! A caller must verify the VM's ownership and derive the per-instance socket
//! from its trusted VM directory before calling this module. A socket path on
//! its own cannot prove that the connected QEMU process belongs to NovaGPT.
//! Guest commands run inside the VM, never through a host shell or QMP.
//!
//! QGA has no standard guest-exec cancellation operation. Timing out a command
//! stops waiting for its result, but does not terminate the guest process.

use crate::{VmError, VmExecResult, VmResult};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{os::unix::fs::FileTypeExt, path::Path, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::{sleep, timeout},
};

const MAX_REQUEST_BYTES: usize = 24 * 1024;
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const MAX_TRANSFER_BYTES: usize = 1024 * 1024;
const MAX_CAPTURE_BYTES: usize = 48 * 1024;
const CHUNK_BYTES: usize = 12 * 1024;
const CALL_TIMEOUT: Duration = Duration::from_secs(15);
const TRANSFER_TIMEOUT: Duration = Duration::from_secs(120);
const MIN_EXEC_MS: u64 = 100;
const MAX_EXEC_MS: u64 = 300_000;

struct Session {
    stream: BufReader<UnixStream>,
}

impl Session {
    async fn connect(path: &Path) -> VmResult<Self> {
        if !path.is_absolute() {
            return Err(VmError::InvalidData);
        }
        // Deny symlink endpoints and regular files. VM ownership is checked by
        // the caller, which knows the per-instance VM home and process.
        let meta = tokio::fs::symlink_metadata(path).await?;
        if meta.file_type().is_symlink() || !meta.file_type().is_socket() {
            return Err(VmError::NotOwned);
        }
        let stream = timeout(Duration::from_secs(5), UnixStream::connect(path))
            .await
            .map_err(|_| VmError::Timeout)??;
        Ok(Self {
            stream: BufReader::new(stream),
        })
    }

    async fn call(&mut self, command: Value) -> VmResult<Value> {
        let mut payload = serde_json::to_vec(&command).map_err(|_| VmError::InvalidData)?;
        if payload.len() >= MAX_REQUEST_BYTES {
            return Err(VmError::OutputLimit);
        }
        payload.push(b'\n');
        self.stream.get_mut().write_all(&payload).await?;

        // fill_buf/consume preserves bytes after the first newline for the
        // next response. read_until would allocate an unbounded frame.
        let mut reply = Vec::new();
        loop {
            let available = self.stream.fill_buf().await?;
            if available.is_empty() {
                return Err(VmError::CommandFailed);
            }
            let count = available
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(available.len(), |pos| pos + 1);
            if count > MAX_RESPONSE_BYTES.saturating_sub(reply.len()) {
                return Err(VmError::OutputLimit);
            }
            reply.extend_from_slice(&available[..count]);
            let complete = reply.last() == Some(&b'\n');
            self.stream.consume(count);
            if complete {
                break;
            }
        }
        let value: Value = serde_json::from_slice(&reply).map_err(|_| VmError::InvalidData)?;
        // Do not return QGA's error descriptions: they may include guest paths.
        if value.get("error").is_some() {
            return Err(VmError::CommandFailed);
        }
        value.get("return").cloned().ok_or(VmError::InvalidData)
    }

    async fn bounded_call(&mut self, command: Value) -> VmResult<Value> {
        timeout(CALL_TIMEOUT, self.call(command))
            .await
            .map_err(|_| VmError::Timeout)?
    }

    async fn close_file(&mut self, handle: u64) -> VmResult<()> {
        self.bounded_call(json!({
            "execute": "guest-file-close",
            "arguments": { "handle": handle }
        }))
        .await?;
        Ok(())
    }
}

fn validate_guest_path(id: &str, guest_path: &str) -> VmResult<()> {
    crate::validate_id(id)?;
    let bytes = guest_path.as_bytes();
    if bytes.is_empty()
        || bytes.len() > 4096
        || bytes.iter().any(u8::is_ascii_control)
        || guest_path.split(['/', '\\']).any(|segment| segment == "..")
    {
        return Err(VmError::InvalidData);
    }
    let unix_absolute = guest_path.starts_with('/');
    let windows_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/');
    if !unix_absolute && !windows_absolute {
        return Err(VmError::InvalidData);
    }
    Ok(())
}

fn decode_b64_bounded(data: &str, max_bytes: usize) -> VmResult<Vec<u8>> {
    if data.len() > max_bytes.div_ceil(3) * 4 {
        return Err(VmError::OutputLimit);
    }
    let bytes = STANDARD.decode(data).map_err(|_| VmError::InvalidData)?;
    if bytes.len() > max_bytes {
        return Err(VmError::OutputLimit);
    }
    Ok(bytes)
}

fn output_bytes(status: &Value, key: &str) -> VmResult<Vec<u8>> {
    match status.get(key) {
        None => Ok(Vec::new()),
        Some(Value::String(encoded)) => decode_b64_bounded(encoded, MAX_CAPTURE_BYTES),
        Some(_) => Err(VmError::InvalidData),
    }
}

/// Check that an owned VM's QGA socket responds to the guest-ping command.
pub async fn ping(socket: &Path) -> VmResult<()> {
    let mut session = Session::connect(socket).await?;
    session
        .bounded_call(json!({"execute": "guest-ping"}))
        .await?;
    Ok(())
}

/// Execute a program directly inside the guest, with no host-side shell.
///
/// Output is UTF-8 lossy (QGA reports base64-encoded raw bytes). A timeout
/// does not terminate the already-started guest program.
pub async fn exec(
    id: &str,
    socket: &Path,
    program: &str,
    args: &[String],
    timeout_ms: u64,
) -> VmResult<VmExecResult> {
    crate::validate_id(id)?;
    if !(MIN_EXEC_MS..=MAX_EXEC_MS).contains(&timeout_ms)
        || program.is_empty()
        || program.len() > 4096
        || program.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r')
        || args.len() > 256
        || args.iter().any(|arg| {
            arg.len() > 16_384 || arg.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r')
        })
    {
        return Err(VmError::InvalidData);
    }

    // The deadline covers connection, guest-exec and all subsequent polls.
    timeout(Duration::from_millis(timeout_ms), async {
        let mut session = Session::connect(socket).await?;
        let started = session
            .bounded_call(json!({
                "execute": "guest-exec",
                "arguments": {
                    "path": program,
                    "arg": args,
                    "capture-output": true
                }
            }))
            .await?;
        let pid = started
            .get("pid")
            .and_then(Value::as_u64)
            .filter(|&pid| pid > 0)
            .ok_or(VmError::InvalidData)?;
        loop {
            let state = session
                .bounded_call(json!({
                    "execute": "guest-exec-status",
                    "arguments": { "pid": pid }
                }))
                .await?;
            match state.get("exited").and_then(Value::as_bool) {
                Some(true) => {
                    let code = state
                        .get("exitcode")
                        .and_then(Value::as_i64)
                        .ok_or(VmError::InvalidData)?;
                    let stdout = output_bytes(&state, "out-data")?;
                    let stderr = output_bytes(&state, "err-data")?;
                    if stdout.len() + stderr.len() > MAX_CAPTURE_BYTES {
                        return Err(VmError::OutputLimit);
                    }
                    let stdout = String::from_utf8_lossy(&stdout).into_owned();
                    let stderr = String::from_utf8_lossy(&stderr).into_owned();
                    if stdout.len() + stderr.len() > MAX_CAPTURE_BYTES {
                        return Err(VmError::OutputLimit);
                    }
                    let truncated = state
                        .get("out-truncated")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                        || state
                            .get("err-truncated")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                    return Ok(VmExecResult {
                        action: "exec",
                        id: id.to_owned(),
                        exit_code: i32::try_from(code).map_err(|_| VmError::InvalidData)?,
                        stdout,
                        stderr,
                        truncated,
                    });
                }
                Some(false) => sleep(Duration::from_millis(100)).await,
                None => return Err(VmError::InvalidData),
            }
        }
    })
    .await
    .map_err(|_| VmError::Timeout)?
}

/// Write up to 1 MiB to an absolute guest path, in bounded QGA chunks.
///
/// This overwrites the guest file. Failure may leave a partial guest file;
/// callers should use a staging path if atomic replacement is required.
pub async fn upload(
    id: &str,
    socket: &Path,
    guest_path: &str,
    data_base64: &str,
) -> VmResult<Value> {
    validate_guest_path(id, guest_path)?;
    let bytes = decode_b64_bounded(data_base64, MAX_TRANSFER_BYTES)?;
    let mut session = Session::connect(socket).await?;
    let opened = session
        .bounded_call(json!({
            "execute": "guest-file-open",
            "arguments": { "path": guest_path, "mode": "wb" }
        }))
        .await?;
    let handle = opened.as_u64().ok_or(VmError::InvalidData)?;
    let written = timeout(TRANSFER_TIMEOUT, async {
        let mut offset = 0;
        while offset < bytes.len() {
            let end = bytes.len().min(offset + CHUNK_BYTES);
            let chunk = &bytes[offset..end];
            let result = session
                .bounded_call(json!({
                    "execute": "guest-file-write",
                    "arguments": {
                        "handle": handle,
                        "buf-b64": STANDARD.encode(chunk)
                    }
                }))
                .await?;
            let count = result
                .get("count")
                .and_then(Value::as_u64)
                .ok_or(VmError::InvalidData)?;
            let count = usize::try_from(count).map_err(|_| VmError::InvalidData)?;
            if count == 0 || count > chunk.len() {
                return Err(VmError::CommandFailed);
            }
            offset += count;
        }
        Ok(())
    })
    .await
    .map_err(|_| VmError::Timeout)
    .and_then(|result: VmResult<()>| result);

    // Always attempt closing an opened QGA handle, including after a failed
    // transfer. Preserve the original transfer error if close also fails.
    let closed = session.close_file(handle).await;
    written?;
    closed?;
    Ok(json!({
        "action": "upload",
        "id": id,
        "guest_path": guest_path,
        "bytes": bytes.len()
    }))
}

/// Read an absolute guest path as base64, with a strict 1 MiB data limit.
pub async fn download(id: &str, socket: &Path, guest_path: &str) -> VmResult<Value> {
    validate_guest_path(id, guest_path)?;
    let mut session = Session::connect(socket).await?;
    let opened = session
        .bounded_call(json!({
            "execute": "guest-file-open",
            "arguments": { "path": guest_path, "mode": "rb" }
        }))
        .await?;
    let handle = opened.as_u64().ok_or(VmError::InvalidData)?;
    let contents = timeout(TRANSFER_TIMEOUT, async {
        let mut data = Vec::new();
        loop {
            let result = session
                .bounded_call(json!({
                    "execute": "guest-file-read",
                    "arguments": { "handle": handle, "count": CHUNK_BYTES }
                }))
                .await?;
            let encoded = result
                .get("buf-b64")
                .and_then(Value::as_str)
                .ok_or(VmError::InvalidData)?;
            let chunk = decode_b64_bounded(encoded, CHUNK_BYTES)?;
            let count = result
                .get("count")
                .and_then(Value::as_u64)
                .ok_or(VmError::InvalidData)?;
            if usize::try_from(count).ok() != Some(chunk.len()) {
                return Err(VmError::InvalidData);
            }
            if chunk.len() > MAX_TRANSFER_BYTES.saturating_sub(data.len()) {
                return Err(VmError::OutputLimit);
            }
            data.extend_from_slice(&chunk);
            match result.get("eof").and_then(Value::as_bool) {
                Some(true) => return Ok(data),
                Some(false) if chunk.is_empty() => return Err(VmError::CommandFailed),
                Some(false) => {}
                None => return Err(VmError::InvalidData),
            }
        }
    })
    .await
    .map_err(|_| VmError::Timeout)
    .and_then(|result: VmResult<Vec<u8>>| result);

    let closed = session.close_file(handle).await;
    let data = contents?;
    closed?;
    Ok(json!({
        "action": "download",
        "id": id,
        "guest_path": guest_path,
        "bytes": data.len(),
        "data_base64": STANDARD.encode(data)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{io::AsyncBufReadExt, net::UnixListener, task::JoinHandle};

    const ID: &str = "vm-0123456789abcdef";

    fn mock<F, Fut>(server: F) -> (tempfile::TempDir, std::path::PathBuf, JoinHandle<()>)
    where
        F: FnOnce(UnixStream) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let temp = tempfile::tempdir().expect("socket directory");
        let path = temp.path().join("qga.sock");
        let listener = UnixListener::bind(&path).expect("bind QGA mock");
        let task = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept");
            server(stream).await;
        });
        (temp, path, task)
    }

    async fn receive(stream: &mut BufReader<UnixStream>) -> Value {
        let mut request = String::new();
        stream.read_line(&mut request).await.expect("request");
        serde_json::from_str(&request).expect("JSON command")
    }

    async fn answer(stream: &mut BufReader<UnixStream>, response: Value) {
        let mut text = serde_json::to_vec(&response).expect("JSON response");
        text.push(b'\n');
        stream.get_mut().write_all(&text).await.expect("reply");
    }

    #[tokio::test]
    async fn ping_succeeds_over_unix_socket() {
        let (_temp, path, server) = mock(|socket| async move {
            let mut stream = BufReader::new(socket);
            assert_eq!(receive(&mut stream).await["execute"], "guest-ping");
            answer(&mut stream, json!({"return": {}})).await;
        });
        ping(&path).await.expect("guest ping");
        server.await.expect("server");
    }

    #[tokio::test]
    async fn guest_agent_error_is_redacted() {
        let (_temp, path, server) = mock(|socket| async move {
            let mut stream = BufReader::new(socket);
            receive(&mut stream).await;
            answer(
                &mut stream,
                json!({
                    "error": {"class":"GenericError","desc":"private guest file path"}
                }),
            )
            .await;
        });
        assert!(matches!(ping(&path).await, Err(VmError::CommandFailed)));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn exec_polls_and_decodes_output() {
        let (_temp, path, server) = mock(|socket| async move {
            let mut stream = BufReader::new(socket);
            let start = receive(&mut stream).await;
            assert_eq!(start["execute"], "guest-exec");
            assert_eq!(start["arguments"]["arg"], json!(["hello"]));
            answer(&mut stream, json!({"return": {"pid": 123}})).await;
            let poll = receive(&mut stream).await;
            assert_eq!(poll["execute"], "guest-exec-status");
            answer(&mut stream, json!({"return": {"exited":false}})).await;
            let poll = receive(&mut stream).await;
            assert_eq!(poll["arguments"]["pid"], 123);
            answer(
                &mut stream,
                json!({"return": {
                    "exited": true, "exitcode": 7,
                    "out-data": STANDARD.encode(b"result"),
                    "err-data": STANDARD.encode(b"warning"),
                    "out-truncated": true
                }}),
            )
            .await;
        });
        let result = exec(ID, &path, "/bin/echo", &["hello".into()], 1500)
            .await
            .expect("guest exec");
        assert_eq!(result.exit_code, 7);
        assert_eq!(result.stdout, "result");
        assert_eq!(result.stderr, "warning");
        assert!(result.truncated);
        server.await.expect("server");
    }

    #[tokio::test]
    async fn upload_handles_partial_writes_and_closes_handle() {
        let (_temp, path, server) = mock(|socket| async move {
            let mut stream = BufReader::new(socket);
            assert_eq!(receive(&mut stream).await["execute"], "guest-file-open");
            answer(&mut stream, json!({"return": 7})).await;
            let write = receive(&mut stream).await;
            assert_eq!(write["execute"], "guest-file-write");
            assert_eq!(write["arguments"]["buf-b64"], STANDARD.encode(b"abc"));
            answer(&mut stream, json!({"return":{"count":1}})).await;
            let write = receive(&mut stream).await;
            assert_eq!(write["arguments"]["buf-b64"], STANDARD.encode(b"bc"));
            answer(&mut stream, json!({"return":{"count":2}})).await;
            assert_eq!(receive(&mut stream).await["execute"], "guest-file-close");
            answer(&mut stream, json!({"return": {}})).await;
        });
        let result = upload(ID, &path, "/tmp/test", &STANDARD.encode(b"abc"))
            .await
            .expect("upload");
        assert_eq!(result["bytes"], 3);
        server.await.expect("server");
    }

    #[tokio::test]
    async fn download_collects_chunks_and_closes_handle() {
        let (_temp, path, server) = mock(|socket| async move {
            let mut stream = BufReader::new(socket);
            assert_eq!(receive(&mut stream).await["execute"], "guest-file-open");
            answer(&mut stream, json!({"return": 8})).await;
            assert_eq!(receive(&mut stream).await["execute"], "guest-file-read");
            answer(
                &mut stream,
                json!({"return":{
                    "count":2, "buf-b64":STANDARD.encode(b"ab"), "eof":false
                }}),
            )
            .await;
            assert_eq!(receive(&mut stream).await["execute"], "guest-file-read");
            answer(
                &mut stream,
                json!({"return":{
                    "count":1, "buf-b64":STANDARD.encode(b"c"), "eof":true
                }}),
            )
            .await;
            assert_eq!(receive(&mut stream).await["execute"], "guest-file-close");
            answer(&mut stream, json!({"return": {}})).await;
        });
        let result = download(ID, &path, r"C:\\Temp\\file")
            .await
            .expect("download");
        assert_eq!(result["bytes"], 3);
        assert_eq!(result["data_base64"], STANDARD.encode(b"abc"));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn failed_write_still_attempts_file_close() {
        let (_temp, path, server) = mock(|socket| async move {
            let mut stream = BufReader::new(socket);
            receive(&mut stream).await;
            answer(&mut stream, json!({"return": 9})).await;
            assert_eq!(receive(&mut stream).await["execute"], "guest-file-write");
            answer(&mut stream, json!({"error":{"class":"GenericError"}})).await;
            assert_eq!(receive(&mut stream).await["execute"], "guest-file-close");
            answer(&mut stream, json!({"return": {}})).await;
        });
        assert!(matches!(
            upload(ID, &path, "/tmp/f", &STANDARD.encode(b"payload")).await,
            Err(VmError::CommandFailed)
        ));
        server.await.expect("server");
    }

    #[tokio::test]
    async fn transfer_and_command_validation_reject_unsafe_inputs() {
        let missing = Path::new("/no-such-owned-vm/qga.sock");
        assert!(matches!(
            upload("wrong", missing, "/tmp/f", "").await,
            Err(VmError::InvalidName)
        ));
        assert!(matches!(
            download(ID, missing, "../relative").await,
            Err(VmError::InvalidData)
        ));
        assert!(matches!(
            download(ID, missing, "/tmp/../sensitive").await,
            Err(VmError::InvalidData)
        ));
        assert!(matches!(
            upload(
                ID,
                missing,
                "/tmp/f",
                &STANDARD.encode(vec![0; MAX_TRANSFER_BYTES + 1])
            )
            .await,
            Err(VmError::OutputLimit)
        ));
        assert!(matches!(
            upload(ID, missing, "/tmp/f", "not-base64").await,
            Err(VmError::InvalidData)
        ));
        assert!(matches!(
            exec(ID, missing, "/bin/echo", &[], 99).await,
            Err(VmError::InvalidData)
        ));
        assert!(matches!(
            exec(ID, missing, "/bin/echo", &[], 300_001).await,
            Err(VmError::InvalidData)
        ));
        assert!(matches!(
            exec(ID, missing, "bad\ncommand", &[], 1000).await,
            Err(VmError::InvalidData)
        ));
    }

    #[tokio::test]
    async fn oversized_agent_response_is_rejected() {
        let (_temp, path, server) = mock(|socket| async move {
            let mut stream = BufReader::new(socket);
            receive(&mut stream).await;
            let mut huge = vec![b' '; MAX_RESPONSE_BYTES + 1];
            huge.push(b'\n');
            let _ = stream.get_mut().write_all(&huge).await;
        });
        assert!(matches!(ping(&path).await, Err(VmError::OutputLimit)));
        server.await.expect("server");
    }
}
