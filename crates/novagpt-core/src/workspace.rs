use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum NovaError {
    #[error("Access outside an allowed workspace")]
    OutsideAllowedRoot,
    #[error("Path is blocked by safety rules")]
    BlockedPath,
    #[error("A symlink cannot be a write target")]
    SymlinkWrite,
    #[error("Unknown workspace_id: {0}. Open the intended root first")]
    UnknownWorkspace(String),
    #[error("File changed since the supplied expected_sha256")]
    StaleWrite,
    #[error("Expected exactly one replacement; found {0}")]
    ReplacementCount(usize),
    #[error("Invalid UTF-8 text")]
    InvalidText,
    #[error("File is too large; limit is {0} bytes")]
    TooLarge(usize),
    #[error("Cannot read binary content as text")]
    BinaryFile,
    #[error("Invalid input: {0}")]
    InvalidInput(String),
    #[error("Operation unavailable because shared state was poisoned")]
    StatePoisoned,
    #[error("Filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
}

pub type NovaResult<T> = Result<T, NovaError>;

#[derive(Clone, Debug, serde::Serialize)]
pub struct Workspace {
    pub workspace_id: String,
    pub root: String,
}

struct SharedState {
    default_root: PathBuf,
    allowed_roots: Vec<PathBuf>,
    known: Mutex<HashMap<String, PathBuf>>,
    write_locks: Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>,
    git_locks: Mutex<HashMap<PathBuf, Weak<tokio::sync::RwLock<()>>>>,
    worktrees: Mutex<HashMap<(String, String), crate::worktree_ops::OwnedWorktree>>,
}

pub(crate) struct GitMutationGuard {
    _common: tokio::sync::OwnedRwLockWriteGuard<()>,
    _tree: tokio::sync::OwnedRwLockWriteGuard<()>,
}

#[derive(Clone)]
pub struct WorkspaceRegistry {
    inner: Arc<SharedState>,
}

