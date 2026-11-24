//! Authenticated workspace mutations and capability probes.
use super::{
    AgentCreateReceipt, AgentError, AgentManager, AgentPatchReceipt, AgentResult, AgentRole,
    AgentState, AgentToolProbe, Entry, MAX_AGENT_PATCH_BYTES, MAX_CREATE_ATTEMPTS,
    MAX_PATCH_ATTEMPTS,
};

/// Compare workspace-relative paths regardless of the platform separator.
/// Patch targets always use `/`, while file-creation receipts use the OS
/// separator on Windows; a plain equality check would deny patching a file
/// this same run had just created.
fn same_relative_path(left: &str, right: &str) -> bool {
    left.replace('\\', "/") == right.replace('\\', "/")
}

impl Entry {
    fn authorize_mutation(
        &mut self,
        code: &str,
        available: bool,
        allow_waiting: bool,
    ) -> AgentResult<()> {
        if !self.ready
            || self.info.role != AgentRole::Implementer
            || !(self.info.state == AgentState::Running
                || (allow_waiting && self.info.state == AgentState::Waiting))
            || self.completion_claimed
            || !available
            || !self.accept_code(code)
        {
            return Err(AgentError::PatchDenied);
        }
        Ok(())
    }
}

impl AgentManager {
    /// Confirm a live callback endpoint without changing repository state.
    /// The optional no-write probe checks the current execution credential.
    /// It never requires a separate browser conversation turn.
    pub async fn probe_tool(
        &self,
        id: &str,
        code: &str,
        capability: &str,
    ) -> AgentResult<AgentToolProbe> {
        let mut entries = self.inner.entries.lock().await;
        let entry = entries.get_mut(id).ok_or(AgentError::PatchDenied)?;
        entry.authorize_mutation(code, true, false)?;
        let capability = match capability {
            "subagent_apply_patch" => "subagent_apply_patch",
            "subagent_create_file" => "subagent_create_file",
            _ => return Err(AgentError::Invalid),
        };
        entry.record_activity("tool_probe", "Authenticated capability probe")?;
        Ok(AgentToolProbe {
            id: id.to_owned(),
            capability,
            available: true,
        })
    }

    /// Apply an explicitly requested, authenticated, single-file update in the
    /// assigned real workspace. The blocking worker owns the per-agent gate
    /// until Git exits, even if its async caller disconnects.
    pub async fn apply_patch(
        &self,
        id: &str,
        code: &str,
        patch: &str,
    ) -> AgentResult<AgentPatchReceipt> {
        if patch.is_empty() || patch.len() > MAX_AGENT_PATCH_BYTES {
            return Err(AgentError::PatchRejected);
        }
        // Queue asynchronously; transfer the owned guard into blocking I/O.
        let gate = self
            .patch_gate(id)
            .await
            .map_err(|_| AgentError::PatchDenied)?
            .lock_owned()
            .await;
        let id = id.to_owned();
        let code = code.to_owned();
        let patch = patch.to_owned();
        let manager = self.clone();
        tokio::task::spawn_blocking(move || {
            let relative =
                novagpt_core::patch_target(&patch).map_err(|_| AgentError::PatchRejected)?;
            let _gate = gate;
            let workspace_id = {
                let mut entries = manager.inner.entries.blocking_lock();
                let entry = entries.get_mut(&id).ok_or(AgentError::PatchDenied)?;
                entry.authorize_mutation(&code, entry.patch_attempts < MAX_PATCH_ATTEMPTS, true)?;
                entry.record_activity("patch_attempt", "Authenticated patch request")?;
                let workspace_id = entry.info.workspace_id.clone();
                let target = manager
                    .inner
                    .workspaces
                    .resolve(Some(&workspace_id), &relative, true)
                    .map_err(|_| AgentError::PatchDenied)?;
                // A file this run created is also an authorized edit target,
                // even when its path was not known at spawn time.
                if !entry.allowed_paths.contains(&target)
                    && !entry
                        .info
                        .created_paths
                        .iter()
                        .any(|path| same_relative_path(path, &relative))
                {
                    return Err(AgentError::PatchDenied);
                }
                entry.patch_attempts += 1;
                workspace_id
            };
            let receipt = manager
                .inner
                .workspaces
                .apply_unified_patch(Some(&workspace_id), &patch)
                .map_err(|_| AgentError::PatchRejected)?;
            let mut entries = manager.inner.entries.blocking_lock();
            let entry = entries.get_mut(&id).ok_or(AgentError::PatchDenied)?;
            entry.info.patches_applied += 1;
            for path in &receipt.paths {
                if !entry.info.patched_paths.contains(path) {
                    entry.info.patched_paths.push(path.clone());
                }
            }
            entry.updates.send_replace(entry.info.clone());
            Ok(AgentPatchReceipt {
                id,
                workspace_id,
                paths: receipt.paths,
                bytes: receipt.bytes,
                patches_applied: entry.info.patches_applied,
            })
        })
        .await
        .map_err(|_| AgentError::Backend)?
    }

    /// Create a new text file anywhere inside the agent's approved workspace.
    /// Unlike updating existing files, creation needs no preselected path. The
    /// exclusive core create operation never replaces another agent's work.
    pub async fn create_file(
        &self,
        id: &str,
        code: &str,
        path: &str,
        content: &str,
    ) -> AgentResult<AgentCreateReceipt> {
        // Bound incoming allocations before cloning work into spawn_blocking.
        if path.trim().is_empty() || path.len() > 1024 || content.len() > 1_000_000 {
            return Err(AgentError::CreateRejected);
        }
        let manager = self.clone();
        // Queue asynchronously; transfer the owned guard into blocking I/O.
        let gate = self
            .patch_gate(id)
            .await
            .map_err(|_| AgentError::PatchDenied)?
            .lock_owned()
            .await;
        let id = id.to_owned();
        let code = code.to_owned();
        let path = path.to_owned();
        let content = content.to_owned();
        tokio::task::spawn_blocking(move || {
            let _gate = gate;
            let workspace_id = {
                let mut entries = manager.inner.entries.blocking_lock();
                let entry = entries.get_mut(&id).ok_or(AgentError::PatchDenied)?;
                entry.authorize_mutation(
                    &code,
                    entry.create_attempts < MAX_CREATE_ATTEMPTS,
                    true,
                )?;
                entry.record_activity("create_attempt", "Authenticated file creation request")?;
                entry.create_attempts += 1;
                entry.info.workspace_id.clone()
            };
            let receipt = manager
                .inner
                .workspaces
                .create_new_file(Some(&workspace_id), &path, &content)
                .map_err(|_| AgentError::CreateRejected)?;
            let mut entries = manager.inner.entries.blocking_lock();
            let entry = entries.get_mut(&id).ok_or(AgentError::PatchDenied)?;
            entry.info.files_created += 1;
            if !entry.info.created_paths.contains(&receipt.path) {
                entry.info.created_paths.push(receipt.path.clone());
            }
            entry.updates.send_replace(entry.info.clone());
            Ok(AgentCreateReceipt {
                id,
                workspace_id,
                path: receipt.path,
                sha256: receipt.sha256,
                bytes: receipt.bytes,
                files_created: entry.info.files_created,
            })
        })
        .await
        .map_err(|_| AgentError::Backend)?
    }
}
