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
    pub message: Option<String>,
    pub discard_changes: bool,
    pub worktree_id: Option<String>,
    pub branch: Option<String>,
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

pub(crate) fn limited_ref(value: &str) -> NovaResult<String> {
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

    async fn verify_staged_paths(&self, request: &GitRequest) -> NovaResult<()> {
        let root = self.workspace(request.workspace_id.as_deref())?.root;
        let staged = run_git(
            Path::new(&root),
            "staged_names",
            vec![
                "diff".into(),
                "--cached".into(),
                "--name-only".into(),
                "-z".into(),
            ],
        )
        .await?;
        if staged.stdout.contains('\u{fffd}') {
            return Err(NovaError::InvalidInput(
                "Unsupported staged path encoding".into(),
            ));
        }
        let mut count = 0;
        for name in staged.stdout.split('\0').filter(|path| !path.is_empty()) {
            // A commit cannot bypass the protected-path policy via pre-existing index changes.
            self.resolve(request.workspace_id.as_deref(), name, false)?;
            count += 1;
        }
        if count == 0 {
            return Err(NovaError::InvalidInput("No staged files to commit".into()));
        }
        Ok(())
    }

    pub async fn git_operation(&self, request: GitRequest) -> NovaResult<GitResult> {
        let root = self.workspace(request.workspace_id.as_deref())?.root;
        let action = request.action.clone();
        if matches!(
            action.as_str(),
            "worktree_create" | "worktree_remove" | "worktree_list"
        ) {
            return self.worktree_action(request).await;
        }
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
            "restore" => vec!["restore", "--worktree"],
            "commit" => vec!["commit", "--no-verify", "-m"],
            "worktree_create" | "worktree_remove" | "worktree_list" =>
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
            "stage" | "unstage" | "restore" => {
                if action == "restore" && !request.discard_changes {
                    return Err(NovaError::InvalidInput(
                        "restore requires discard_changes=true".into(),
                    ));
                }
                args.push("--".into());
                args.extend(self.git_file_args(&request)?);
                index_mutation = true;
            }
            "commit" => {
                let message = request
                    .message
                    .as_deref()
                    .ok_or_else(|| NovaError::InvalidInput("commit message required".into()))?;
                if message.trim().is_empty()
                    || message.len() > 200
                    || message.chars().any(|c| c.is_control())
                {
                    return Err(NovaError::InvalidInput(
                        "commit message must be one bounded line".into(),
                    ));
                }
                args.push(message.into());
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
        if action == "commit" {
            self.verify_staged_paths(&request).await?;
        }
        let mut result = run_git(Path::new(&root), &action, args).await?;
        if action == "commit" {
            result.args = vec!["commit".into(), "[redacted-message]".into()];
            result.stdout = "Commit created".into();
        }
        Ok(result)
    }
}

fn git_cwd(root: &Path) -> std::path::PathBuf {
    #[cfg(windows)]
    {
        let value = root.to_string_lossy();
        if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
            return std::path::PathBuf::from(format!(r"\\{}", rest));
        }
        if let Some(rest) = value.strip_prefix(r"\\?\") {
            return std::path::PathBuf::from(rest);
        }
    }
    root.to_path_buf()
}

