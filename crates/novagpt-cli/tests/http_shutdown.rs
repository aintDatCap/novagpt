//! SIGTERM from the development supervisor must drain HTTP and clean up ngrok.
#![cfg(unix)]

use std::{
    fs,
    net::{TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

fn wait_for_exit(child: &mut Child) -> std::process::ExitStatus {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().expect("poll shutdown") {
            return status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("NovaGPT failed to stop after SIGTERM");
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn terminate(pid: u32) {
    let sent = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("send SIGTERM");
    assert!(sent.success(), "send SIGTERM");
}

#[test]
fn sigterm_stops_http_and_persists_shutdown_event() {
    let root = tempfile::tempdir().expect("workspace");
    let private = tempfile::tempdir().expect("private state");
    let socket = TcpListener::bind("127.0.0.1:0").expect("free port");
    let port = socket.local_addr().expect("port").port();
    drop(socket);

    let binary = env!("CARGO_BIN_EXE_novagpt");
    let mut child = Command::new(binary)
        .arg("--root")
        .arg(root.path())
        .args(["http", "--no-ngrok", "--port"])
        .arg(port.to_string())
        .env("NOVAGPT_HTTP_TOKEN", "Q".repeat(40))
        .env("XDG_STATE_HOME", private.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start NovaGPT HTTP");

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut ready = false;
    while Instant::now() < deadline {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            ready = true;
            break;
        }
        if child.try_wait().expect("poll HTTP").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if !ready {
        let _ = child.kill();
        let _ = child.wait();
        panic!("HTTP did not become ready");
    }

    terminate(child.id());
    let status = wait_for_exit(&mut child);
    assert!(status.success(), "graceful SIGTERM should exit cleanly");

    let audit = Command::new(binary)
        .arg("--root")
        .arg(root.path())
        .args(["logs", "tail"])
        .env("XDG_STATE_HOME", private.path())
        .output()
        .expect("read audit");
    assert!(audit.status.success());
    assert!(
        String::from_utf8_lossy(&audit.stdout).contains("http_server_stopped"),
        "shutdown event must have been flushed"
    );
}

#[test]
fn sigterm_during_ngrok_discovery_reaps_owned_tunnel_without_public_access() {
    let root = tempfile::tempdir().expect("workspace");
    let private = tempfile::tempdir().expect("private state");
    let mocks = tempfile::tempdir().expect("mock ngrok bin");
    let pidfile = private.path().join("ngrok.pid");
    let fake = mocks.path().join("ngrok");
    fs::write(
        &fake,
        "#!/bin/sh\nif [ \"$1\" = version ]; then echo 'ngrok version 3'; exit 0; fi\necho $$ > \"$NOVAGPT_MOCK_NGROK_PID\"\nexec sleep 60\n",
    )
    .expect("fake executable");
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o700)).expect("chmod");
    let path = format!(
        "{}:{}",
        mocks.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("unused port");
    let port = listener.local_addr().expect("port").port();
    drop(listener);

    let mut child = Command::new(env!("CARGO_BIN_EXE_novagpt"))
        .arg("--root")
        .arg(root.path())
        .args(["http", "--ngrok", "--port"])
        .arg(port.to_string())
        .env("PATH", path)
        .env("NOVAGPT_MOCK_NGROK_PID", &pidfile)
        .env("NOVAGPT_HTTP_TOKEN", "R".repeat(40))
        .env("XDG_STATE_HOME", private.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start NovaGPT HTTP with fake ngrok");

    let deadline = Instant::now() + Duration::from_secs(6);
    while !pidfile.exists() && Instant::now() < deadline {
        if child.try_wait().expect("poll startup").is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !pidfile.exists() {
        let _ = child.kill();
        let _ = child.wait();
        panic!("fake ngrok never started");
    }
    let tunnel_pid = fs::read_to_string(pidfile)
        .expect("read mock ngrok PID")
        .trim()
        .to_string();

    terminate(child.id());
    assert!(
        wait_for_exit(&mut child).success(),
        "clean startup shutdown"
    );
    let still_running = Command::new("kill")
        .args(["-0", &tunnel_pid])
        .status()
        .expect("inspect fake ngrok");
    assert!(
        !still_running.success(),
        "owned ngrok must be killed and reaped"
    );
}
