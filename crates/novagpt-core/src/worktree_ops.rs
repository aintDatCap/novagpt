//! Explicitly owned disposable Git worktrees. Never removes an unregistered path,
//! and never force-removes dirty worktrees. State is shared per server process.
use crate::{GitRequest, GitResult, NovaError, NovaResult, WorkspaceRegistry};
use serde::Serialize;
use std::path::Path;

#[derive(Clone, Debug, Serialize)]
pub struct OwnedWorktree {
    pub workspace_id: String,
    pub id: String,
    pub path: String,
    pub branch: String,
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 48
        && id
            .bytes()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'_')
        && id.as_bytes()[0].is_ascii_alphanumeric()
}

impl WorkspaceRegistry {
    async fn owned_worktree_home(&self, workspace_id: &str) -> NovaResult<std::path::PathBuf> {
        let raw_temp = std::env::temp_dir();
        let base = tokio::fs::canonicalize(&raw_temp).await?;
        let home = raw_temp.join(format!("novagpt-owned-worktrees-{workspace_id}"));
        tokio::fs::create_dir_all(&home).await?;
        let canonical = tokio::fs::canonicalize(&home).await?;
        if canonical.parent() != Some(base.as_path()) {
            return Err(NovaError::OutsideAllowedRoot);
        }
        // Git for Windows rejects some extended-length canonical path spellings.
        // The canonical-parent check above protects the location; pass the ordinary
        // temp-dir spelling when invoking Git as a child process.
        Ok(home)
    }

    pub async fn worktree_action(&self, request: GitRequest) -> NovaResult<GitResult> {
        let ws = self.workspace(request.workspace_id.as_deref())?;
        let action = request.action.clone();
        let root = Path::new(&ws.root);
        let _lock = self.git_mutation_lock(root).await?;
        if action == "worktree_list" {
            let records = self.owned_worktrees(&ws.workspace_id)?;
            let stdout = records
                .iter()
                .map(|record| format!("{}\t{}\t{}", record.id, record.branch, record.path))
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(GitResult {
                action,
                args: vec!["worktree", "list-owned"]
                    .into_iter()
                    .map(str::to_string)
                    .collect(),
                exit_code: 0,
                stdout,
                stderr: String::new(),
                truncated: false,
            });
        }
        let id = request.worktree_id.as_deref().unwrap_or("");
        if !valid_id(id) {
            return Err(NovaError::InvalidInput(
                "worktree_id must be 1..48 safe ASCII characters".into(),
            ));
        }
        match action.as_str() {
            "worktree_create" => {
                if self.owned_worktree(&ws.workspace_id, id)?.is_some() {
                    return Err(NovaError::InvalidInput("Worktree ID already owned".into()));
                }
                let branch = request
                    .branch
                    .as_deref()
                    .map(crate::git_ops::limited_ref)
                    .transpose()?
                    .unwrap_or_else(|| format!("novagpt/{id}"));
                let home = self.owned_worktree_home(&ws.workspace_id).await?;
                let target = home.join(id);
                if target.exists() {
                    return Err(NovaError::InvalidInput(
                        "Worktree path already exists".into(),
                    ));
                }
                if tokio::fs::symlink_metadata(&target).await.is_ok() {
                    return Err(NovaError::InvalidInput(
                        "Existing worktree target is not owned".into(),
                    ));
                }
                let result = crate::git_ops::run_git(
                    root,
                    &action,
                    vec![
                        "worktree".into(),
                        "add".into(),
                        "-b".into(),
                        branch.clone(),
                        target.to_string_lossy().into_owned(),
                        "HEAD".into(),
                    ],
                )
                .await?;
                self.register_worktree(OwnedWorktree {
                    workspace_id: ws.workspace_id,
                    id: id.into(),
                    path: target.to_string_lossy().into_owned(),
                    branch,
                })?;
                Ok(result)
            }
            "worktree_remove" => {
                let record = self.owned_worktree(&ws.workspace_id, id)?.ok_or_else(|| {
                    NovaError::InvalidInput("Unowned worktree cannot be removed".into())
                })?;
                let home = self.owned_worktree_home(&ws.workspace_id).await?;
                let target = home.join(id);
                if target.to_string_lossy() != record.path
                    || tokio::fs::symlink_metadata(&target)
                        .await
                        .map(|m| m.file_type().is_symlink())
                        .unwrap_or(true)
                {
                    return Err(NovaError::InvalidInput(
                        "Owned worktree path changed".into(),
                    ));
                }
                let result = crate::git_ops::run_git(
                    root,
                    &action,
                    vec!["worktree".into(), "remove".into(), record.path.clone()],
                )
                .await?;
                self.unregister_worktree(&ws.workspace_id, id)?;
                Ok(result)
            }
            _ => Err(NovaError::InvalidInput(
                "Unsupported worktree operation".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, process::Command};

    fn git(root: &Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[tokio::test]
    async fn worktrees_owned_deny_unowned_and_dirty_removal() {
        let tmp = tempfile::tempdir().expect("root");
        git(tmp.path(), &["init", "-q"]);
        git(
            tmp.path(),
            &["config", "user.email", "fixture@example.test"],
        );
        git(tmp.path(), &["config", "user.name", "Fixture"]);
        fs::write(tmp.path().join("one.txt"), "one").expect("fixture");
        git(tmp.path(), &["add", "one.txt"]);
        git(tmp.path(), &["commit", "-qm", "baseline"]);
        let state = WorkspaceRegistry::new(tmp.path(), &[]).expect("workspace");
        let make = || GitRequest {
            action: "worktree_create".into(),
            worktree_id: Some("unit".into()),
            ..Default::default()
        };
        let created = state.git_operation(make()).await;
        assert!(created.is_ok(), "{created:?}");
        assert!(state.git_operation(make()).await.is_err());
        let list = state
            .git_operation(GitRequest {
                action: "worktree_list".into(),
                ..Default::default()
            })
            .await
            .expect("list");
        assert!(list.stdout.contains("unit"));
        assert!(
            state
                .git_operation(GitRequest {
                    action: "worktree_remove".into(),
                    worktree_id: Some("unknown".into()),
                    ..Default::default()
                })
                .await
                .is_err()
        );
        let target = std::path::PathBuf::from(list.stdout.split('\t').nth(2).expect("record path"));
        fs::write(target.join("dirty.txt"), "dirty").expect("dirty fixture");
        assert!(
            state
                .git_operation(GitRequest {
                    action: "worktree_remove".into(),
                    worktree_id: Some("unit".into()),
                    ..Default::default()
                })
                .await
                .is_err()
        );
        fs::remove_file(target.join("dirty.txt")).expect("cleanup fixture");
        state
            .git_operation(GitRequest {
                action: "worktree_remove".into(),
                worktree_id: Some("unit".into()),
                ..Default::default()
            })
            .await
            .expect("remove");
        assert!(!target.exists());
        fs::remove_dir(target.parent().expect("owned home")).expect("remove empty test home");
        assert!(
            !state
                .git_operation(GitRequest {
                    action: "worktree_list".into(),
                    ..Default::default()
                })
                .await
                .expect("list")
                .stdout
                .contains("unit")
        );
    }
}
