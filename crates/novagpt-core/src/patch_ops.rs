//! Guarded application of a single ordinary unified diff.
use crate::{NovaError, NovaResult, WorkspaceRegistry};
use serde::Serialize;
use std::{
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

const MAX_PATCH_BYTES: usize = 200_000;

#[derive(Debug, Serialize)]
pub struct PatchResult {
    pub paths: Vec<String>,
    pub changed: bool,
    pub bytes: u64,
}

fn patch_target(patch: &str) -> NovaResult<String> {
    if patch.len() > MAX_PATCH_BYTES || patch.is_empty() {
        return Err(NovaError::InvalidInput("Patch missing or too large".into()));
    }
    let mut old = None;
    let mut new = None;
    let mut hunk = false;
    for line in patch.lines() {
        if line.starts_with("diff --git ")
            || line.starts_with("rename ")
            || line.starts_with("copy ")
            || line.starts_with("GIT binary patch")
            || line.starts_with("new file mode")
            || line.starts_with("deleted file mode")
        {
            return Err(NovaError::InvalidInput(
                "Extended patches are not supported".into(),
            ));
        }
        if let Some(path) = line.strip_prefix("--- ") {
            if old.is_some() {
                return Err(NovaError::InvalidInput("Multiple patch targets".into()));
            }
            old = Some(
                path.strip_prefix("a/")
                    .ok_or_else(|| NovaError::InvalidInput("Expected a/ path".into()))?,
            );
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            if new.is_some() {
                return Err(NovaError::InvalidInput("Multiple patch targets".into()));
            }
            new = Some(
                path.strip_prefix("b/")
                    .ok_or_else(|| NovaError::InvalidInput("Expected b/ path".into()))?,
            );
        }
        if line.starts_with("@@ ") {
            hunk = true;
        }
    }
    match (old, new, hunk) {
        (Some(before), Some(after), true)
            if before == after && !before.is_empty() && !before.chars().any(|c| c.is_control()) =>
        {
            Ok(before.to_string())
        }
        _ => Err(NovaError::InvalidInput(
            "Expected one ordinary update diff with matching file paths".into(),
        )),
    }
}

fn apply(root: &Path, patch: &str, check: bool) -> NovaResult<()> {
    let mut command = Command::new("git");
    command
        .arg("-c")
        .arg("core.autocrlf=false")
        .arg("apply")
        .arg("--whitespace=nowarn");
    if check {
        command.arg("--check");
    }
    let mut child = command
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| NovaError::InvalidInput("Git executable unavailable".into()))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(patch.as_bytes())?;
    }
    let start = std::time::Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if start.elapsed() >= std::time::Duration::from_secs(15) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(NovaError::InvalidInput(
                "Patch application timed out".into(),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    if !status.success() {
        return Err(NovaError::InvalidInput(if check {
            "Patch does not apply cleanly; no changes made".into()
        } else {
            "Patch application failed; check file before retrying".into()
        }));
    }
    Ok(())
}

impl WorkspaceRegistry {
    pub fn apply_unified_patch(&self, id: Option<&str>, patch: &str) -> NovaResult<PatchResult> {
        let relative = patch_target(patch)?;
        let root = self.workspace(id)?.root;
        let target = self.resolve(id, &relative, true)?;
        self.with_write_lock(&target, || {
            self.resolve(id, &relative, true)?;
            apply(Path::new(&root), patch, true)?;
            apply(Path::new(&root), patch, false)?;
            Ok(PatchResult {
                paths: vec![relative],
                changed: true,
                bytes: std::fs::metadata(&target)?.len(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn patch_updates_one_file_and_refuses_conflict() {
        let temp = tempfile::tempdir().expect("temporary root");
        let state = WorkspaceRegistry::new(temp.path(), &[]).expect("registry");
        fs::write(temp.path().join("sample.txt"), "before\n").expect("fixture");
        let patch = "--- a/sample.txt\n+++ b/sample.txt\n@@ -1 +1 @@\n-before\n+after\n";
        assert!(state.apply_unified_patch(None, patch).is_ok());
        assert_eq!(
            fs::read_to_string(temp.path().join("sample.txt")).expect("read"),
            "after\n"
        );
        assert!(state.apply_unified_patch(None, patch).is_err());
    }

    #[test]
    fn rejects_escape_and_multiple_files() {
        let temp = tempfile::tempdir().expect("temporary root");
        let state = WorkspaceRegistry::new(temp.path(), &[]).expect("registry");
        let escape = "--- a/../outside\n+++ b/../outside\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(matches!(
            state.apply_unified_patch(None, escape),
            Err(NovaError::OutsideAllowedRoot)
        ));
        let multiple =
            "--- a/a\n+++ b/a\n@@ -1 +1 @@\n-a\n+b\n--- a/b\n+++ b/b\n@@ -1 +1 @@\n-a\n+b\n";
        assert!(state.apply_unified_patch(None, multiple).is_err());
    }
}
