//! Explicit, consent-based ngrok installation through supported OS package managers.
//! No shell, auto-download scripts, or implicit privileged writes.
use std::{process::Stdio, time::Duration};
use tokio::process::Command;

pub struct InstallSpec {
    pub program: &'static str,
    pub args: &'static [&'static str],
}

/// Deterministic installer selection; no commands execute until the user agrees.
fn spec(platform: &str, has_manager: bool, has_elevation: bool) -> Option<InstallSpec> {
    match platform {
        "macos" if has_manager => Some(InstallSpec {
            program: "brew",
            args: &["install", "ngrok"],
        }),
        "windows" if has_manager => Some(InstallSpec {
            program: "winget",
            args: &[
                "install",
                "ngrok",
                "-s",
                "msstore",
                "--accept-source-agreements",
                "--accept-package-agreements",
            ],
        }),
        "linux" if has_manager && has_elevation => Some(InstallSpec {
            program: "pkexec",
            args: &["snap", "install", "ngrok"],
        }),
        _ => None,
    }
}

fn executable_available(program: &str) -> bool {
    std::process::Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

/// Which trusted manager is present on this system?
fn install_spec() -> Option<InstallSpec> {
    if cfg!(target_os = "macos") {
        spec("macos", executable_available("brew"), false)
    } else if cfg!(windows) {
        spec("windows", executable_available("winget"), false)
    } else if cfg!(target_os = "linux") {
        spec(
            "linux",
            executable_available("snap"),
            executable_available("pkexec"),
        )
    } else {
        None
    }
}

pub async fn available() -> bool {
    tokio::time::timeout(
        Duration::from_secs(3),
        Command::new("ngrok")
            .arg("version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .is_ok_and(|result| result.is_ok_and(|status| status.success()))
}

/// Installs only after a confirmed TUI action. Leaves account login to the user.
pub async fn install() -> Result<(), &'static str> {
    if available().await {
        return Ok(());
    }
    let plan = tokio::task::spawn_blocking(install_spec)
        .await
        .map_err(|_| "Unable to inspect system package managers.")?
        .ok_or("No supported package manager is available. Install ngrok from https://ngrok.com/download and restart NovaGPT.")?;
    let status = tokio::time::timeout(
        Duration::from_secs(180),
        Command::new(plan.program)
            .args(plan.args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .map_err(|_| "ngrok installation timed out; check your package manager.")?
    .map_err(|_| "Unable to start the selected package manager.")?;
    if !status.success() {
        return Err(
            "ngrok installation failed or was cancelled. Try installing it manually from https://ngrok.com/download.",
        );
    }
    if !available().await {
        return Err(
            "The package manager finished but ngrok is not on PATH. Restart your terminal or install it manually.",
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn installer_never_runs_without_a_supported_manager() {
        assert!(spec("linux", true, false).is_none());
        assert!(spec("linux", false, true).is_none());
        assert!(spec("macos", false, false).is_none());
        assert!(spec("windows", false, false).is_none());
        assert!(spec("other", true, true).is_none());
    }

    #[test]
    fn only_known_literal_commands_can_be_selected() {
        let mac = spec("macos", true, false).unwrap();
        assert_eq!((mac.program, mac.args), ("brew", &["install", "ngrok"][..]));
        let linux = spec("linux", true, true).unwrap();
        assert_eq!(
            (linux.program, linux.args),
            ("pkexec", &["snap", "install", "ngrok"][..])
        );
        let windows = spec("windows", true, false).unwrap();
        assert_eq!(windows.program, "winget");
        assert!(windows.args.contains(&"msstore"));
    }
}
