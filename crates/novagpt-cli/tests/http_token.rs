use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
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

fn set_by_pipe(root: &Path, home: &Path, value: &str, replace: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_novagpt"));
    command
        .arg("--root")
        .arg(root)
        .args(["token", "set", "--stdin"])
        .env_remove("NOVAGPT_HTTP_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if replace {
        command.arg("--replace");
    }
    with_private_state(&mut command, home);
    let mut child = command.spawn().expect("spawn secret input");
    child
        .stdin
        .take()
        .expect("secret input pipe")
        .write_all(value.as_bytes())
        .expect("send fixture token");
    child.wait_with_output().expect("finish token set")
}

#[test]
fn chosen_secret_is_shared_across_workspaces_and_never_echoed() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let first = "a".repeat(64);
    let replacement = "b".repeat(64);
    let written = set_by_pipe(root.path(), private.path(), &first, false);
    assert!(
        written.status.success(),
        "{}",
        String::from_utf8_lossy(&written.stderr)
    );
    assert!(!String::from_utf8_lossy(&written.stdout).contains(&first));
    assert!(!String::from_utf8_lossy(&written.stderr).contains(&first));
    assert_eq!(
        String::from_utf8(run(root.path(), private.path(), "show").stdout)
            .unwrap()
            .trim(),
        first
    );
    let other_token = run(other.path(), private.path(), "show");
    assert!(other_token.status.success());
    assert_eq!(String::from_utf8(other_token.stdout).unwrap().trim(), first);

    let denied = set_by_pipe(root.path(), private.path(), &replacement, false);
    assert!(
        !denied.status.success(),
        "replacing should require --replace"
    );
    assert!(!String::from_utf8_lossy(&denied.stderr).contains(&replacement));
    assert_eq!(
        String::from_utf8(run(root.path(), private.path(), "show").stdout)
            .unwrap()
            .trim(),
        first
    );
    let changed = set_by_pipe(root.path(), private.path(), &replacement, true);
    assert!(
        changed.status.success(),
        "{}",
        String::from_utf8_lossy(&changed.stderr)
    );
    assert!(!String::from_utf8_lossy(&changed.stdout).contains(&replacement));
    assert_eq!(
        String::from_utf8(run(root.path(), private.path(), "show").stdout)
            .unwrap()
            .trim(),
        replacement
    );
    assert_eq!(
        String::from_utf8(run(other.path(), private.path(), "show").stdout)
            .unwrap()
            .trim(),
        replacement
    );
    assert!(!root.path().join(".novagpt").exists());
}

#[test]
fn malformed_piped_secret_never_replaces_saved_value() {
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let first = "d".repeat(64);
    assert!(
        set_by_pipe(root.path(), private.path(), &first, false)
            .status
            .success()
    );
    for invalid in ["short".to_string(), "z".repeat(64)] {
        let output = set_by_pipe(root.path(), private.path(), &invalid, true);
        assert!(!output.status.success());
    }
    let multiline = format!("{}\nextra\n", "a".repeat(64));
    assert!(
        !set_by_pipe(root.path(), private.path(), &multiline, true)
            .status
            .success()
    );
    assert_eq!(
        String::from_utf8(run(root.path(), private.path(), "show").stdout)
            .unwrap()
            .trim(),
        first
    );
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
fn manually_set_secret_authenticates_real_http_server() {
    use std::{
        net::{TcpListener, TcpStream},
        thread,
        time::Duration,
    };
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let chosen = "b".repeat(64);
    assert!(
        set_by_pipe(root.path(), private.path(), &chosen, false)
            .status
            .success()
    );
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_novagpt"));
    cmd.arg("--root")
        .arg(root.path())
        .arg("http")
        .arg("--port")
        .arg(port.to_string())
        .env_remove("NOVAGPT_HTTP_TOKEN")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    with_private_state(&mut cmd, private.path());
    let mut child = ProcessGuard(cmd.spawn().expect("start real MCP HTTP process"));
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
    assert!(response_status(port, None).contains("401"));
    assert!(response_status(port, Some(&"a".repeat(64))).contains("401"));
    assert!(!response_status(port, Some(&chosen)).contains("401"));
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
fn token_show_is_stable_per_user_and_kept_outside_git_tree() {
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
    assert_eq!(String::from_utf8(different.stdout).unwrap().trim(), token);
    assert_eq!(
        String::from_utf8(run(other.path(), private.path(), "path").stdout)
            .unwrap()
            .trim(),
        location
    );
    assert_eq!(file.file_name().unwrap(), "http.token");
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

fn create_legacy_token(home: &Path, legacy_name: char, value: &str) -> std::path::PathBuf {
    let app = home.join("NovaGPT");
    let tokens = app.join("tokens");
    fs::create_dir_all(&tokens).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&app, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&tokens, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let old = tokens.join(format!("{}.token", legacy_name.to_string().repeat(64)));
    fs::write(&old, value).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&old, fs::Permissions::from_mode(0o600)).unwrap();
    }
    old
}

#[test]
fn one_legacy_token_is_preserved_and_removed_from_old_location() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let previous = "1".repeat(64);
    let legacy = create_legacy_token(private.path(), 'a', &previous);
    let unrelated = private
        .path()
        .join("NovaGPT")
        .join("tokens")
        .join("keep.txt");
    fs::write(&unrelated, "keep").unwrap();
    for root in [&first, &second] {
        let output = run(root.path(), private.path(), "show");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), previous);
    }
    assert!(!legacy.exists());
    assert_eq!(fs::read_to_string(unrelated).unwrap(), "keep");
    assert_eq!(
        fs::read_to_string(private.path().join("NovaGPT/tokens/http.token")).unwrap(),
        previous
    );
}

