//! Bounded shell execution with an explicit startup-granted mode.
//! Safe mode invokes a tiny fixed development-command allow-list without a shell.
use crate::{NovaError, NovaResult, WorkspaceRegistry};
use serde::Serialize;
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::process::Command;

const MAX_OUTPUT: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BashMode {
    Off,
    #[default]
    Safe,
    Full,
}

#[derive(Clone, Debug)]
pub struct BashRequest {
    pub workspace_id: Option<String>,
    pub command: String,
    pub shell: Option<String>,
    pub cwd: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct BashResult {
    pub command: String,
    pub cwd: String,
    pub shell: String,
    #[serde(rename = "exitCode")]
    pub exit_code: Option<i32>,
    #[serde(rename = "durationMs")]
    pub duration_ms: u128,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub termination_reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_resource_id: Option<String>,
}

fn safe_argv(command: &str) -> NovaResult<Vec<&'static str>> {
    let argv: &[&str] = match command.trim() {
        "cargo --version" => &["cargo", "--version"],
        "rustc --version" => &["rustc", "--version"],
        "cargo fmt --all -- --check" => &["cargo", "fmt", "--all", "--", "--check"],
        "cargo clippy --workspace --all-targets -- -D warnings" => &["cargo", "clippy", "--workspace", "--all-targets", "--", "-D", "warnings"],
        "cargo check --workspace" => &["cargo", "check", "--workspace"],
        "cargo test --workspace" => &["cargo", "test", "--workspace"],
        "cargo build --release -p novagpt-cli" => &["cargo", "build", "--release", "-p", "novagpt-cli"],
        _ => return Err(NovaError::InvalidInput(
            "Command denied in safe mode: only fixed Rust quality-gate commands are permitted; structured Git/file tools handle other operations".into()
        )),
    };
    Ok(argv.to_vec())
}

fn make_command(shell: &str, command: &str, mode: BashMode) -> NovaResult<(Command, String)> {
    if mode == BashMode::Safe {
        let argv = safe_argv(command)?;
        let mut process = Command::new(argv[0]);
        process.args(&argv[1..]);
        return Ok((process, "direct-safe".into()));
    }
    let selected = if shell == "auto" {
        if cfg!(windows) { "powershell" } else { "bash" }
    } else {
        shell
    };
    let (program, arguments): (&str, Vec<&str>) = match selected {
        "powershell" => (
            "powershell",
            vec!["-NoProfile", "-NonInteractive", "-Command", command],
        ),
        "cmd" if cfg!(windows) => ("cmd", vec!["/D", "/S", "/C", command]),
        "bash" => ("bash", vec!["-lc", command]),
        "wsl" if cfg!(windows) => ("wsl", vec!["--exec", "bash", "-lc", command]),
        _ => {
            return Err(NovaError::InvalidInput(
                "Shell unavailable on this platform".into(),
            ));
        }
    };
    let mut process = Command::new(program);
    process.args(arguments);
    Ok((process, selected.into()))
}

