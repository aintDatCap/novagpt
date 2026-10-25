//! Git operations: argument-vector invocation, guarded paths, bounded output and shared index locking.
//! This is an INITIAL compatibility slice; dangerous actions are deliberately unavailable.
use crate::{NovaError, NovaResult, WorkspaceRegistry};
use serde::Serialize;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::process::Command;

const MAX_OUTPUT: usize = 64 * 1024;
const GIT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone, Debug, Default)]
pub struct GitRequest {
    pub workspace_id: Option<String>,
    pub action: String,
    pub path: Option<String>,
    pub staged: bool,
    pub include_diff: bool,
    pub count: Option<usize>,
    pub reference: Option<String>,
    pub name: Option<String>,
    pub start_point: Option<String>,
    pub create: bool,
    pub files: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct GitResult {
    pub action: String,
    pub args: Vec<String>,
    #[serde(rename = "exitCode")]
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

fn valid_ref(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 200
        && !value.starts_with('-')
        && !value.contains("..")
        && !value.contains("@{")
        && !value.ends_with('.')
        && !value.ends_with('/')
        && !value
            .chars()
            .any(|c| c.is_control() || matches!(c, ' ' | '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
}

fn limited_ref(value: &str) -> NovaResult<String> {
    if !valid_ref(value) {
        return Err(NovaError::InvalidInput(
            "Invalid Git ref or branch name".into(),
        ));
    }
    Ok(value.to_string())
}

impl WorkspaceRegistry {
    fn git_file_args(&self, request: &GitRequest) -> NovaResult<Vec<String>> {
        if request.files.is_empty() || request.files.len() > 200 {
            return Err(NovaError::InvalidInput(
                "Expected 1 to 200 explicit file paths".into(),
            ));
        }
        let root = self.workspace(request.workspace_id.as_deref())?.root;
        request
            .files
            .iter()
            .map(|file| {
                if file.trim().is_empty() || file == "." {
                    return Err(NovaError::InvalidInput(
                        "A specific file path is required".into(),
                    ));
                }
                let resolved = self.resolve(request.workspace_id.as_deref(), file, true)?;
                Ok(resolved
                    .strip_prefix(&root)
                    .map_err(|_| NovaError::OutsideAllowedRoot)?
                    .to_string_lossy()
                    .into_owned())
            })
            .collect()
    }

    fn git_scoped_path(&self, request: &GitRequest) -> NovaResult<Option<String>> {
        let Some(path) = request.path.as_deref() else {
            return Ok(None);
        };
        if path.is_empty() || path == "." {
            return Err(NovaError::InvalidInput(
                "A specific path is required".into(),
            ));
        }
        let root = self.workspace(request.workspace_id.as_deref())?.root;
        let resolved = self.resolve(request.workspace_id.as_deref(), path, false)?;
        Ok(Some(
            resolved
                .strip_prefix(&root)
                .map_err(|_| NovaError::OutsideAllowedRoot)?
                .to_string_lossy()
                .into_owned(),
        ))
    }

    pub async fn git_operation(&self, request: GitRequest) -> NovaResult<GitResult> {
        let root = self.workspace(request.workspace_id.as_deref())?.root;
        let action = request.action.clone();
        let mut args: Vec<String> = match action.as_str() {
            "status" => vec!["status", "--short", "--branch"],
            "current_branch" => vec!["branch", "--show-current"],
            "branches" => vec!["branch", "--format=%(refname:short)"],
            "log" => vec!["log", "--oneline", "--decorate", "--no-show-signature"],
            "show" => vec!["show", "--no-patch", "--format=fuller", "--no-show-signature"],
            "root" => vec!["rev-parse", "--show-toplevel"],
            "changed_files" => vec!["status", "--porcelain=v1"],
            "git_status" => vec!["status", "--short", "--branch"],
            "git_diff" | "show_changes" => vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv"],
            "blame" => vec!["blame"],
            "create_branch" => vec!["branch"],
            "switch_branch" => vec!["switch"],
            "stage" => vec!["add"],
            "unstage" => vec!["restore", "--staged"],
            "restore" | "commit" | "worktree_create" | "worktree_remove" | "worktree_list" =>
                return Err(NovaError::InvalidInput(format!(
                    "Git action {action} is disabled until ownership and destructive-operation checks are implemented"
                ))),
            _ => return Err(NovaError::InvalidInput("Unknown Git action".into())),
        }.into_iter().map(str::to_string).collect();

        let mut index_mutation = false;
        match action.as_str() {
            "status" | "git_status" | "git_diff" | "show_changes" => {
                if matches!(action.as_str(), "git_diff" | "show_changes") {
                    if request.include_diff
                        && let Some(scoped) = request.path.as_deref()
                    {
                        let target =
                            self.resolve(request.workspace_id.as_deref(), scoped, false)?;
                        if !target.is_file() {
                            return Err(NovaError::InvalidInput(
                                "Raw Git diff requires an existing safe regular file".into(),
                            ));
                        }
                    }
                    if request.staged {
                        args.push("--staged".into());
                    }
                    if !request.include_diff {
                        args.push("--stat".into());
                    }
                }
                if let Some(path) = self.git_scoped_path(&request)? {
                    args.extend(["--".into(), path]);
                } else if matches!(action.as_str(), "git_diff" | "show_changes") {
                    // Never disclose arbitrary tracked file contents (e.g. a committed .env).
                    // Require an explicit guard-checked path for raw patches.
                    if request.include_diff {
                        return Err(NovaError::InvalidInput(
                            "A specific guarded path is required to return Git patch contents; use include_diff=false for workspace-wide stats".into()
                        ));
                    }
                }
            }
            "log" => args.push(format!(
                "--max-count={}",
                request.count.unwrap_or(10).clamp(1, 100)
            )),
            "show" => args.push(limited_ref(request.reference.as_deref().unwrap_or("HEAD"))?),
            "blame" => {
                let path = self
                    .git_scoped_path(&request)?
                    .ok_or_else(|| NovaError::InvalidInput("blame needs path".into()))?;
                args.extend(["--".into(), path]);
            }
            "create_branch" => {
                let name = request
                    .name
                    .as_deref()
                    .ok_or_else(|| NovaError::InvalidInput("name is required".into()))?;
                args.push(limited_ref(name)?);
                if let Some(point) = request.start_point.as_deref() {
                    args.push(limited_ref(point)?);
                }
                index_mutation = true;
            }
            "switch_branch" => {
                let name = request
                    .name
                    .as_deref()
                    .ok_or_else(|| NovaError::InvalidInput("name is required".into()))?;
                if request.create {
                    args.push("-c".into());
                }
                args.push(limited_ref(name)?);
                index_mutation = true;
            }
            "stage" | "unstage" => {
                args.push("--".into());
                args.extend(self.git_file_args(&request)?);
                index_mutation = true;
            }
            _ => {}
        }

        // Independent reads can proceed; index/branch mutations serialize per root only.
        let _index_guard = if index_mutation {
            Some(self.git_mutation_lock(Path::new(&root)).await?)
        } else {
            None
        };
        run_git(Path::new(&root), &action, args).await
    }
}

async fn run_git(root: &Path, action: &str, args: Vec<String>) -> NovaResult<GitResult> {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg(if cfg!(windows) {
            "core.hooksPath=NUL"
        } else {
            "core.hooksPath=/dev/null"
        })
        .args(&args)
        .current_dir(root)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            if cfg!(windows) { "NUL" } else { "/dev/null" },
        )
        .env("GIT_PAGER", "cat")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|_| NovaError::InvalidInput("Git executable unavailable".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| NovaError::InvalidInput("Git stdout unavailable".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| NovaError::InvalidInput("Git stderr unavailable".into()))?;
    let captured = tokio::time::timeout(GIT_TIMEOUT, async {
        tokio::join!(
            crate::process_output::read_bounded(stdout, MAX_OUTPUT),
            crate::process_output::read_bounded(stderr, MAX_OUTPUT),
            child.wait()
        )
    })
    .await;

    let (out_result, err_result, status_result) = match captured {
        Ok(result) => result,
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            return Err(NovaError::InvalidInput("Git operation timed out".into()));
        }
    };
    let (out, out_truncated) = out_result.map_err(NovaError::Io)?;
    let (_err, err_truncated) = err_result.map_err(NovaError::Io)?;
    let exit_code = status_result?.code().unwrap_or(-1);
    if out_truncated || err_truncated {
        return Err(NovaError::TooLarge(MAX_OUTPUT));
    }
    if exit_code != 0 {
        return Err(NovaError::InvalidInput(format!(
            "Git operation failed with exit code {exit_code}"
        )));
    }
    Ok(GitResult {
        action: action.into(),
        args,
        exit_code,
        stdout: String::from_utf8_lossy(&out).trim().to_string(),
        stderr: if exit_code == 0 {
            String::new()
        } else {
            // Git stderr can contain arbitrary repository content or URLs with credentials.
            format!("Git exited with status {exit_code}")
        },
        truncated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command as StdCommand};

    fn git(root: &Path, args: &[&str]) {
        let result = StdCommand::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git executable");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[tokio::test]
    async fn guarded_status_diff_and_history() {
        let temp = tempfile::tempdir().expect("temp");
        git(temp.path(), &["init", "-q"]);
        git(
            temp.path(),
            &["config", "user.email", "fixture@example.test"],
        );
        git(temp.path(), &["config", "user.name", "Fixture"]);
        fs::write(temp.path().join("hello.txt"), "one\n").expect("write");
        git(temp.path(), &["add", "hello.txt"]);
        git(temp.path(), &["commit", "-qm", "baseline"]);
        fs::write(temp.path().join("hello.txt"), "two\n").expect("write");
        fs::write(temp.path().join(".env"), "TOP_SECRET_VALUE").expect("secret");
        let state = WorkspaceRegistry::new(temp.path(), &[]).expect("registry");
        let status = state
            .git_operation(GitRequest {
                action: "status".into(),
                ..Default::default()
            })
            .await
            .expect("status");
        assert!(status.stdout.contains("hello.txt"));
        let diff = state
            .git_operation(GitRequest {
                action: "git_diff".into(),
                path: Some("hello.txt".into()),
                include_diff: true,
                ..Default::default()
            })
            .await
            .expect("diff");
        assert!(diff.stdout.contains("+two"));
        assert!(!diff.stdout.contains("TOP_SECRET_VALUE"));
        assert!(
            state
                .git_operation(GitRequest {
                    action: "git_diff".into(),
                    path: Some(".env".into()),
                    include_diff: true,
                    ..Default::default()
                })
                .await
                .is_err()
        );
        assert!(
            state
                .git_operation(GitRequest {
                    action: "git_diff".into(),
                    include_diff: true,
                    ..Default::default()
                })
                .await
                .is_err()
        );
        let history = state
            .git_operation(GitRequest {
                action: "log".into(),
                ..Default::default()
            })
            .await
            .expect("log");
        assert!(history.stdout.contains("baseline"));
        let denied = state
            .git_operation(GitRequest {
                action: "commit".into(),
                ..Default::default()
            })
            .await;
        assert!(denied.is_err());
    }

    #[tokio::test]
    async fn mutations_reject_traversal_and_invalid_refs() {
        let temp = tempfile::tempdir().expect("temp");
        git(temp.path(), &["init", "-q"]);
        let state = WorkspaceRegistry::new(temp.path(), &[]).expect("registry");
        assert!(
            state
                .git_operation(GitRequest {
                    action: "stage".into(),
                    files: vec!["../escape".into()],
                    ..Default::default()
                })
                .await
                .is_err()
        );
        assert!(
            state
                .git_operation(GitRequest {
                    action: "create_branch".into(),
                    name: Some("-malicious".into()),
                    ..Default::default()
                })
                .await
                .is_err()
        );
        let branch = state
            .git_operation(GitRequest {
                action: "create_branch".into(),
                name: Some("feature".into()),
                ..Default::default()
            })
            .await;
        // An unborn repository cannot branch; the operation is a tool error.
        assert!(branch.is_err());
    }

    #[tokio::test]
    async fn concurrent_staging_and_branching_on_disposable_git_repository() {
        let temp = tempfile::tempdir().expect("temporary repository");
        git(temp.path(), &["init", "-q"]);
        git(
            temp.path(),
            &["config", "user.email", "fixture@example.test"],
        );
        git(temp.path(), &["config", "user.name", "Fixture"]);
        for file in ["one.txt", "two.txt"] {
            fs::write(temp.path().join(file), "original\n").expect("fixture file");
        }
        git(temp.path(), &["add", "."]);
        git(temp.path(), &["commit", "-qm", "baseline"]);
        for file in ["one.txt", "two.txt"] {
            fs::write(temp.path().join(file), "modified\n").expect("updated file");
        }

        let state = WorkspaceRegistry::new(temp.path(), &[]).expect("registry");
        let (one, two) = tokio::join!(
            state.git_operation(GitRequest {
                action: "stage".into(),
                files: vec!["one.txt".into()],
                ..Default::default()
            }),
            state.git_operation(GitRequest {
                action: "stage".into(),
                files: vec!["two.txt".into()],
                ..Default::default()
            }),
        );
        assert_eq!(one.expect("first staged").exit_code, 0);
        assert_eq!(two.expect("second staged").exit_code, 0);
        let status = state
            .git_operation(GitRequest {
                action: "status".into(),
                ..Default::default()
            })
            .await
            .expect("status");
        assert!(status.stdout.contains("one.txt"));
        assert!(status.stdout.contains("two.txt"));

        state
            .git_operation(GitRequest {
                action: "unstage".into(),
                files: vec!["one.txt".into()],
                ..Default::default()
            })
            .await
            .expect("unstage");
        let created = state
            .git_operation(GitRequest {
                action: "create_branch".into(),
                name: Some("feature/a".into()),
                ..Default::default()
            })
            .await
            .expect("create branch");
        assert_eq!(created.exit_code, 0);
        let switched = state
            .git_operation(GitRequest {
                action: "switch_branch".into(),
                name: Some("feature/a".into()),
                ..Default::default()
            })
            .await
            .expect("switch branch");
        assert_eq!(switched.exit_code, 0);
        let active = state
            .git_operation(GitRequest {
                action: "current_branch".into(),
                ..Default::default()
            })
            .await
            .expect("current");
        assert_eq!(active.stdout, "feature/a");
    }
}