#[test]
fn multiple_legacy_tokens_are_discarded_for_one_new_token() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let a = "1".repeat(64);
    let b = "2".repeat(64);
    let old_a = create_legacy_token(private.path(), 'a', &a);
    let old_b = create_legacy_token(private.path(), 'b', &b);
    let shown = run(first.path(), private.path(), "show");
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    assert!(String::from_utf8_lossy(&shown.stderr).contains("ATTENZIONE"));
    let current = String::from_utf8(shown.stdout).unwrap().trim().to_owned();
    assert_eq!(current.len(), 64);
    assert_ne!(current, a);
    assert_ne!(current, b);
    assert!(!old_a.exists());
    assert!(!old_b.exists());
    assert_eq!(
        String::from_utf8(run(second.path(), private.path(), "show").stdout)
            .unwrap()
            .trim(),
        current
    );
}

#[test]
fn existing_shared_token_wins_if_previous_cleanup_was_interrupted() {
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let first = run(root.path(), private.path(), "show");
    assert!(first.status.success());
    let shared = String::from_utf8(first.stdout).unwrap().trim().to_owned();
    let old = create_legacy_token(private.path(), 'd', &"3".repeat(64));
    let current = run(root.path(), private.path(), "show");
    assert!(current.status.success());
    assert_eq!(String::from_utf8(current.stdout).unwrap().trim(), shared);
    assert!(!old.exists());
}

#[test]
fn migration_refuses_invalid_legacy_files_without_deleting_them() {
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let old = create_legacy_token(private.path(), 'a', "invalid");
    let shown = run(root.path(), private.path(), "show");
    assert!(!shown.status.success());
    assert_eq!(fs::read_to_string(old).unwrap(), "invalid");
    assert!(!private.path().join("NovaGPT/tokens/http.token").exists());
}

#[cfg(unix)]
#[test]
fn migration_refuses_symlinks_without_touching_their_targets() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let old = create_legacy_token(private.path(), 'a', &"a".repeat(64));
    fs::remove_file(&old).unwrap();
    let outside = private.path().join("other");
    fs::write(&outside, "must remain unchanged").unwrap();
    symlink(&outside, &old).unwrap();
    assert!(!run(root.path(), private.path(), "show").status.success());
    assert_eq!(
        fs::read_to_string(outside).unwrap(),
        "must remain unchanged"
    );
    assert!(!private.path().join("NovaGPT/tokens/http.token").exists());
}

#[test]
fn log_directories_remain_workspace_specific_and_do_not_create_a_token() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let mut paths = Vec::new();
    for root in [&first, &second] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_novagpt"));
        command
            .arg("--root")
            .arg(root.path())
            .args(["logs", "path"]);
        with_private_state(&mut command, private.path());
        let output = command.output().unwrap();
        assert!(output.status.success());
        paths.push(String::from_utf8(output.stdout).unwrap());
    }
    assert_ne!(paths[0], paths[1]);
    assert!(!private.path().join("NovaGPT/tokens/http.token").exists());
}

#[test]
fn concurrent_cli_processes_reuse_one_shared_token() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let private = tempfile::tempdir().unwrap();
    let outputs = std::thread::scope(|scope| {
        let left = scope.spawn(|| run(first.path(), private.path(), "show"));
        let right = scope.spawn(|| run(second.path(), private.path(), "show"));
        [left.join().unwrap(), right.join().unwrap()]
    });
    assert!(outputs.iter().all(|o| o.status.success()));
    assert_eq!(outputs[0].stdout, outputs[1].stdout);
    assert_eq!(
        fs::read_to_string(private.path().join("NovaGPT/tokens/http.token"))
            .unwrap()
            .trim(),
        String::from_utf8(outputs[0].stdout.clone()).unwrap().trim()
    );
}

#[test]
fn token_management_does_not_require_a_valid_workspace() {
    let private = tempfile::tempdir().unwrap();
    let absent = private.path().join("nonexistent-workspace");
    let output = run(&absent, private.path(), "show");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let token = String::from_utf8(output.stdout).unwrap();
    assert_eq!(token.trim().len(), 64);
    let output = run(&absent, private.path(), "path");
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("http.token")
    );
}
