//! Real local CLI -> HTTP MCP -> debug_restart -> supervisor socket contract.
//! Uses disposable state and a simulated supervisor; no running dev service touched.
#![cfg(all(debug_assertions, unix))]

use std::{
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
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

fn isolated_server(root: &Path, state: &Path, port: u16, socket: Option<&Path>) -> OwnedChild {
    let mut command = Command::new(env!("CARGO_BIN_EXE_novagpt"));
    command
        .arg("--root")
        .arg(root)
        .args(["http", "--port"])
        .arg(port.to_string())
        .arg("--no-ngrok")
        .env("NOVAGPT_HTTP_TOKEN", "Q".repeat(40))
        .env("XDG_STATE_HOME", state)
        .env_remove("NOVAGPT_DEV_RESTART_SOCKET")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(socket) = socket {
        command.env("NOVAGPT_DEV_RESTART_SOCKET", socket);
    }
    OwnedChild(command.spawn().expect("run temporary NovaGPT HTTP"))
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
            .env("XDG_STATE_HOME", state)
            .output()
            .expect("run debug-restart command")
    })
    .await
    .expect("join CLI command")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_restart_schedules_via_real_http_mcp_and_handles_pending() {
    let temp = tempfile::tempdir().expect("fixture");
    let root = temp.path().join("source");
    let state = temp.path().join("private");
    std::fs::create_dir(&root).expect("workspace");
    std::fs::create_dir(&state).expect("state");
    let socket_path = temp.path().join("dev-control.sock");
    let server_socket = tokio::net::UnixListener::bind(&socket_path).expect("supervisor socket");
    std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o600))
        .expect("private control socket");
    let port = unused_port();
    let _server = isolated_server(&root, &state, port, Some(&socket_path));
    available(port).await;

    let supervisor = tokio::spawn(async move {
        for reply in [b"accepted\n".as_slice(), b"pending\n".as_slice()] {
            let (mut stream, _) =
                tokio::time::timeout(Duration::from_secs(10), server_socket.accept())
                    .await
                    .expect("restart arrives")
                    .expect("supervisor connection");
            let mut command = [0; 8];
            stream
                .read_exact(&mut command)
                .await
                .expect("restart request");
            assert_eq!(&command, b"restart\n");
            stream.write_all(reply).await.expect("acknowledge request");
        }
    });

    let first = request(&root, &state, port).await;
    assert!(
        first.status.success(),
        "{:?}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_text = String::from_utf8_lossy(&first.stdout);
    assert!(first_text.contains("restart scheduled"), "{first_text}");
    assert!(!first_text.contains(&"Q".repeat(40)), "token must not leak");

    let second = request(&root, &state, port).await;
    assert!(
        second.status.success(),
        "{:?}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert!(String::from_utf8_lossy(&second.stdout).contains("already scheduled"));
    supervisor.await.expect("supervisor task");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_restart_denies_plain_http_without_supervisor_and_unreachable_ports() {
    let temp = tempfile::tempdir().expect("fixture");
    let root = temp.path().join("source");
    let state = temp.path().join("private");
    std::fs::create_dir(&root).expect("workspace");
    std::fs::create_dir(&state).expect("state");
    let port = unused_port();
    let _server = isolated_server(&root, &state, port, None);
    available(port).await;

    let denied = request(&root, &state, port).await;
    assert!(
        !denied.status.success(),
        "ordinary MCP must not be able to restart"
    );
    let error = String::from_utf8_lossy(&denied.stderr);
    assert!(error.contains("Development supervisor unavailable or restart denied"));
    assert!(
        !error.contains(&"Q".repeat(40)),
        "never reveal the credential"
    );

    let missing = request(&root, &state, unused_port()).await;
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("Local MCP server unreachable"));
}