impl WorkspaceRegistry {
    pub async fn run_bash(&self, mode: BashMode, request: BashRequest) -> NovaResult<BashResult> {
        if mode == BashMode::Off {
            return Err(NovaError::InvalidInput(
                "Bash tool is disabled by server configuration".into(),
            ));
        }
        if request.command.is_empty()
            || request.command.len() > 4096
            || request.command.contains('\0')
        {
            return Err(NovaError::InvalidInput(
                "Invalid command length or content".into(),
            ));
        }
        let workspace = self.workspace(request.workspace_id.as_deref())?;
        let cwd = self.resolve(
            request.workspace_id.as_deref(),
            request.cwd.as_deref().unwrap_or("."),
            false,
        )?;
        if !cwd.is_dir() {
            return Err(NovaError::InvalidInput(
                "Working directory must be an existing directory".into(),
            ));
        }
        let shell = request.shell.as_deref().unwrap_or("auto");
        if !["auto", "bash", "powershell", "cmd", "wsl"].contains(&shell) {
            return Err(NovaError::InvalidInput("Invalid shell selection".into()));
        }
        let timeout_ms = request.timeout_ms.unwrap_or(30_000);
        if !(1_000..=600_000).contains(&timeout_ms) {
            return Err(NovaError::InvalidInput(
                "Timeout must be 1000–600000 ms".into(),
            ));
        }

        let (mut process, selected_shell) = make_command(shell, &request.command, mode)?;
        process
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("CARGO_TERM_COLOR", "never")
            .env("NO_COLOR", "1")
            .env_remove("NOVAGPT_HTTP_TOKEN");
        if mode == BashMode::Safe {
            // Cargo build scripts still run with host permissions, but need not
            // inherit bearer tokens, cloud keys or unrelated process secrets.
            process.env_clear();
            for key in [
                "PATH",
                "Path",
                "SystemRoot",
                "WINDIR",
                "TEMP",
                "TMP",
                "TMPDIR",
                "HOME",
                "USERPROFILE",
                "HOMEDRIVE",
                "HOMEPATH",
                "APPDATA",
                "LOCALAPPDATA",
                "CARGO_HOME",
                "RUSTUP_HOME",
                "RUSTUP_TOOLCHAIN",
            ] {
                if let Some(value) = std::env::var_os(key) {
                    process.env(key, value);
                }
            }
            process
                .env("CARGO_TERM_COLOR", "never")
                .env("NO_COLOR", "1");
        }

        let start = Instant::now();
        let mut child = process.spawn().map_err(|_| {
            NovaError::InvalidInput("Requested shell or executable unavailable".into())
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| NovaError::InvalidInput("Process stdout unavailable".into()))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| NovaError::InvalidInput("Process stderr unavailable".into()))?;
        let captured = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
            tokio::join!(
                crate::process_output::read_bounded(stdout, MAX_OUTPUT),
                crate::process_output::read_bounded(stderr, MAX_OUTPUT),
                child.wait()
            )
        })
        .await;
        let (exit_code, termination_reason, out, err, truncated) = match captured {
            Ok((Ok((out, ot)), Ok((err, et)), Ok(status))) => {
                (status.code(), "exit", out, err, ot || et)
            }
            Ok(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(NovaError::InvalidInput(
                    "Unable to collect command output".into(),
                ));
            }
            Err(_) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                (None, "timeout", Vec::new(), Vec::new(), false)
            }
        };

        Ok(BashResult {
            // Do not echo arbitrary command text: full-mode commands may contain secrets.
            command: if mode == BashMode::Safe {
                request.command
            } else {
                "[redacted full-mode command]".into()
            },
            cwd: Path::new(&workspace.root)
                .strip_prefix(&workspace.root)
                .unwrap_or(Path::new("."))
                .join(
                    cwd.strip_prefix(&workspace.root)
                        .map_err(|_| NovaError::OutsideAllowedRoot)?,
                )
                .display()
                .to_string(),
            shell: selected_shell,
            exit_code,
            duration_ms: start.elapsed().as_millis(),
            stdout: String::from_utf8_lossy(&out).into_owned(),
            stderr: String::from_utf8_lossy(&err).into_owned(),
            truncated,
            termination_reason: termination_reason.into(),
            output_resource_id: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn safe_mode_denies_shell_mischief_and_traversal() {
        let home = tempfile::tempdir().expect("workspace");
        let registry = WorkspaceRegistry::new(home.path(), &[]).expect("registry");
        for evil in [
            "cargo test --workspace; echo secret",
            "powershell -enc Z",
            "cargo run",
            "rm -rf .",
            "cargo test --workspace && pwd",
        ] {
            let denied = registry
                .run_bash(
                    BashMode::Safe,
                    BashRequest {
                        workspace_id: None,
                        command: evil.into(),
                        shell: None,
                        cwd: None,
                        timeout_ms: None,
                    },
                )
                .await;
            assert!(denied.is_err(), "should deny {evil}");
        }
        let outside = registry
            .run_bash(
                BashMode::Safe,
                BashRequest {
                    workspace_id: None,
                    command: "cargo --version".into(),
                    shell: None,
                    cwd: Some("..".into()),
                    timeout_ms: None,
                },
            )
            .await;
        assert!(matches!(outside, Err(NovaError::OutsideAllowedRoot)));
    }

    #[tokio::test]
    async fn safe_mode_can_run_bounded_tool() {
        let home = tempfile::tempdir().expect("workspace");
        let registry = WorkspaceRegistry::new(home.path(), &[]).expect("registry");
        let result = registry
            .run_bash(
                BashMode::Safe,
                BashRequest {
                    workspace_id: None,
                    command: "cargo --version".into(),
                    shell: None,
                    cwd: None,
                    timeout_ms: Some(30_000),
                },
            )
            .await
            .expect("run command");
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.starts_with("cargo "));
        assert!(!result.truncated);
    }
}
