use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn with_private_state(command: &mut Command, home: &Path) {
    if cfg!(windows) {
        command.env("LOCALAPPDATA", home);
    } else {
        command.env("XDG_STATE_HOME", home);
    }
}
fn run(root: &Path, home: &Path, action: &str) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_novagpt"));
    command.arg("--root").arg(root).arg("token").arg(action);
    command.env_remove("NOVAGPT_HTTP_TOKEN");
    with_private_state(&mut command, home);
    command.output().expect("run CLI")
}

struct ProcessGuard(std::process::Child);
impl Drop for ProcessGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn response_status(port: u16, token: Option<&str>) -> String {
    use std::{
        io::{Read, Write},
        net::TcpStream,
        time::Duration,
    };
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect to local server");
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let auth = token.map_or(String::new(), |token| {
        format!("Authorization: Bearer {token}\r\n")
    });
    write!(
        stream,
        "GET /mcp HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{auth}\r\n"
    )
    .expect("send HTTP request");
    let mut reply = [0u8; 1024];
    let count = stream.read(&mut reply).expect("read HTTP status");
    String::from_utf8_lossy(&reply[..count])
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}
#[test]
fn generated_secret_is_accepted_by_real_http_server() {
    use std::{
        net::{TcpListener, TcpStream},
        process::Stdio,
        thread,
        time::Duration,
    };
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut command = Command::new(env!("CARGO_BIN_EXE_novagpt"));
    command
        .arg("--root")
        .arg(root.path())
        .arg("http")
        .arg("--port")
        .arg(port.to_string())
        .env_remove("NOVAGPT_HTTP_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    with_private_state(&mut command, private.path());
    let mut child = ProcessGuard(command.spawn().expect("start real MCP HTTP process"));
    let mut ready = false;
    for _ in 0..150 {
        if let Some(exit) = child.0.try_wait().unwrap() {
            panic!("MCP server exited during startup: {exit}");
        }
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        thread::sleep(Duration::from_millis(30));
    }
    assert!(ready, "HTTP server did not start");
    let revealed = run(root.path(), private.path(), "show");
    assert!(revealed.status.success());
    let token = String::from_utf8(revealed.stdout)
        .unwrap()
        .trim()
        .to_string();
    assert!(response_status(port, None).contains("401"));
    assert!(response_status(port, Some(&"0".repeat(64))).contains("401"));
    assert!(!response_status(port, Some(&token)).contains("401"));
}
#[test]
fn token_show_is_stable_per_workspace_and_kept_outside_git_tree() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let first = run(root.path(), private.path(), "show");
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let token = String::from_utf8(first.stdout).unwrap().trim().to_string();
    assert_eq!(token.len(), 64);
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
    let next = run(root.path(), private.path(), "show");
    assert_eq!(String::from_utf8(next.stdout).unwrap().trim(), token);
    let location = run(root.path(), private.path(), "path");
    assert!(location.status.success());
    let location = String::from_utf8(location.stdout)
        .unwrap()
        .trim()
        .to_string();
    let file = Path::new(&location);
    assert!(file.is_file());
    assert!(!file.starts_with(root.path()));
    assert_eq!(fs::read_to_string(file).unwrap(), token);
    assert!(!root.path().join(".novagpt").exists());
    let different = run(other.path(), private.path(), "show");
    assert!(different.status.success());
    assert_ne!(String::from_utf8(different.stdout).unwrap().trim(), token);
}

#[test]
fn explicit_environment_token_takes_priority_without_writing_a_file() {
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let explicit = "X".repeat(32);
    let mut command = Command::new(env!("CARGO_BIN_EXE_novagpt"));
    command
        .arg("--root")
        .arg(root.path())
        .args(["token", "show"])
        .env("NOVAGPT_HTTP_TOKEN", &explicit);
    with_private_state(&mut command, private.path());
    let output = command.output().unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), explicit);
    assert!(fs::read_dir(private.path()).unwrap().next().is_none());
}

#[test]
fn weak_environment_override_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_novagpt"));
    command
        .arg("--root")
        .arg(root.path())
        .args(["token", "show"])
        .env("NOVAGPT_HTTP_TOKEN", "too-short");
    with_private_state(&mut command, private.path());
    let output = command.output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("token"));
    assert!(fs::read_dir(private.path()).unwrap().next().is_none());
}
