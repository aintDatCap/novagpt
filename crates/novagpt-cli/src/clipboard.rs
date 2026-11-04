//! Explicit clipboard access for public-facing connector URLs.
//! Secrets are passed only through child stdin, never argv, logs or the shell.
use std::{
    ffi::OsStr,
    io::{self, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Backend {
    program: &'static str,
    args: &'static [&'static str],
}

const WAYLAND: Backend = Backend {
    program: "wl-copy",
    args: &["--type", "text/plain"],
};
const XCLIP: Backend = Backend {
    program: "xclip",
    args: &["-selection", "clipboard", "-in"],
};
const XSEL: Backend = Backend {
    program: "xsel",
    args: &["--clipboard", "--input"],
};
const WSL: Backend = Backend {
    program: "clip.exe",
    args: &[],
};
const MACOS: Backend = Backend {
    program: "pbcopy",
    args: &[],
};

// Prefer the active desktop server. On GNOME Wayland, XWayland tools can be
// used if wl-clipboard is not installed; on X11, wl-copy may still be useful
// for unusual mixed sessions. WSL uses the Windows clipboard directly.
fn candidates(os: &str, wayland: bool, x11: bool, wsl: bool) -> Vec<Backend> {
    match os {
        "windows" => vec![WSL],
        "macos" => vec![MACOS],
        _ => {
            let mut commands = Vec::new();
            if wsl {
                commands.push(WSL);
            }
            if wayland {
                commands.push(WAYLAND);
            }
            if x11 {
                commands.extend([XCLIP, XSEL]);
            }
            // A Wayland compositor can coexist with XWayland, but do not
            // attempt graphics commands with no graphical session.
            commands
        }
    }
}

// Never read stdout/stderr of a clipboard helper: some helpers echo their
// input or print diagnostic context that can contain the credential.
fn run_command(program: &OsStr, args: &[&str], value: &str) -> io::Result<()> {
    let mut process = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    let written = match process.stdin.take() {
        Some(mut stdin) => stdin.write_all(value.as_bytes()),
        None => Err(io::Error::other("clipboard helper did not expose stdin")),
    };
    if let Err(error) = written {
        let _ = process.kill();
        let _ = process.wait();
        return Err(error);
    }
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match process.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(_)) => return Err(io::Error::other("clipboard helper failed")),
            Err(_) => {
                let _ = process.kill();
                let _ = process.wait();
                return Err(io::Error::other("clipboard helper failed"));
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                let _ = process.kill();
                let _ = process.wait();
                return Err(io::Error::other("clipboard helper timed out"));
            }
        }
    }
}

fn try_backends(
    value: &str,
    backends: &[Backend],
    mut run: impl FnMut(Backend, &str) -> io::Result<()>,
) -> bool {
    backends
        .iter()
        .copied()
        .any(|backend| run(backend, value).is_ok())
}

pub fn copy(value: &str) -> Result<(), String> {
    let os = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    };
    let backends = candidates(
        os,
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var_os("DISPLAY").is_some(),
        std::env::var_os("WSL_INTEROP").is_some() || std::env::var_os("WSL_DISTRO_NAME").is_some(),
    );
    if try_backends(value, &backends, |backend, input| {
        run_command(OsStr::new(backend.program), backend.args, input)
    }) {
        return Ok(());
    }
    // Avoid leaking the full URL even through error strings or diagnostics.
    let help = if os == "linux" {
        "Clipboard unavailable. On Ubuntu/Debian install wl-clipboard (Wayland) or xclip (X11); on Fedora install wl-clipboard/xclip, on Arch install wl-clipboard/xclip, or on openSUSE install wl-clipboard/xclip. Check your graphical session and try again."
    } else {
        "Clipboard unavailable. Check the system clipboard service and try again."
    };
    Err(help.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_clipboard_helpers_for_major_desktops() {
        assert_eq!(candidates("linux", true, false, false), vec![WAYLAND]);
        assert_eq!(candidates("linux", false, true, false), vec![XCLIP, XSEL]);
        assert_eq!(
            candidates("linux", true, true, false),
            vec![WAYLAND, XCLIP, XSEL]
        );
        assert_eq!(candidates("linux", false, false, true), vec![WSL]);
        assert!(candidates("linux", false, false, false).is_empty());
        assert_eq!(candidates("macos", false, false, false), vec![MACOS]);
        assert_eq!(candidates("windows", false, false, false), vec![WSL]);
    }

    #[test]
    fn failed_wayland_helper_falls_back_to_x11_without_exposing_token() {
        let token = "private-test-token";
        let mut tried = Vec::new();
        assert!(try_backends(
            token,
            &[WAYLAND, XCLIP, XSEL],
            |backend, value| {
                assert_eq!(value, token);
                tried.push(backend.program);
                if backend == XCLIP {
                    Ok(())
                } else {
                    Err(io::Error::other("not available"))
                }
            }
        ));
        assert_eq!(tried, vec!["wl-copy", "xclip"]);
        assert!(!try_backends(token, &[WAYLAND], |_backend, _value| {
            Err(io::Error::other("not available"))
        }));
    }

    #[cfg(unix)]
    #[test]
    fn helper_receives_private_url_on_stdin_without_logging_it() {
        use std::fs;
        let temp = tempfile::tempdir().unwrap();
        let output = temp.path().join("clipboard-result");
        // Execute a fixed shell command instead of writing and immediately
        // executing a temporary script (which can fail with ETXTBSY on CI).
        // Only the output path is in argv; the credential travels via stdin.
        let private_url = "https://example.org/mcp?novagpt_token=fake-private-test-value";
        run_command(
            OsStr::new("/bin/sh"),
            &["-c", "cat > \"$1\"", "sh", output.to_str().unwrap()],
            private_url,
        )
        .unwrap();
        assert_eq!(fs::read_to_string(&output).unwrap(), private_url);
    }

    #[cfg(unix)]
    #[test]
    fn failing_helper_returns_error_without_secret() {
        let secret = "do-not-log-this-credential";
        // Simulate a failing helper without creating a transient executable.
        let error = run_command(
            OsStr::new("/bin/sh"),
            &["-c", "cat >/dev/null; exit 1"],
            secret,
        )
        .unwrap_err();
        assert!(!error.to_string().contains(secret));
    }
}
