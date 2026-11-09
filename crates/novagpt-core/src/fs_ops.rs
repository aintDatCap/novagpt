use crate::workspace::{NovaError, NovaResult, WorkspaceRegistry, blocked};
use grep_regex::RegexMatcher;
use grep_searcher::{SearcherBuilder, sinks::UTF8};
use ignore::WalkBuilder;
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
            // fully verified: track as pending, never claim otherwise.
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

    /// Create a new file inside an approved workspace without ever replacing
    /// an existing entry. Missing parent directories may be created, but path
    /// guards are checked again after acquiring the target lock and mkdir.
    pub fn create_new_file(
        &self,
        workspace_id: Option<&str>,
        relative: &str,
        content: &str,
    ) -> NovaResult<FileReceipt> {
        if relative.trim().is_empty() || content.len() > self.max_write_bytes() {
            return Err(if relative.trim().is_empty() {
                NovaError::InvalidInput("New file path is required".into())
            } else {
                NovaError::TooLarge(self.max_write_bytes())
            });
        }
        let target = self.resolve(workspace_id, relative, true)?;
        self.with_write_lock(&target, || {
            self.resolve(workspace_id, relative, true)?;
            let parent = target
                .parent()
                .ok_or_else(|| NovaError::InvalidInput("Invalid file parent".into()))?;
            fs::create_dir_all(parent)?;
            self.resolve(workspace_id, relative, true)?;
            // create_new uses exclusive creation, preventing overwrites even
            // across independent NovaGPT processes or concurrent agents.
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
            file.write_all(content.as_bytes())?;
            file.sync_all()?;
            self.receipt(workspace_id, &target, content.as_bytes())
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

    /// Regex search using ripgrep's matcher, searcher and ignore-aware walker.
    /// Unlike the existing literal search, this respects .gitignore/.ignore
    /// and skips hidden files. All matches are still bounded by core guards.
    pub fn grep(
        &self,
        id: Option<&str>,
        pattern: &str,
        max_results: usize,
    ) -> NovaResult<Vec<SearchHit>> {
        if pattern.is_empty() || pattern.len() > 4096 {
            return Err(NovaError::InvalidInput(
                "regex pattern must be 1..=4096 bytes".into(),
            ));
        }
        let matcher = RegexMatcher::new_line_matcher(pattern)
            .map_err(|_| NovaError::InvalidInput("invalid regex pattern".into()))?;
        let root = self.workspace(id)?.root;
        let limit = max_results.clamp(1, 2000);
        let max_bytes = self.max_read_bytes();
        let mut hits = Vec::new();
        let mut walker = WalkBuilder::new(&root);
        walker
            .max_depth(Some(12))
            .follow_links(false)
            .require_git(false);
        for entry in walker.build().take(15_000) {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let Ok(relative) = entry.path().strip_prefix(&root) else {
                continue;
            };
            if blocked(relative) {
                continue;
            }
            let Some(relative_str) = relative.to_str() else {
                continue;
            };
            let Ok(path) = self.resolve(id, relative_str, false) else {
                continue;
            };
            if !fs::metadata(&path)
                .is_ok_and(|metadata| metadata.is_file() && metadata.len() <= max_bytes as u64)
            {
                continue;
            }
            // A bounded pre-read prevents binary or invalid UTF-8 matches from
            // being reported before a later NUL/invalid byte is encountered.
            let mut bytes = Vec::new();
            let Ok(file) = fs::File::open(&path) else {
                continue;
            };
            if file
                .take((max_bytes + 1) as u64)
                .read_to_end(&mut bytes)
                .is_err()
                || bytes.len() > max_bytes
                || bytes.contains(&0)
                || std::str::from_utf8(&bytes).is_err()
            {
                continue;
            }
            let mut searcher = SearcherBuilder::new().line_number(true).build();
            let result = searcher.search_slice(
                &matcher,
                &bytes,
                UTF8(|line, text| {
                    hits.push(SearchHit {
                        path: relative_str.to_owned(),
                        line: line as usize,
                        text: text
                            .trim_end_matches(['\r', '\n'])
                            .chars()
                            .take(400)
                            .collect(),
                    });
                    Ok(hits.len() < limit)
                }),
            );
            if result.is_err() {
                continue;
            }
            if hits.len() >= limit {
                break;
            }
        }
        Ok(hits)
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

#[cfg(test)]
mod create_new_tests {
    use super::*;
    #[test]
    fn creates_nested_files_without_overwriting_existing_content() {
        let dir = tempfile::tempdir().unwrap();
        let registry = WorkspaceRegistry::new(dir.path(), &[]).unwrap();
        let receipt = registry
            .create_new_file(None, "generated/nested/hello.rs", "fn hello() {}\n")
            .unwrap();
        assert_eq!(receipt.path.replace('\\', "/"), "generated/nested/hello.rs");
        assert_eq!(receipt.bytes, "fn hello() {}\n".len());
        assert_eq!(
            fs::read_to_string(dir.path().join("generated/nested/hello.rs")).unwrap(),
            "fn hello() {}\n"
        );
        assert!(
            registry
                .create_new_file(None, "generated/nested/hello.rs", "overwrite")
                .is_err()
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("generated/nested/hello.rs")).unwrap(),
            "fn hello() {}\n"
        );
    }

    #[test]
    fn create_new_file_denies_sensitive_and_outside_workspace_paths() {
        let dir = tempfile::tempdir().unwrap();
        let registry = WorkspaceRegistry::new(dir.path(), &[]).unwrap();
        assert!(matches!(
            registry.create_new_file(None, "../escape.txt", "no"),
            Err(NovaError::OutsideAllowedRoot)
        ));
        assert!(matches!(
            registry.create_new_file(None, ".env", "no"),
            Err(NovaError::BlockedPath)
        ));
        assert!(matches!(
            registry.create_new_file(None, "generated/.git/config", "no"),
            Err(NovaError::BlockedPath)
        ));
        assert!(!dir.path().join("generated").exists());
        assert!(registry.create_new_file(None, "", "no").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn new_file_cannot_follow_a_directory_symlink_outside_workspace() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        symlink(other.path(), dir.path().join("redirect")).unwrap();
        let registry = WorkspaceRegistry::new(dir.path(), &[]).unwrap();
        assert!(matches!(
            registry.create_new_file(None, "redirect/private.txt", "no"),
            Err(NovaError::OutsideAllowedRoot)
        ));
        assert!(!other.path().join("private.txt").exists());
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

    #[test]
    fn grep_matches_regex_with_ignores_and_bounded_results() {
        let tmp = tempfile::tempdir().unwrap();
        let r = WorkspaceRegistry::new(tmp.path(), &[]).unwrap();
        fs::write(tmp.path().join(".gitignore"), "skip.txt\n").unwrap();
        fs::write(tmp.path().join(".ignore"), "also_skip.txt\n").unwrap();
        fs::write(tmp.path().join("hello.rs"), "hello world\nHELLO rust\n").unwrap();
        fs::write(tmp.path().join("skip.txt"), "hello ignored\n").unwrap();
        fs::write(tmp.path().join("also_skip.txt"), "hello ignored\n").unwrap();
        fs::write(tmp.path().join(".env"), "hello secret\n").unwrap();
        fs::write(tmp.path().join(".hidden.txt"), "hello hidden\n").unwrap();
        fs::write(tmp.path().join("binary.txt"), b"hello before\n\0after").unwrap();
        fs::write(tmp.path().join("huge.txt"), vec![b'a'; 180_001]).unwrap();
        let results = r.grep(None, "(?i)hello", 100).unwrap();
        assert_eq!(
            results.len(),
            2,
            "ignored, binary and hidden files must be skipped"
        );
        assert_eq!(results[0].path, "hello.rs");
        assert_eq!(results[0].line, 1);
        assert_eq!(results[1].line, 2);
        assert_eq!(r.grep(None, "hello", 1).unwrap().len(), 1);
        assert!(matches!(
            r.grep(None, "(", 5),
            Err(NovaError::InvalidInput(_))
        ));
        assert!(matches!(
            r.grep(None, "", 5),
            Err(NovaError::InvalidInput(_))
        ));
        assert!(matches!(
            r.grep(None, &"a".repeat(4097), 5),
            Err(NovaError::InvalidInput(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn grep_does_not_follow_symlink_outside_workspace() {
        use std::os::unix::fs::symlink;
        let workspace = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("private.txt"), "hello secret").unwrap();
        symlink(
            outside.path().join("private.txt"),
            workspace.path().join("linked.txt"),
        )
        .unwrap();
        let r = WorkspaceRegistry::new(workspace.path(), &[]).unwrap();
        assert!(r.grep(None, "hello", 10).unwrap().is_empty());
    }
}
