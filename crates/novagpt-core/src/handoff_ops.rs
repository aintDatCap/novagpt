//! Bounded workspace-scoped handoff artifacts. Creation never starts an agent.
//! Handoff IDs are explicit, new files only; no CodexPro state/profile access.
use crate::{FileDocument, NovaError, NovaResult, WorkspaceRegistry};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{fs, io::Write};

const MAX_HANDOFF: usize = 32 * 1024;

#[derive(Debug, Serialize)]
pub struct HandoffReceipt {
    pub id: String,
    pub path: String,
    pub bytes: usize,
    pub sha256: String,
    pub executed: bool,
}

fn handoff_path(id: &str) -> NovaResult<String> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(NovaError::InvalidInput(
            "Handoff ID must use 1..64 ASCII letters, numbers, dashes or underscores".into(),
        ));
    }
    Ok(format!(".ai-bridge/handoffs/{id}.md"))
}

impl WorkspaceRegistry {
    pub fn create_handoff(
        &self,
        workspace_id: Option<&str>,
        id: &str,
        instructions: &str,
    ) -> NovaResult<HandoffReceipt> {
        let relative = handoff_path(id)?;
        if instructions.trim().is_empty()
            || instructions.len() > MAX_HANDOFF
            || instructions.contains('\0')
        {
            return Err(NovaError::InvalidInput(
                "Handoff text must be nonempty UTF-8 and <=32768 bytes".into(),
            ));
        }
        let target = self.resolve(workspace_id, &relative, true)?;
        self.with_write_lock(&target, || {
            self.resolve(workspace_id, &relative, true)?;
            if target.exists() {
                return Err(NovaError::InvalidInput(
                    "Handoff ID already exists; refusing overwrite".into(),
                ));
            }
            let parent = target.parent().ok_or(NovaError::OutsideAllowedRoot)?;
            fs::create_dir_all(parent)?;
            self.resolve(workspace_id, &relative, true)?;
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
            if let Err(error) = output
                .write_all(instructions.as_bytes())
                .and_then(|_| output.sync_all())
            {
                let _ = fs::remove_file(&target);
                return Err(NovaError::Io(error));
            }
            Ok(HandoffReceipt {
                id: id.into(),
                path: relative,
                bytes: instructions.len(),
                sha256: hex::encode(Sha256::digest(instructions.as_bytes())),
                executed: false,
            })
        })
    }

    pub fn read_handoff(&self, workspace_id: Option<&str>, id: &str) -> NovaResult<FileDocument> {
        self.read_file(workspace_id, &handoff_path(id)?)
    }

    pub fn load_skill(&self, workspace_id: Option<&str>, path: &str) -> NovaResult<FileDocument> {
        let target = std::path::Path::new(path);
        if target.file_name().is_none_or(|name| name != "SKILL.md") {
            return Err(NovaError::InvalidInput(
                "Skills must be opened by an explicit SKILL.md path".into(),
            ));
        }
        self.read_file(workspace_id, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_handoff_and_safe_skill_paths() {
        let root = tempfile::tempdir().expect("fixture");
        let state = WorkspaceRegistry::new(root.path(), &[]).expect("workspace");
        let created = state
            .create_handoff(None, "run_01", "Investigate a bounded fixture.")
            .expect("handoff");
        assert!(!created.executed);
        assert_eq!(
            state.read_handoff(None, "run_01").expect("read").bytes,
            created.bytes
        );
        assert!(state.create_handoff(None, "run_01", "overwrite").is_err());
        assert!(state.create_handoff(None, "../bad", "text").is_err());
        assert!(state.read_handoff(None, "../../outside").is_err());
        assert!(state.load_skill(None, ".env").is_err());
        std::fs::create_dir_all(root.path().join("skills").join("demo")).expect("dir");
        std::fs::write(
            root.path().join("skills").join("demo").join("SKILL.md"),
            "approved",
        )
        .expect("fixture");
        assert_eq!(
            state
                .load_skill(None, "skills/demo/SKILL.md")
                .expect("skill")
                .text,
            "approved"
        );
    }
}
