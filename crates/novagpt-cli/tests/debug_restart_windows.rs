//! Real Windows CLI -> HTTP MCP -> debug_restart -> loopback supervisor.
#![cfg(all(debug_assertions, windows))]
use std::{
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn unused_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("free port");
    listener.local_addr().expect("local port").port()
}

async fn available(port: u16) {
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("isolated MCP HTTP listener unavailable");
}

fn isolated_server(
    root: &Path,
    state: &Path,
    port: u16,
    control: Option<(u16, &str)>,
) -> OwnedChild {
    let mut command = Command::new(env!("CARGO_BIN_EXE_novagpt"));
    command
        .arg("--root")
        .arg(root)
        .args(["http", "--port"])
        .arg(port.to_string())
        .arg("--no-ngrok")
        .env("NOVAGPT_HTTP_TOKEN", "Q".repeat(40))
        .env("LOCALAPPDATA", state)
        .env_remove("NOVAGPT_DEV_RESTART_SOCKET")
        .env_remove("NOVAGPT_DEV_RESTART_PORT")
        .env_remove("NOVAGPT_DEV_RESTART_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some((control_port, token)) = control {
        command
            .env("NOVAGPT_DEV_RESTART_PORT", control_port.to_string())
            .env("NOVAGPT_DEV_RESTART_TOKEN", token);
    }
    OwnedChild(command.spawn().expect("start temporary MCP server"))
}

async fn request(root: &Path, state: &Path, port: u16) -> std::process::Output {
    let root = root.to_owned();
    let state = state.to_owned();
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_novagpt"))
            .arg("--root")
            .arg(root)
            .args(["debug-restart", "--port"])
            .arg(port.to_string())
            .env("NOVAGPT_HTTP_TOKEN", "Q".repeat(40))
            .env("LOCALAPPDATA", state)
            .output()
            .expect("debug-restart command")
    })
    .await
    .expect("CLI join")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_restart_schedules_via_authenticated_local_supervisor() {
    let temp = tempfile::tempdir().expect("fixture");
    let root = temp.path().join("source");
    let state = temp.path().join("private");
    std::fs::create_dir(&root).expect("workspace");
    std::fs::create_dir(&state).expect("state");

    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("loopback supervisor");
    let control_port = listener.local_addr().expect("address").port();
    let capability = "a".repeat(64);
    let port = unused_port();
    let _server = isolated_server(&root, &state, port, Some((control_port, &capability)));
    available(port).await;

    let supervisor = tokio::spawn(async move {
        for reply in [b"accepted\n".as_slice(), b"pending\n".as_slice()] {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(10), listener.accept())
                .await
                .expect("restart arrives")
                .expect("supervisor connection");
            let mut request = vec![0; 64 + b" restart\n".len()];
            stream.read_exact(&mut request).await.expect("request");
            assert_eq!(request, format!("{capability} restart\n").as_bytes());
            stream.write_all(reply).await.expect("acknowledge");
        }
    });

    let first = request(&root, &state, port).await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert!(String::from_utf8_lossy(&first.stdout).contains("restart scheduled"));
    let second = request(&root, &state, port).await;
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stdout).contains("already scheduled"));
    supervisor.await.expect("supervisor task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn restart_denied_without_control_or_running_server() {
    let temp = tempfile::tempdir().expect("fixture");
    let root = temp.path().join("source");
    let state = temp.path().join("private");
    std::fs::create_dir(&root).expect("workspace");
    std::fs::create_dir(&state).expect("state");
    let port = unused_port();
    let _server = isolated_server(&root, &state, port, None);
    available(port).await;

    let denied = request(&root, &state, port).await;
    assert!(!denied.status.success());
    assert!(
        String::from_utf8_lossy(&denied.stderr)
            .contains("Development supervisor unavailable or restart denied")
    );
    let missing = request(&root, &state, unused_port()).await;
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("Local MCP server unreachable"));
}