impl WorkspaceRegistry {
    pub fn new(root: impl AsRef<Path>, extra_allowed: &[PathBuf]) -> NovaResult<Self> {
        let default_root = fs::canonicalize(root)?;
        if !default_root.is_dir() {
            return Err(NovaError::InvalidInput(
                "Workspace root must be a directory".into(),
            ));
        }
        let mut allowed_roots = vec![default_root.clone()];
        for candidate in extra_allowed {
            let allowed = fs::canonicalize(candidate)?;
            if !allowed.is_dir() {
                return Err(NovaError::InvalidInput(
                    "Allowed root must be a directory".into(),
                ));
            }
            if !allowed_roots.contains(&allowed) {
                allowed_roots.push(allowed);
            }
        }
        let id = id_for_root(&default_root);
        let known = HashMap::from([(id, default_root.clone())]);
        Ok(Self {
            inner: Arc::new(SharedState {
                default_root,
                allowed_roots,
                known: Mutex::new(known),
                write_locks: Mutex::new(HashMap::new()),
                git_locks: Mutex::new(HashMap::new()),
                worktrees: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn default_workspace(&self) -> Workspace {
        self.describe(&self.inner.default_root)
    }

    pub fn open_workspace(&self, root: impl AsRef<Path>) -> NovaResult<Workspace> {
        let canonical = fs::canonicalize(root)?;
        if !canonical.is_dir() || !self.is_allowed(&canonical) {
            return Err(NovaError::OutsideAllowedRoot);
        }
        let result = self.describe(&canonical);
        self.inner
            .known
            .lock()
            .map_err(|_| NovaError::StatePoisoned)?
            .insert(result.workspace_id.clone(), canonical);
        Ok(result)
    }

    pub fn reconnect_workspace(&self, id: &str, root: Option<&str>) -> NovaResult<Workspace> {
        if id.trim().is_empty() {
            return Err(NovaError::InvalidInput("workspace_id is required".into()));
        }
        let ws = if let Some(original_root) = root {
            let canonical = fs::canonicalize(original_root)?;
            if !canonical.is_dir() || !self.is_allowed(&canonical) {
                return Err(NovaError::OutsideAllowedRoot);
            }
            let expected = self.describe(&canonical);
            if expected.workspace_id != id {
                return Err(NovaError::InvalidInput(
                    "Workspace ID does not match supplied root".into(),
                ));
            }
            self.open_workspace(&canonical)?
        } else {
            self.workspace(Some(id))?
        };
        Ok(ws)
    }

    pub fn workspace(&self, id: Option<&str>) -> NovaResult<Workspace> {
        let Some(id) = id.filter(|v| !v.is_empty()) else {
            return Ok(self.default_workspace());
        };
        let known = self
            .inner
            .known
            .lock()
            .map_err(|_| NovaError::StatePoisoned)?;
        if let Some(root) = known.get(id)
            && root.is_dir()
            && self.is_allowed(root)
            && id_for_root(root) == id
        {
            return Ok(self.describe(root));
        }
        // Allowed roots can be recovered across transport lifetimes without
        // learning any implicit "currently selected" workspace.
        self.inner
            .allowed_roots
            .iter()
            .find(|root| id_for_root(root) == id)
            .map(|root| self.describe(root))
            .ok_or_else(|| NovaError::UnknownWorkspace(id.into()))
    }

    pub fn resolve(
        &self,
        workspace_id: Option<&str>,
        input: &str,
        write: bool,
    ) -> NovaResult<PathBuf> {
        let ws = self.workspace(workspace_id)?;
        let root = PathBuf::from(ws.root);
        let input = if input.is_empty() { "." } else { input };
        let path = Path::new(input);
        let target = lexical_clean(if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        });
        if !target.starts_with(&root) {
            return Err(NovaError::OutsideAllowedRoot);
        }
        let relative = target
            .strip_prefix(&root)
            .map_err(|_| NovaError::OutsideAllowedRoot)?;
        if blocked(relative) {
            return Err(NovaError::BlockedPath);
        }

        // Canonicalize the existing ancestor, including intermediary symlinks.
        let mut ancestor = target.as_path();
        while !ancestor.exists() {
            ancestor = ancestor.parent().ok_or(NovaError::OutsideAllowedRoot)?;
        }
        let resolved_ancestor = fs::canonicalize(ancestor)?;
        if !resolved_ancestor.starts_with(&root) {
            return Err(NovaError::OutsideAllowedRoot);
        }
        if let Ok(real_target) = fs::canonicalize(&target) {
            if !real_target.starts_with(&root) {
                return Err(NovaError::OutsideAllowedRoot);
            }
            let real_relative = real_target
                .strip_prefix(&root)
                .map_err(|_| NovaError::OutsideAllowedRoot)?;
            if blocked(real_relative) {
                return Err(NovaError::BlockedPath);
            }
        }
        if write
            && target
                .symlink_metadata()
                .is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(NovaError::SymlinkWrite);
        }
        Ok(target)
    }

    // Canonical existing ancestor plus the missing suffix gives aliases the
    // same lock identity, including targets that do not yet exist.
    fn canonical_target(target: &Path) -> NovaResult<PathBuf> {
        let mut ancestor = target;
        while !ancestor.exists() {
            ancestor = ancestor.parent().ok_or(NovaError::OutsideAllowedRoot)?;
        }
        Ok(fs::canonicalize(ancestor)?.join(
            target
                .strip_prefix(ancestor)
                .map_err(|_| NovaError::OutsideAllowedRoot)?,
        ))
    }

    pub(crate) fn repository_root(&self, target: &Path) -> NovaResult<Option<PathBuf>> {
        for ancestor in target.ancestors() {
            if ancestor.join(".git").exists() {
                return Ok(Some(ancestor.to_path_buf()));
            }
        }
        Ok(None)
    }

    fn worktree_gate(&self, root: &Path) -> NovaResult<Arc<tokio::sync::RwLock<()>>> {
        let mut table = self
            .inner
            .git_locks
            .lock()
            .map_err(|_| NovaError::StatePoisoned)?;
        table.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = table.get(root).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(tokio::sync::RwLock::new(()));
        table.insert(root.to_path_buf(), Arc::downgrade(&lock));
        Ok(lock)
    }

    pub(crate) fn with_write_lock<T>(
        &self,
        target: &Path,
        operation: impl FnOnce() -> NovaResult<T>,
    ) -> NovaResult<T> {
        let identity = Self::canonical_target(target)?;
        let worktree = self
            .repository_root(&identity)?
            .map(|root| self.worktree_gate(&root))
            .transpose()?;
        // Lock order: worktree (shared), then canonical target (exclusive).
        let _worktree_guard = worktree.as_ref().map(|gate| gate.blocking_read());
        let lock = {
            let mut table = self
                .inner
                .write_locks
                .lock()
                .map_err(|_| NovaError::StatePoisoned)?;
            table.retain(|_, lock| lock.strong_count() > 0);
            if let Some(lock) = table.get(&identity).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(Mutex::new(()));
                table.insert(identity.clone(), Arc::downgrade(&lock));
                lock
            }
        };
        let _guard = lock.lock().map_err(|_| NovaError::StatePoisoned)?;
        if Self::canonical_target(target)? != identity {
            return Err(NovaError::StaleWrite);
        }
        operation()
    }

    pub(crate) async fn git_mutation_lock(&self, root: &Path) -> NovaResult<GitMutationGuard> {
        let root = tokio::fs::canonicalize(root).await?;
        let registry = self.clone();
        let scan_root = root.clone();
        let repo = tokio::task::spawn_blocking(move || registry.repository_root(&scan_root))
            .await
            .map_err(|_| NovaError::StatePoisoned)??;
        let repo = repo.ok_or_else(|| NovaError::InvalidInput("Git worktree required".into()))?;
        if !self.is_allowed(&repo) {
            return Err(NovaError::OutsideAllowedRoot);
        }
        // Linked worktrees have distinct worktree gates but share Git refs.
        let common = crate::git_ops::run_git(
            &repo,
            "common_dir",
            vec![
                "rev-parse".into(),
                "--path-format=absolute".into(),
                "--git-common-dir".into(),
            ],
        )
        .await?;
        let common = tokio::fs::canonicalize(common.stdout.trim_end_matches(['\r', '\n'])).await?;
        let common_gate = self.worktree_gate(&common)?;
        let tree_gate = self.worktree_gate(&repo)?;
        // Git locks shared repository metadata first, then the selected worktree.
        let common = common_gate.write_owned().await;
        let tree = tree_gate.write_owned().await;
        Ok(GitMutationGuard {
            _common: common,
            _tree: tree,
        })
    }

    pub(crate) fn inner_known_roots(&self) -> NovaResult<Vec<Workspace>> {
        let known = self
            .inner
            .known
            .lock()
            .map_err(|_| NovaError::StatePoisoned)?;
        Ok(known
            .values()
            .filter(|root| root.is_dir() && self.is_allowed(root))
            .map(|root| self.describe(root))
            .collect())
    }

    pub(crate) fn owned_worktree(
        &self,
        workspace_id: &str,
        id: &str,
    ) -> NovaResult<Option<crate::worktree_ops::OwnedWorktree>> {
        let table = self
            .inner
            .worktrees
            .lock()
            .map_err(|_| NovaError::StatePoisoned)?;
        Ok(table
            .get(&(workspace_id.to_string(), id.to_string()))
            .cloned())
    }

    pub(crate) fn owned_worktrees(
        &self,
        workspace_id: &str,
    ) -> NovaResult<Vec<crate::worktree_ops::OwnedWorktree>> {
        let table = self
            .inner
            .worktrees
            .lock()
            .map_err(|_| NovaError::StatePoisoned)?;
        Ok(table
            .values()
            .filter(|record| record.workspace_id == workspace_id)
            .cloned()
            .collect())
    }

    pub(crate) fn register_worktree(
        &self,
        record: crate::worktree_ops::OwnedWorktree,
    ) -> NovaResult<()> {
        let mut table = self
            .inner
            .worktrees
            .lock()
            .map_err(|_| NovaError::StatePoisoned)?;
        let key = (record.workspace_id.clone(), record.id.clone());
        if table.contains_key(&key) {
            return Err(NovaError::InvalidInput(
                "Worktree is already registered".into(),
            ));
        }
        table.insert(key, record);
        Ok(())
    }

    pub(crate) fn unregister_worktree(&self, workspace_id: &str, id: &str) -> NovaResult<()> {
        let mut table = self
            .inner
            .worktrees
            .lock()
            .map_err(|_| NovaError::StatePoisoned)?;
        table.remove(&(workspace_id.to_string(), id.to_string()));
        Ok(())
    }

    pub(crate) fn max_read_bytes(&self) -> usize {
        180_000
    }
    pub(crate) fn max_write_bytes(&self) -> usize {
        1_000_000
    }

    fn is_allowed(&self, candidate: &Path) -> bool {
        self.inner
            .allowed_roots
            .iter()
            .any(|allowed| candidate.starts_with(allowed))
    }

    fn describe(&self, root: &Path) -> Workspace {
        Workspace {
            workspace_id: id_for_root(root),
            root: root.to_string_lossy().into_owned(),
        }
    }
}

fn id_for_root(root: &Path) -> String {
    let hash = Sha256::digest(root.to_string_lossy().as_bytes());
    format!("ws_{}", &hex::encode(hash)[..24])
}

fn lexical_clean(path: PathBuf) -> PathBuf {
    let mut output = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                output.pop();
            }
            other => output.push(other.as_os_str()),
        }
    }
    output
}