// Preserve standard user Git configuration (identity, signing, hooks).
// Tool-level path validation takes place before this helper.
fn git_command(root: &Path, args: &[String]) -> Command {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(git_cwd(root))
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

pub(crate) async fn run_git(root: &Path, action: &str, args: Vec<String>) -> NovaResult<GitResult> {
    let mut child = git_command(root, &args)
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

    #[tokio::test]
    async fn git_inherits_custom_global_identity_instead_of_disabling_it() {
        let temp = tempfile::tempdir().expect("fixture root");
        let repo = temp.path().join("repository");
        fs::create_dir(&repo).expect("repo directory");
        git(&repo, &["init", "-q"]);
        let global = temp.path().join("global.gitconfig");
        fs::write(
            &global,
            "[user]\n    name = Nova Global Fixture\n    email = global@example.invalid\n",
        )
        .expect("isolated global config");
        fs::write(repo.join("file.txt"), "fixture\n").expect("fixture file");
        git(&repo, &["add", "file.txt"]);
        // Verify inherited global identity with a real commit, without
        // changing or depending on the machine's real ~/.gitconfig.
        let mut command = git_command(
            &repo,
            &["commit".into(), "-m".into(), "from global config".into()],
        );
        command
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_AUTHOR_NAME")
            .env_remove("GIT_AUTHOR_EMAIL")
            .env_remove("GIT_COMMITTER_NAME")
            .env_remove("GIT_COMMITTER_EMAIL");
        let result = command.output().await.expect("run real git commit");
        assert!(
            result.status.success(),
            "Git should use the supplied global identity, exit {:?}",
            result.status.code()
        );
        let mut verify = git_command(
            &repo,
            &["log".into(), "-1".into(), "--format=%an <%ae>".into()],
        );
        verify
            .env("GIT_CONFIG_GLOBAL", &global)
            .env("GIT_CONFIG_NOSYSTEM", "1");
        let identity = verify.output().await.expect("read resulting identity");
        assert!(identity.status.success());
        assert_eq!(
            String::from_utf8_lossy(&identity.stdout).trim(),
            "Nova Global Fixture <global@example.invalid>"
        );
    }

    #[tokio::test]
    async fn commit_and_restore_require_explicit_safe_targets() {
        let temp = tempfile::tempdir().expect("root");
        git(temp.path(), &["init", "-q"]);
        // Keep line-ending fixtures deterministic even when the user's global
        // Git config enables core.autocrlf on Windows.
        git(temp.path(), &["config", "core.autocrlf", "false"]);
        git(
            temp.path(),
            &["config", "user.email", "fixture@example.test"],
        );
        git(temp.path(), &["config", "user.name", "Fixture"]);
        fs::write(temp.path().join("file.txt"), "initial\n").expect("initial");
        git(temp.path(), &["add", "file.txt"]);
        git(temp.path(), &["commit", "-qm", "baseline"]);
        let state = WorkspaceRegistry::new(temp.path(), &[]).expect("registry");

        fs::write(temp.path().join("file.txt"), "changed\n").expect("update");
        assert!(
            state
                .git_operation(GitRequest {
                    action: "restore".into(),
                    files: vec!["file.txt".into()],
                    ..Default::default()
                })
                .await
                .is_err()
        );
        state
            .git_operation(GitRequest {
                action: "restore".into(),
                files: vec!["file.txt".into()],
                discard_changes: true,
                ..Default::default()
            })
            .await
            .expect("explicit restore");
        assert_eq!(
            fs::read_to_string(temp.path().join("file.txt")).expect("read"),
            "initial\n"
        );

        fs::write(temp.path().join("file.txt"), "committed\n").expect("update");
        state
            .git_operation(GitRequest {
                action: "stage".into(),
                files: vec!["file.txt".into()],
                ..Default::default()
            })
            .await
            .expect("stage");
        let commit = state
            .git_operation(GitRequest {
                action: "commit".into(),
                message: Some("bounded test commit".into()),
                ..Default::default()
            })
            .await
            .expect("commit");
        assert_eq!(commit.stdout, "Commit created");
        assert!(!format!("{commit:?}").contains("bounded test commit"));
        let log = state
            .git_operation(GitRequest {
                action: "log".into(),
                ..Default::default()
            })
            .await
            .expect("history");
        assert!(log.stdout.contains("bounded test commit"));

        fs::write(temp.path().join("other.txt"), "safe").expect("other");
        fs::write(temp.path().join(".env"), "protected").expect("blocked fixture");
        git(temp.path(), &["add", ".env"]);
        git(temp.path(), &["add", "other.txt"]);
        let denied = state
            .git_operation(GitRequest {
                action: "commit".into(),
                message: Some("deny protected staging".into()),
                ..Default::default()
            })
            .await;
        assert!(matches!(denied, Err(NovaError::BlockedPath)));
    }
}
