//! Bounded local shell execution with a fixed resource budget.
//! Browser operations are independent of shell permissions.
use crate::{NovaError, NovaResult, WorkspaceRegistry};
use serde::Serialize;
use std::{
    path::Path,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::process::Command;

const MAX_OUTPUT: usize = 64 * 1024;

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

fn make_command(shell: &str, command: &str) -> NovaResult<(Command, String)> {
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
    pub async fn run_bash(&self, request: BashRequest) -> NovaResult<BashResult> {
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

        let (mut process, selected_shell) = make_command(shell, &request.command)?;
        process
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("CARGO_TERM_COLOR", "never")
            .env("NO_COLOR", "1")
            .env_remove("NOVAGPT_HTTP_TOKEN");

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
            command: "[redacted shell command]".into(),
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
    async fn shell_rejects_working_directory_outside_approved_roots() {
        let root = tempfile::tempdir().expect("workspace");
        let registry = WorkspaceRegistry::new(root.path(), &[]).expect("registry");
        let outside = registry
            .run_bash(BashRequest {
                workspace_id: None,
                command: "echo fixture".into(),
                shell: None,
                cwd: Some("..".into()),
                timeout_ms: None,
            })
            .await;
        assert!(matches!(outside, Err(NovaError::OutsideAllowedRoot)));
    }

    #[tokio::test]
    async fn shell_executes_commands_without_a_runtime_mode() {
        let home = tempfile::tempdir().expect("workspace");
        let registry = WorkspaceRegistry::new(home.path(), &[]).expect("registry");
        let (shell, command) = if cfg!(windows) {
            ("powershell", "Write-Output NOVAGPT_FULL_SHELL_FIXTURE")
        } else {
            ("bash", "printf NOVAGPT_FULL_SHELL_FIXTURE")
        };
        let result = registry
            .run_bash(BashRequest {
                workspace_id: None,
                command: command.into(),
                shell: Some(shell.into()),
                cwd: None,
                timeout_ms: Some(30_000),
            })
            .await
            .expect("shell command");
        assert_eq!(result.exit_code, Some(0));
        assert!(result.stdout.contains("NOVAGPT_FULL_SHELL_FIXTURE"));
        assert_eq!(result.command, "[redacted shell command]");
    }
}
