//! Bounded workspace inventory without reading user file contents.
use crate::{NovaResult, Workspace, WorkspaceRegistry};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Serialize)]
pub struct WorkspaceInspection {
    pub workspace_id: String,
    pub root: String,
    pub total_entries: usize,
    pub files: usize,
    pub directories: usize,
    pub truncated: bool,
    pub extension_counts: BTreeMap<String, usize>,
    pub manifests: Vec<String>,
    pub test_files: usize,
}

impl WorkspaceRegistry {
    pub fn list_workspaces(&self) -> NovaResult<Vec<Workspace>> {
        let mut known = self.inner_known_roots()?;
        known.sort_by(|a, b| a.workspace_id.cmp(&b.workspace_id));
        known.dedup_by(|a, b| a.workspace_id == b.workspace_id);
        Ok(known)
    }

    pub fn inspect_workspace(
        &self,
        id: Option<&str>,
        max_entries: usize,
    ) -> NovaResult<WorkspaceInspection> {
        let workspace = self.workspace(id)?;
        let cap = max_entries.clamp(1, 3000);
        let entries = self.tree(id, ".", 8, cap)?;
        let mut extension_counts = BTreeMap::new();
        let mut manifests = Vec::new();
        let mut files = 0;
        let mut directories = 0;
        let mut test_files = 0;

        for entry in &entries {
            if entry.kind == "directory" {
                directories += 1;
                continue;
            }
            files += 1;
            let path = std::path::Path::new(&entry.path);
            let extension = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("(none)")
                .to_ascii_lowercase();
            *extension_counts.entry(extension).or_insert(0) += 1;
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if matches!(
                name,
                "Cargo.toml"
                    | "package.json"
                    | "pyproject.toml"
                    | "go.mod"
                    | "Makefile"
                    | "CMakeLists.txt"
                    | "requirements.txt"
            ) && manifests.len() < 40
            {
                manifests.push(entry.path.clone());
            }
            if entry
                .path
                .split(['/', '\\'])
                .any(|part| part == "tests" || part == "__tests__")
                || name.ends_with("_test.rs")
                || name.ends_with(".test.ts")
            {
                test_files += 1;
            }
        }

        Ok(WorkspaceInspection {
            workspace_id: workspace.workspace_id,
            root: workspace.root,
            total_entries: entries.len(),
            files,
            directories,
            truncated: entries.len() == cap,
            extension_counts,
            manifests,
            test_files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn inventory_excludes_blocked_files_and_is_bounded() {
        let temp = tempfile::tempdir().expect("root");
        fs::create_dir(temp.path().join("tests")).expect("tests dir");
        fs::write(temp.path().join("Cargo.toml"), "[package]\n").expect("manifest");
        fs::write(temp.path().join("tests").join("basic_test.rs"), "assert").expect("test");
        fs::write(temp.path().join(".env"), "ignored").expect("blocked fixture");
        let registry = WorkspaceRegistry::new(temp.path(), &[]).expect("registry");
        let summary = registry.inspect_workspace(None, 20).expect("summary");
        assert_eq!(summary.files, 2);
        assert_eq!(summary.test_files, 1);
        assert_eq!(summary.manifests, ["Cargo.toml"]);
        assert_eq!(registry.list_workspaces().expect("roots").len(), 1);
        let capped = registry.inspect_workspace(None, 1).expect("cap");
        assert_eq!(capped.total_entries, 1);
        assert!(capped.truncated);
    }
}
