//! Explicit, bounded, read-only Codex-style AGENTS.md instruction context.
//! Never reads secrets, arbitrary source files, or .ai-bridge state by default.
use crate::{NovaError, NovaResult, WorkspaceRegistry};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct CodexInstructions {
    pub workspace_id: String,
    pub target_path: String,
    pub agents_files: Vec<String>,
    pub instruction_fingerprint: String,
    pub instructions_changed: bool,
    pub text: String,
}

impl WorkspaceRegistry {
    pub fn codex_instructions(
        &self,
        id: Option<&str>,
        target_path: Option<&str>,
        previous_fingerprint: Option<&str>,
    ) -> NovaResult<CodexInstructions> {
        let target = target_path.unwrap_or(".");
        let root = self.workspace(id)?;
        let resolved = self.resolve(id, target, false)?;
        let relative = resolved
            .strip_prefix(&root.root)
            .map_err(|_| NovaError::OutsideAllowedRoot)?;
        if relative.components().count() > 16 {
            return Err(NovaError::InvalidInput(
                "Target path is too deeply nested".into(),
            ));
        }
        let mut parent = std::path::PathBuf::new();
        let mut candidates = vec!["AGENTS.md".to_string()];
        // If the target itself is a file, only ancestors are relevant.
        let mut components: Vec<_> = relative.components().collect();
        if !resolved.is_dir() {
            components.pop();
        }
        for component in components {
            parent.push(component.as_os_str());
            candidates.push(parent.join("AGENTS.md").to_string_lossy().into_owned());
        }

        let mut files = Vec::new();
        let mut texts = Vec::new();
        let mut digest = Sha256::new();
        for candidate in candidates {
            let path = self.resolve(id, &candidate, false)?;
            if !path.is_file() {
                continue;
            }
            let document = self.read_file(id, &candidate)?;
            if document.bytes > 16 * 1024
                || texts.iter().map(String::len).sum::<usize>() + document.bytes > 64 * 1024
            {
                return Err(NovaError::TooLarge(16 * 1024));
            }
            digest.update(candidate.as_bytes());
            digest.update(b"\0");
            digest.update(document.text.as_bytes());
            digest.update(b"\0");
            files.push(candidate);
            texts.push(document.text);
        }
        let fingerprint = hex::encode(digest.finalize());
        let changed = previous_fingerprint != Some(fingerprint.as_str());
        let text = if changed {
            files
                .iter()
                .zip(texts.iter())
                .map(|(file, contents)| format!("# {file}\n\n{contents}"))
                .collect::<Vec<_>>()
                .join("\n\n")
        } else {
            String::new()
        };

        Ok(CodexInstructions {
            workspace_id: root.workspace_id,
            target_path: Path::new(target).to_string_lossy().into_owned(),
            agents_files: files,
            instruction_fingerprint: fingerprint,
            instructions_changed: changed,
            text,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn instructions_chain_respects_target_and_fingerprint() {
        let temp = tempfile::tempdir().expect("root");
        fs::write(temp.path().join("AGENTS.md"), "root instruction").expect("root");
        fs::create_dir_all(temp.path().join("src").join("unit")).expect("directories");
        fs::write(temp.path().join("src").join("AGENTS.md"), "src instruction").expect("nested");
        fs::write(
            temp.path().join("src").join("unit").join("test.rs"),
            "fn test() {}",
        )
        .expect("file");
        let registry = WorkspaceRegistry::new(temp.path(), &[]).expect("workspace");
        let first = registry
            .codex_instructions(None, Some("src/unit/test.rs"), None)
            .expect("context");
        assert_eq!(first.agents_files.len(), 2);
        assert!(first.text.contains("root instruction"));
        assert!(first.text.contains("src instruction"));
        assert!(!first.text.contains("fn test()"));
        let second = registry
            .codex_instructions(
                None,
                Some("src/unit/test.rs"),
                Some(&first.instruction_fingerprint),
            )
            .expect("unchanged");
        assert!(!second.instructions_changed);
        assert!(second.text.is_empty());
        assert!(
            registry
                .codex_instructions(None, Some("../outside"), None)
                .is_err()
        );
    }
}
