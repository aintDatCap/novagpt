//! Bounded read-only handoff run-state observer. No executor, shell, or state-defined paths.
use crate::{NovaError, NovaResult, WorkspaceRegistry};
use serde::{Deserialize, Serialize};
use std::io::ErrorKind;

const STATE_PATH: &str = ".ai-bridge/handoff-run-state.json";

#[derive(Deserialize)]
struct StateFile {
    state: String,
    #[serde(default)]
    iteration: Option<u32>,
    #[serde(default)]
    plan_hash: Option<String>,
    #[serde(default)]
    exit_code: Option<i32>,
}

#[derive(Debug, Serialize)]
pub struct HandoffStatus {
    pub workspace_id: String,
    pub state_file: &'static str,
    pub state: String,
    pub awaited_terminal: bool,
    pub awaited_completed: bool,
    pub iteration: Option<u32>,
    pub plan_hash: Option<String>,
    pub expected_plan_hash: Option<String>,
    pub plan_hash_mismatch: bool,
    pub exit_code: Option<i32>,
    pub next_poll_after_seconds: Option<u64>,
}

fn valid_hash(hash: &str) -> bool {
    (1..=128).contains(&hash.len()) && hash.bytes().all(|ch| ch.is_ascii_hexdigit())
}

impl WorkspaceRegistry {
    pub fn handoff_status(
        &self,
        workspace_id: Option<&str>,
        expected_hash: Option<&str>,
        since_iteration: Option<u32>,
    ) -> NovaResult<HandoffStatus> {
        if expected_hash.is_some_and(|hash| !valid_hash(hash)) {
            return Err(NovaError::InvalidInput(
                "Plan hash must be bounded hexadecimal".into(),
            ));
        }
        let workspace = self.workspace(workspace_id)?;
        let file = self.resolve(workspace_id, STATE_PATH, false)?;
        let state = match std::fs::symlink_metadata(&file) {
            Ok(meta) => {
                if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > 16 * 1024 {
                    return Err(NovaError::InvalidInput("Unsafe handoff state file".into()));
                }
                let text = self.read_file(workspace_id, STATE_PATH)?.text;
                let parsed: StateFile = serde_json::from_str(&text)
                    .map_err(|_| NovaError::InvalidInput("Malformed handoff run state".into()))?;
                if !matches!(
                    parsed.state.as_str(),
                    "running" | "pending" | "completed" | "failed" | "timed_out" | "cancelled"
                ) || parsed
                    .plan_hash
                    .as_deref()
                    .is_some_and(|hash| !valid_hash(hash))
                {
                    return Err(NovaError::InvalidInput(
                        "Unrecognized handoff run state".into(),
                    ));
                }
                Some(parsed)
            }
            Err(err) if err.kind() == ErrorKind::NotFound => None,
            Err(err) => return Err(NovaError::Io(err)),
        };
        let mismatch = expected_hash.is_some_and(|expected| {
            state.as_ref().and_then(|s| s.plan_hash.as_deref()) != Some(expected)
        });
        let terminal = state
            .as_ref()
            .is_some_and(|s| matches!(s.state.as_str(), "completed" | "failed" | "timed_out"));
        let awaited = terminal
            && !mismatch
            && since_iteration.is_none_or(|since| {
                state
                    .as_ref()
                    .and_then(|s| s.iteration)
                    .is_some_and(|current| current > since)
            });
        let completed = awaited && state.as_ref().is_some_and(|s| s.state == "completed");
        let displayed = if awaited {
            state.as_ref().map_or("unknown", |s| s.state.as_str())
        } else if state.is_some() {
            "running"
        } else {
            "unknown"
        }
        .to_string();
        Ok(HandoffStatus {
            workspace_id: workspace.workspace_id,
            state_file: STATE_PATH,
            state: displayed,
            awaited_terminal: awaited,
            awaited_completed: completed,
            iteration: state.as_ref().and_then(|s| s.iteration),
            plan_hash: state.as_ref().and_then(|s| s.plan_hash.clone()),
            expected_plan_hash: expected_hash.map(str::to_owned),
            plan_hash_mismatch: mismatch,
            exit_code: state.as_ref().and_then(|s| s.exit_code),
            next_poll_after_seconds: (!awaited).then_some(1),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[test]
    fn state_filters_plan_iteration_and_denies_unsafe_input() {
        let root = tempfile::tempdir().expect("root");
        let registry = WorkspaceRegistry::new(root.path(), &[]).expect("registry");
        assert_eq!(
            registry
                .handoff_status(None, None, None)
                .expect("missing")
                .state,
            "unknown"
        );
        let home = root.path().join(".ai-bridge");
        fs::create_dir(&home).expect("dir");
        fs::write(home.join("handoff-run-state.json"),
            r#"{"state":"completed","iteration":3,"plan_hash":"abcd0123","exit_code":0,"unrecognized_field":"ignored"}"#)
            .expect("state fixture");
        let completed = registry
            .handoff_status(None, Some("abcd0123"), Some(2))
            .expect("done");
        assert!(completed.awaited_completed && completed.awaited_terminal);
        assert!(
            !serde_json::to_string(&completed)
                .expect("serialize")
                .contains("unrecognized_field")
        );
        let stale = registry
            .handoff_status(None, Some("deadbeef"), Some(3))
            .expect("stale");
        assert!(!stale.awaited_terminal && stale.plan_hash_mismatch);
        assert!(registry.handoff_status(None, Some("../bad"), None).is_err());
        fs::write(
            home.join("handoff-run-state.json"),
            r#"{"state":"nonsense"}"#,
        )
        .expect("bad");
        assert!(registry.handoff_status(None, None, None).is_err());
    }
}
