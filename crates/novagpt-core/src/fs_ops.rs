use crate::workspace::{NovaError, NovaResult, WorkspaceRegistry, blocked};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
};

#[derive(Debug, Serialize)]
pub struct FileDocument {
    pub path: String,
    pub text: String,
    pub sha256: String,
    pub bytes: usize,
}

#[derive(Debug, Serialize)]
pub struct FileReceipt {
    pub path: String,
    pub sha256: String,
    pub bytes: usize,
}

#[derive(Debug, Serialize)]
pub struct TreeEntry {
    pub path: String,
    pub kind: String,
}

#[derive(Debug, Serialize)]
pub struct SearchHit {
    pub path: String,
    pub line: usize,
    pub text: String,
}

pub struct FileWriteRequest<'a> {
    pub workspace_id: Option<&'a str>,
    pub path: &'a str,
    pub content: &'a str,
    pub expected_sha256: Option<&'a str>,
    pub create_dirs: bool,
}

pub struct EditRequest<'a> {
    pub workspace_id: Option<&'a str>,
    pub path: &'a str,
    pub old_text: &'a str,
    pub new_text: &'a str,
    pub expected_sha256: Option<&'a str>,
    pub replace_all: bool,
}

impl WorkspaceRegistry {
    pub fn read_file(&self, id: Option<&str>, relative: &str) -> NovaResult<FileDocument> {
        let root = self.workspace(id)?.root;
        let path = self.resolve(id, relative, false)?;
        let cap = self.max_read_bytes();
        if fs::metadata(&path)?.len() > cap as u64 {
            return Err(NovaError::TooLarge(cap));
        }
        let mut bytes = Vec::new();
        fs::File::open(&path)?
            .take((cap + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > cap {
            return Err(NovaError::TooLarge(cap));
        }
        if bytes.contains(&0) {
            return Err(NovaError::BinaryFile);
        }
        let text = String::from_utf8(bytes.clone()).map_err(|_| NovaError::InvalidText)?;
        Ok(FileDocument {
            path: path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string(),
            sha256: hex_sha(&bytes),
            bytes: bytes.len(),
            text,
        })
    }

    pub fn write_file(&self, request: FileWriteRequest<'_>) -> NovaResult<FileReceipt> {
        if request.content.len() > self.max_write_bytes() {
            return Err(NovaError::TooLarge(self.max_write_bytes()));
        }
        let path = self.resolve(request.workspace_id, request.path, true)?;
        self.with_write_lock(&path, || {
            // Recheck after acquiring the per-file lock.
            self.resolve(request.workspace_id, request.path, true)?;
            if path.exists() {
                let current = fs::read(&path)?;
                check_hash(&current, request.expected_sha256)?;
            } else if request.expected_sha256.is_some() {
                return Err(NovaError::StaleWrite);
            }
            if request.create_dirs
                && let Some(parent) = path.parent()
            {
                fs::create_dir_all(parent)?;
            }
            // Windows-compatible bounded write. Replacement atomicity is not
            // yet at CodexPro parity: track as pending, never claim otherwise.
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            file.write_all(request.content.as_bytes())?;
            file.sync_all()?;
            self.receipt(request.workspace_id, &path, request.content.as_bytes())
        })
    }

    pub fn edit_file(&self, request: EditRequest<'_>) -> NovaResult<FileReceipt> {
        if request.old_text.is_empty() {
            return Err(NovaError::InvalidInput("old_text must not be empty".into()));
        }
        let path = self.resolve(request.workspace_id, request.path, true)?;
        self.with_write_lock(&path, || {
            self.resolve(request.workspace_id, request.path, true)?;
            let current = fs::read(&path)?;
            check_hash(&current, request.expected_sha256)?;
            let text = String::from_utf8(current).map_err(|_| NovaError::InvalidText)?;
            let occurrences = text.matches(request.old_text).count();
            if !request.replace_all && occurrences != 1 {
                return Err(NovaError::ReplacementCount(occurrences));
            }
            if request.replace_all && occurrences == 0 {
                return Err(NovaError::ReplacementCount(0));
            }
            let updated = if request.replace_all {
                text.replace(request.old_text, request.new_text)
            } else {
                text.replacen(request.old_text, request.new_text, 1)
            };
            if updated.len() > self.max_write_bytes() {
                return Err(NovaError::TooLarge(self.max_write_bytes()));
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)?;
            file.write_all(updated.as_bytes())?;
            file.sync_all()?;
            self.receipt(request.workspace_id, &path, updated.as_bytes())
        })
    }

    fn receipt(
        &self,
        workspace_id: Option<&str>,
        path: &Path,
        contents: &[u8],
    ) -> NovaResult<FileReceipt> {
        let root = self.workspace(workspace_id)?.root;
        Ok(FileReceipt {
            path: path
                .strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string(),
            sha256: hex_sha(contents),
            bytes: contents.len(),
        })
    }

    pub fn tree(
        &self,
        id: Option<&str>,
        relative: &str,
        max_depth: usize,
        max_entries: usize,
    ) -> NovaResult<Vec<TreeEntry>> {
        let base = self.resolve(id, relative, false)?;
        let root = self.workspace(id)?.root;
        let mut result = Vec::new();
        walk(
            &base,
            Path::new(&root),
            max_depth.min(8),
            max_entries.min(3000),
            &mut result,
        )?;
        Ok(result)
    }

    pub fn search(
        &self,
        id: Option<&str>,
        text: &str,
        max_results: usize,
    ) -> NovaResult<Vec<SearchHit>> {
        if text.is_empty() {
            return Err(NovaError::InvalidInput("query is required".into()));
        }
        let root = self.workspace(id)?.root;
        let mut paths = Vec::new();
        walk(Path::new(&root), Path::new(&root), 12, 15_000, &mut paths)?;
        let mut hits = Vec::new();
        for item in paths {
            if item.kind != "file" {
                continue;
            }
            let target = self.resolve(id, &item.path, false)?;
            if fs::metadata(&target).is_ok_and(|m| m.len() > self.max_read_bytes() as u64) {
                continue;
            }
            if let Ok(bytes) = fs::read(&target) {
                if bytes.contains(&0) {
                    continue;
                }
                if let Ok(contents) = String::from_utf8(bytes) {
                    for (n, line) in contents.lines().enumerate() {
                        if line.contains(text) {
                            hits.push(SearchHit {
                                path: item.path.clone(),
                                line: n + 1,
                                text: line.chars().take(400).collect(),
                            });
                            if hits.len() >= max_results.clamp(1, 2000) {
                                return Ok(hits);
                            }
                        }
                    }
                }
            }
        }
        Ok(hits)
    }
}

fn hex_sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn check_hash(content: &[u8], expected: Option<&str>) -> NovaResult<()> {
    if let Some(expected) = expected
        && hex_sha(content) != expected
    {
        return Err(NovaError::StaleWrite);
    }
    Ok(())
}

fn walk(
    path: &Path,
    root: &Path,
    depth: usize,
    max: usize,
    result: &mut Vec<TreeEntry>,
) -> NovaResult<()> {
    if depth == 0 || result.len() >= max {
        return Ok(());
    }
    let entries = fs::read_dir(path)?;
    for entry in entries {
        if result.len() >= max {
            break;
        }
        let entry = entry?;
        let target = entry.path();
        let relative = target
            .strip_prefix(root)
            .map_err(|_| NovaError::OutsideAllowedRoot)?;
        if blocked(relative) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            continue;
        }
        if kind.is_file() {
            result.push(TreeEntry {
                path: relative.display().to_string(),
                kind: "file".into(),
            });
        } else if kind.is_dir() {
            result.push(TreeEntry {
                path: relative.display().to_string(),
                kind: "directory".into(),
            });
            walk(&target, root, depth - 1, max, result)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stale_sha_does_not_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let r = WorkspaceRegistry::new(tmp.path(), &[]).unwrap();
        let first = r
            .write_file(FileWriteRequest {
                workspace_id: None,
                path: "a.txt",
                content: "one",
                expected_sha256: None,
                create_dirs: true,
            })
            .unwrap();
        let edited = r
            .edit_file(EditRequest {
                workspace_id: None,
                path: "a.txt",
                old_text: "one",
                new_text: "two",
                expected_sha256: Some(&first.sha256),
                replace_all: false,
            })
            .unwrap();
        assert_eq!(edited.bytes, 3);
        assert_eq!(edited.sha256, r.read_file(None, "a.txt").unwrap().sha256);
        assert!(matches!(
            r.write_file(FileWriteRequest {
                workspace_id: None,
                path: "a.txt",
                content: "lost",
                expected_sha256: Some(&first.sha256),
                create_dirs: true
            }),
            Err(NovaError::StaleWrite)
        ));
        assert_eq!(r.read_file(None, "a.txt").unwrap().text, "two");
    }

    #[test]
    fn two_concurrent_writers_cannot_both_use_stale_hash() {
        use std::sync::{Arc, Barrier};
        let tmp = tempfile::tempdir().unwrap();
        let registry = WorkspaceRegistry::new(tmp.path(), &[]).unwrap();
        let initial = registry
            .write_file(FileWriteRequest {
                workspace_id: None,
                path: "shared.txt",
                content: "start",
                expected_sha256: None,
                create_dirs: true,
            })
            .unwrap();
        let gate = Arc::new(Barrier::new(3));
        let mut workers = Vec::new();
        for candidate in ["alice", "bob"] {
            let state = registry.clone();
            let gate = Arc::clone(&gate);
            let sha = initial.sha256.clone();
            workers.push(std::thread::spawn(move || {
                gate.wait();
                state.write_file(FileWriteRequest {
                    workspace_id: None,
                    path: "shared.txt",
                    content: candidate,
                    expected_sha256: Some(&sha),
                    create_dirs: true,
                })
            }));
        }
        gate.wait();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(NovaError::StaleWrite)))
                .count(),
            1
        );
        assert!(
            ["alice", "bob"].contains(
                &registry
                    .read_file(None, "shared.txt")
                    .unwrap()
                    .text
                    .as_str()
            )
        );
    }

    #[test]
    fn large_files_are_rejected_before_loading_unbounded_content() {
        let tmp = tempfile::tempdir().unwrap();
        let registry = WorkspaceRegistry::new(tmp.path(), &[]).unwrap();
        fs::write(tmp.path().join("huge.txt"), vec![b'a'; 180_001]).unwrap();
        assert!(matches!(
            registry.read_file(None, "huge.txt"),
            Err(NovaError::TooLarge(_))
        ));
    }

    #[test]
    fn search_skips_blocked_and_finds_text() {
        let tmp = tempfile::tempdir().unwrap();
        let r = WorkspaceRegistry::new(tmp.path(), &[]).unwrap();
        fs::write(tmp.path().join("hello.txt"), "alpha\nbeta").unwrap();
        fs::write(tmp.path().join(".env"), "alpha secret").unwrap();
        let results = r.search(None, "alpha", 20).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].line, 1);
    }
}