pub(crate) fn blocked(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        matches!(
            name.as_str(),
            ".git"
                | ".ssh"
                | ".gnupg"
                | ".env"
                | ".codexpro"
                | ".novagpt"
                | "node_modules"
                | "target"
                | "id_rsa"
                | "id_ed25519"
                | "credentials.json"
                | "secrets.json"
        ) || name.starts_with(".env.")
            || name.ends_with(".pem")
            || name.ends_with(".key")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn nested_workspace_and_file_writes_share_repository_gate() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let nested = dir.path().join("nested");
        fs::create_dir(&nested).unwrap();
        let registry = WorkspaceRegistry::new(dir.path(), &[]).unwrap();
        let canonical = fs::canonicalize(dir.path()).unwrap();
        let guard = registry.git_mutation_lock(&nested).await.unwrap();
        let gate = registry.worktree_gate(&canonical).unwrap();
        assert!(gate.try_read().is_err());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
        let target = nested.join("file");
        let copy = registry.clone();
        let task = tokio::task::spawn_blocking(move || {
            started_tx.send(()).unwrap();
            copy.with_write_lock(&target, || {
                done_tx.send(()).unwrap();
                Ok(())
            })
            .unwrap();
        });
        started_rx.await.unwrap();
        assert!(done_rx.try_recv().is_err());
        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(2), done_rx)
            .await
            .unwrap()
            .unwrap();
        task.await.unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn canonical_aliases_share_lock_identity() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        assert_eq!(
            WorkspaceRegistry::canonical_target(&real.join("new")).unwrap(),
            WorkspaceRegistry::canonical_target(&alias.join("new")).unwrap()
        );
    }

    #[test]
    fn stable_workspace_ids_and_default_isolation() {
        let tmp = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let registry = WorkspaceRegistry::new(tmp.path(), &[other.path().to_path_buf()]).unwrap();
        let default = registry.default_workspace();
        let alternate = registry.open_workspace(other.path()).unwrap();
        assert_ne!(default.workspace_id, alternate.workspace_id);
        assert_eq!(
            registry.workspace(None).unwrap().workspace_id,
            default.workspace_id
        );
        assert_eq!(
            registry
                .workspace(Some(&alternate.workspace_id))
                .unwrap()
                .workspace_id,
            alternate.workspace_id
        );
    }

    #[test]
    fn reconnect_rejects_spoofed_ids_and_preserves_default() {
        let base = tempfile::tempdir().expect("default");
        let alternate = tempfile::tempdir().expect("alternate");
        let excluded = tempfile::tempdir().expect("outside");
        let state = WorkspaceRegistry::new(base.path(), &[alternate.path().to_path_buf()])
            .expect("workspace");
        let default = state.default_workspace();
        let other = state
            .open_workspace(alternate.path())
            .expect("open approved");
        assert_eq!(
            state
                .reconnect_workspace(&other.workspace_id, Some(&other.root))
                .expect("reconnect")
                .workspace_id,
            other.workspace_id
        );
        assert!(
            state
                .reconnect_workspace(&default.workspace_id, Some(&other.root))
                .is_err()
        );
        assert!(state.reconnect_workspace("ws_unknown", None).is_err());
        assert!(
            state
                .reconnect_workspace(
                    &default.workspace_id,
                    Some(excluded.path().to_str().expect("UTF-8 test fixture"))
                )
                .is_err()
        );
        assert_eq!(
            state.workspace(None).expect("default").workspace_id,
            default.workspace_id
        );
    }

    #[test]
    fn traversal_and_blocked_paths_fail() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = WorkspaceRegistry::new(tmp.path(), &[]).unwrap();
        assert!(matches!(
            registry.resolve(None, "../escape.txt", true),
            Err(NovaError::OutsideAllowedRoot)
        ));
        assert!(matches!(
            registry.resolve(None, ".env", true),
            Err(NovaError::BlockedPath)
        ));
        assert!(matches!(
            registry.resolve(None, ".git/config", false),
            Err(NovaError::BlockedPath)
        ));
        assert!(matches!(
            registry.resolve(None, ".novagpt/config.json", false),
            Err(NovaError::BlockedPath)
        ));
    }
}
