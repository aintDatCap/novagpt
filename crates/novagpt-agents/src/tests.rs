use super::*;
use crate::backend::map_browser;
use async_trait::async_trait;
use novagpt_browser::BrowserError;
use tokio::time::{Duration, timeout};

#[test]
fn browser_failures_have_specific_safe_parent_messages() {
    assert_eq!(
        map_browser(BrowserError::NavigationTimeout).to_string(),
        "ChatGPT navigation timed out before prompt submission"
    );
    assert_eq!(
        map_browser(BrowserError::ComposerUnavailable).to_string(),
        "ChatGPT composer unavailable; no prompt submitted"
    );
    assert_eq!(
        map_browser(BrowserError::AmbiguousSubmission).to_string(),
        "Browser outcome uncertain after a possible submission"
    );
    assert_eq!(
        map_browser(BrowserError::Timeout).to_string(),
        "Browser outcome uncertain after a possible submission"
    );
    assert!(matches!(
        map_browser(BrowserError::Manual("verification")),
        BackendFailure::Manual("verification")
    ));
}

#[tokio::test]
async fn invalid_connector_hints_are_denied_before_tab_creation() {
    let dir = tempfile::tempdir().expect("fixture");
    let backend = Arc::new(SingleTurnBackend::default());
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
        backend.clone(),
    );
    let request = SpawnAgent {
        workspace_id: None,
        task: "do not dispatch".into(),
        role: AgentRole::Implementer,
        paths: vec![],
        context: None,
    };
    assert!(matches!(
        manager
            .spawn_with_connector(request.clone(), Some("  "))
            .await,
        Err(AgentError::ConnectorMissing)
    ));
    assert!(matches!(
        manager
            .spawn_with_connector(request, Some("malicious\nconnector"))
            .await,
        Err(AgentError::Invalid)
    ));
    assert!(backend.prompts.lock().await.is_empty());
}

#[derive(Default)]
struct SingleTurnBackend {
    prompts: Mutex<HashMap<String, Vec<String>>>,
    permits: Mutex<HashMap<String, Arc<tokio::sync::Notify>>>,
}

#[async_trait]
impl AgentBackend for SingleTurnBackend {
    fn name(&self) -> &'static str {
        "single-turn-fixture"
    }
    fn model(&self) -> &'static str {
        "fixture"
    }
    async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
        Ok(())
    }
    async fn send(
        &self,
        id: &str,
        prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        let permit = Arc::new(tokio::sync::Notify::new());
        self.permits
            .lock()
            .await
            .insert(id.to_owned(), permit.clone());
        self.prompts
            .lock()
            .await
            .entry(id.to_owned())
            .or_default()
            .push(prompt.to_owned());
        permit.notified().await;
        Ok(("fixture response".into(), None))
    }
    async fn cancel(&self, id: &str) {
        self.complete(id).await;
    }
    async fn complete(&self, id: &str) {
        if let Some(permit) = self.permits.lock().await.get(id) {
            permit.notify_one();
        }
    }
    async fn diagnostics(&self, _id: &str) -> serde_json::Value {
        serde_json::json!({})
    }
}

#[tokio::test]
async fn three_implementers_receive_one_distinct_assignment_per_tab() {
    let dir = tempfile::tempdir().expect("fixture");
    let backend = Arc::new(SingleTurnBackend::default());
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
        backend.clone(),
    );
    let mut workers = Vec::new();
    for index in 0..3 {
        let task = format!("UNIQUE ASSIGNMENT {index}");
        let info = manager
            .spawn_with_connector(
                SpawnAgent {
                    workspace_id: None,
                    role: AgentRole::Implementer,
                    task: task.clone(),
                    paths: vec![],
                    context: None,
                },
                Some("NovaGPT Laptop"),
            )
            .await
            .expect("spawn");
        let code = manager.inner.entries.lock().await[&info.id]
            .completion_code
            .clone();
        workers.push((info.id, code, task));
    }
    timeout(Duration::from_secs(3), async {
        loop {
            if backend.prompts.lock().await.len() == 3 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("all workers received an initial prompt");
    let prompts = backend.prompts.lock().await;
    for (id, _, task) in &workers {
        let own = &prompts[id];
        assert_eq!(own.len(), 1, "one initial send per worker");
        assert!(own[0].contains(task), "worker received its own task");
        assert!(!own[0].contains("CAPABILITY PREFLIGHT"));
        for (_, _, other) in workers.iter().filter(|(_, _, other)| other != task) {
            assert!(!own[0].contains(other), "tasks must not cross tabs");
        }
    }
    drop(prompts);
    for (id, code, _) in &workers {
        let status = manager.status(Some(id)).await.expect("status");
        assert_eq!(status.agents[0].capability_status, "unchecked");
        assert_eq!(status.agents[0].state, AgentState::Running);
        manager
            .finished(id, code, "completed")
            .await
            .expect("finish");
    }
    manager
        .message(&workers[1].0, "FOLLOW-UP FOR WORKER ONE")
        .await
        .expect("follow-up");
    timeout(Duration::from_secs(3), async {
        loop {
            if backend
                .prompts
                .lock()
                .await
                .get(&workers[1].0)
                .is_some_and(|p| p.len() == 2)
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("follow-up reached the same worker");
    let prompts = backend.prompts.lock().await;
    assert!(prompts[&workers[1].0][1].contains("FOLLOW-UP FOR WORKER ONE"));
    assert_eq!(prompts[&workers[0].0].len(), 1);
    assert_eq!(prompts[&workers[2].0].len(), 1);
}

#[tokio::test]
async fn single_turn_implementer_retains_authenticated_write_guards() {
    let dir = tempfile::tempdir().expect("fixture");
    std::fs::write(dir.path().join("approved.rs"), "old\n").expect("existing");
    std::fs::write(dir.path().join("other.rs"), "untouched\n").expect("existing");
    let backend = Arc::new(SingleTurnBackend::default());
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
        backend.clone(),
    );
    let info = manager
        .spawn_with_connector(
            SpawnAgent {
                workspace_id: None,
                role: AgentRole::Implementer,
                task: "Edit the approved file".into(),
                paths: vec!["approved.rs".into()],
                context: None,
            },
            None,
        )
        .await
        .expect("spawn");
    timeout(Duration::from_secs(3), async {
        loop {
            if backend.prompts.lock().await.contains_key(&info.id) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("prompt sent");
    let code = manager.inner.entries.lock().await[&info.id]
        .completion_code
        .clone();
    assert!(matches!(
        manager
            .create_file(&info.id, "WRONG!", "bad.txt", "bad")
            .await,
        Err(AgentError::PatchDenied)
    ));
    assert!(matches!(
        manager
            .probe_tool(&info.id, "WRONG!", "subagent_apply_patch")
            .await,
        Err(AgentError::PatchDenied)
    ));
    assert!(
        manager
            .probe_tool(&info.id, &code, "subagent_apply_patch")
            .await
            .expect("optional no-write probe")
            .available
    );
    manager
        .create_file(&info.id, &code, "new/created.txt", "created\n")
        .await
        .expect("create receipt");
    manager
        .apply_patch(
            &info.id,
            &code,
            "--- a/approved.rs\n+++ b/approved.rs\n@@ -1 +1 @@\n-old\n+updated\n",
        )
        .await
        .expect("patch receipt");
    assert!(matches!(
        manager
            .apply_patch(
                &info.id,
                &code,
                "--- a/other.rs\n+++ b/other.rs\n@@ -1 +1 @@\n-untouched\n+changed\n",
            )
            .await,
        Err(AgentError::PatchDenied)
    ));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("approved.rs")).expect("read"),
        "updated\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("other.rs")).expect("read"),
        "untouched\n"
    );
    let activity = manager.activity(&info.id).await.expect("worker activity");
    assert_eq!(
        activity
            .events
            .iter()
            .map(|event| event.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "tool_probe",
            "create_attempt",
            "patch_attempt",
            "patch_attempt"
        ]
    );
    assert_eq!(activity.cursor, 4);
    assert!(
        activity
            .events
            .iter()
            .all(|event| !event.description.contains("approved.rs")
                && !event.description.contains(&code)),
        "event text must never contain paths or worker credentials"
    );
    manager
        .finished(&info.id, &code, "done")
        .await
        .expect("finished");
    assert!(matches!(
        manager
            .create_file(&info.id, &code, "late.txt", "denied")
            .await,
        Err(AgentError::PatchDenied)
    ));
}

struct TimeoutAfterSubmission;
#[async_trait]
impl AgentBackend for TimeoutAfterSubmission {
    fn name(&self) -> &'static str {
        "timeout-fixture"
    }
    fn model(&self) -> &'static str {
        "fixture"
    }
    async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
        Ok(())
    }
    async fn send(
        &self,
        _id: &str,
        _prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        Err(BackendFailure::Uncertain(
            "Reply timeout after possible submission",
        ))
    }
    async fn cancel(&self, _id: &str) {}
    async fn diagnostics(&self, _id: &str) -> serde_json::Value {
        serde_json::json!({})
    }
}

#[tokio::test]
async fn ambiguous_browser_timeout_keeps_execution_credential_valid() {
    let dir = tempfile::tempdir().expect("fixture");
    let reported = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = reported.clone();
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
        Arc::new(TimeoutAfterSubmission),
    )
    .with_diagnostic_reporter(move |_, stage| {
        captured.lock().expect("diagnostic lock").push(stage);
    });
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "already sent".into(),
            role: AgentRole::Reviewer,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    let timed_out = manager.wait(&info.id, 2).await.expect("wait");
    assert_eq!(timed_out.agent.state, AgentState::Waiting);
    assert!(
        reported
            .lock()
            .expect("diagnostics")
            .contains(&AgentLogStage::BrowserReplyUncertain)
    );
    assert!(
        timed_out
            .agent
            .error
            .as_deref()
            .unwrap_or("")
            .contains("timeout")
    );
    assert!(
        matches!(
            manager.resume(&info.id, true).await,
            Err(AgentError::NotWaiting)
        ),
        "no accidental replay"
    );
    let code = manager.inner.entries.lock().await[&info.id]
        .completion_code
        .clone();
    assert!(manager.progress(&info.id, &code, "working").await.is_ok());
    manager
        .finished(&info.id, &code, "late completion")
        .await
        .expect("callback still active");
    assert_eq!(
        manager.result(&info.id).await.expect("result").state,
        AgentState::Completed
    );
}

struct BlockedBackend;
#[async_trait]
impl AgentBackend for BlockedBackend {
    fn name(&self) -> &'static str {
        "blocked-fixture"
    }
    fn model(&self) -> &'static str {
        "fixture"
    }
    async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
        Ok(())
    }
    async fn send(
        &self,
        _id: &str,
        _prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        Err(BackendFailure::Blocked(
            "ChatGPT composer unavailable; no prompt submitted",
        ))
    }
    async fn cancel(&self, _id: &str) {}
    async fn diagnostics(&self, _id: &str) -> serde_json::Value {
        serde_json::json!({})
    }
}

#[tokio::test]
async fn diagnostic_observer_reports_minted_id_and_browser_failure_stage() {
    let dir = tempfile::tempdir().expect("workspace");
    let reported = Arc::new(std::sync::Mutex::new(Vec::<(String, AgentLogStage)>::new()));
    let recorded = reported.clone();
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
        Arc::new(BlockedBackend),
    )
    .with_diagnostic_reporter(move |id, stage| {
        recorded
            .lock()
            .expect("report mutex")
            .push((id.to_owned(), stage));
    });
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            role: AgentRole::Reviewer,
            task: "check source".into(),
            context: Some("private fixture not logged".into()),
            paths: vec![],
        })
        .await
        .expect("spawn");
    let completed = manager.wait(&info.id, 2).await.expect("wait");
    assert_eq!(completed.agent.state, AgentState::Failed);
    let events = reported.lock().expect("diagnostics").clone();
    assert!(events.iter().all(|(id, _)| id == &info.id));
    assert!(
        events
            .iter()
            .any(|(_, stage)| *stage == AgentLogStage::Created)
    );
    assert!(
        events
            .iter()
            .any(|(_, stage)| *stage == AgentLogStage::BrowserCreated)
    );
    assert!(
        events
            .iter()
            .any(|(_, stage)| *stage == AgentLogStage::BrowserReplyBlocked)
    );
    assert!(
        !events
            .iter()
            .any(|(_, stage)| stage.audit_name().contains("private"))
    );
}

#[tokio::test]
async fn blocked_browser_reason_reaches_parent_without_manual_wait() {
    let dir = tempfile::tempdir().expect("fixture");
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        Arc::new(BlockedBackend),
    );
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "review".into(),
            role: AgentRole::Reviewer,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    let result = manager.wait(&info.id, 2).await.expect("completed wait");
    assert!(!result.timed_out);
    assert_eq!(result.agent.state, AgentState::Failed);
    assert_eq!(
        result.agent.error.as_deref(),
        Some("ChatGPT composer unavailable; no prompt submitted")
    );
}

struct FakeBackend;
#[async_trait]
impl AgentBackend for FakeBackend {
    fn name(&self) -> &'static str {
        "test"
    }
    fn model(&self) -> &'static str {
        "fake"
    }
    async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
        Ok(())
    }
    async fn send(
        &self,
        _id: &str,
        prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        Ok((format!("echo: {prompt}"), None))
    }
    async fn cancel(&self, _id: &str) {}
    async fn diagnostics(&self, _id: &str) -> serde_json::Value {
        serde_json::json!({"ready":true})
    }
}
#[tokio::test]
async fn vm_fork_requires_explicit_approval_and_configured_manager() {
    let root = tempfile::tempdir().expect("workspace");
    let manager = AgentManager::new(
        WorkspaceRegistry::new(root.path(), &[]).expect("registry"),
        Arc::new(FakeBackend),
    );
    let request = SpawnAgent {
        workspace_id: None,
        task: "test isolated guest".into(),
        role: AgentRole::Tester,
        paths: vec![],
        context: None,
    };
    assert!(matches!(
        manager
            .spawn_with_connector_vm(request.clone(), None, Some("approved-windows"))
            .await,
        Err(AgentError::VmUnavailable)
    ));
    assert!(
        manager
            .status(None)
            .await
            .expect("status")
            .agents
            .is_empty()
    );
    let ordinary = manager.spawn(request).await.expect("no VM needed");
    assert!(ordinary.vm.is_none());
}

#[test]
fn vm_lease_states_are_visible_without_host_file_paths() {
    let lease = AgentVmLease {
        image: "base".into(),
        id: "vm-0123456789abcdef".into(),
        state: AgentVmState::CleanupFailed,
    };
    let value = serde_json::to_value(&lease).expect("serialize");
    assert_eq!(value["state"], "cleanup_failed");
    assert!(value.get("host_path").is_none());
}

/// Opt-in host test: exercises the real Hyper-V fork from an approved base
/// without Chrome or guest credentials. Never runs in ordinary CI.
#[cfg(windows)]
#[tokio::test]
#[ignore = "requires NOVAGPT_LIVE_VM_HOME and NOVAGPT_LIVE_VM_IMAGE on a Hyper-V host"]
async fn live_agent_vm_fork_is_released_after_backend_completion() {
    let home = std::env::var("NOVAGPT_LIVE_VM_HOME").expect("explicit VM home");
    let image = std::env::var("NOVAGPT_LIVE_VM_IMAGE").expect("approved base name");
    let vm = VmManager::new(&home).expect("owned VM home");
    let ws = tempfile::tempdir().expect("workspace");
    let manager = AgentManager::new(
        WorkspaceRegistry::new(ws.path(), &[]).expect("registry"),
        Arc::new(FakeBackend),
    )
    .with_vm_manager(vm.clone());
    let info = manager
        .spawn_with_connector_vm(
            SpawnAgent {
                workspace_id: None,
                task: "Report on isolated guest lifecycle".into(),
                role: AgentRole::Tester,
                paths: vec![],
                context: None,
            },
            None,
            Some(&image),
        )
        .await
        .expect("create VM-backed subagent");
    let lease = info.vm.as_ref().expect("VM lease");
    assert_eq!(lease.image, image);
    assert_eq!(lease.state, AgentVmState::Active);
    let vm_id = lease.id.clone();
    let completed = manager.wait(&info.id, 25).await.expect("agent wait");
    assert_eq!(completed.agent.state, AgentState::Completed);
    let terminal = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let state = manager
                .result(&info.id)
                .await
                .expect("status")
                .vm
                .expect("VM lease")
                .state;
            if state == AgentVmState::Destroyed || state == AgentVmState::CleanupFailed {
                return state;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .expect("cleanup timeout");
    assert_eq!(terminal, AgentVmState::Destroyed);
    assert!(
        vm.status(&vm_id).await.is_err(),
        "fork must no longer be registered"
    );
    assert!(
        vm.images()
            .expect("image registry")
            .images
            .iter()
            .any(|entry| entry.name == image)
    );
}

struct UrlBackend;
#[async_trait]
impl AgentBackend for UrlBackend {
    fn name(&self) -> &'static str {
        "test-url"
    }
    fn model(&self) -> &'static str {
        "fake"
    }
    async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
        Ok(())
    }
    async fn send(
        &self,
        _id: &str,
        prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        Ok((
            format!("echo: {prompt}"),
            Some("https://chatgpt.com/c/mock".into()),
        ))
    }
    async fn cancel(&self, _id: &str) {}
    async fn diagnostics(&self, _id: &str) -> serde_json::Value {
        serde_json::json!({"ready":true})
    }
}

#[tokio::test]
async fn conversation_recorder_receives_agent_id_and_conversation_url_only() {
    let dir = tempfile::tempdir().expect("fixture");
    let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
        Arc::new(UrlBackend),
    )
    .with_conversation_recorder(move |id, url| {
        sink.lock()
            .expect("sink")
            .push((id.to_owned(), url.to_owned()));
    });
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "inspect only".into(),
            role: AgentRole::Explorer,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    manager.wait(&info.id, 2).await.expect("wait");
    let seen = seen.lock().expect("seen");
    assert_eq!(
        seen.as_slice(),
        [(info.id.clone(), "https://chatgpt.com/c/mock".to_owned())]
    );
}

#[tokio::test]
async fn usage_observer_reports_bounded_counts_for_spawn_and_followup() {
    let dir = tempfile::tempdir().expect("fixture");
    type UsageTuple = (String, String, usize, Option<usize>, bool);
    let seen: Arc<std::sync::Mutex<Vec<UsageTuple>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
        Arc::new(FakeBackend),
    )
    .with_usage_reporter(move |id, repo, input_chars, output_chars, failed| {
        sink.lock().expect("sink").push((
            id.to_string(),
            repo.to_string(),
            input_chars,
            output_chars,
            failed,
        ));
    });
    let first = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "private-only-fixture".into(),
            role: AgentRole::Explorer,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    let finished = manager.wait(&first.id, 2).await.expect("first turn");
    assert_eq!(finished.agent.state, AgentState::Completed);
    manager
        .message(&first.id, "followup")
        .await
        .expect("followup");
    let followed = manager.wait(&first.id, 2).await.expect("second turn");
    assert_eq!(followed.agent.state, AgentState::Completed);
    let seen = seen.lock().expect("recorded usage");
    assert_eq!(seen.len(), 2);
    for (id, repo, input, output, failed) in seen.iter() {
        assert_eq!(id, &first.id);
        assert_eq!(repo, &first.workspace_id);
        assert!(*input > 8);
        assert!(output.is_some_and(|chars| chars > 0));
        assert!(!failed);
    }
}

#[tokio::test]
async fn unnamed_read_only_roles_can_spawn_from_legacy_mcp_clients() {
    let dir = tempfile::tempdir().expect("fixture");
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("registry"),
        Arc::new(FakeBackend),
    );
    for role in [AgentRole::Explorer, AgentRole::Reviewer, AgentRole::Tester] {
        let info = manager
            .spawn_with_connector(
                SpawnAgent {
                    workspace_id: None,
                    task: "inspect without changing files".into(),
                    role,
                    paths: vec![],
                    context: None,
                },
                None,
            )
            .await
            .expect("read-only spawn without client-owned alias");
        assert_eq!(info.connector_name, None);
        assert_eq!(info.capability_status, "unchecked");
        let finished = manager.wait(&info.id, 2).await.expect("worker result");
        assert!(!finished.timed_out);
        assert_eq!(finished.agent.state, AgentState::Completed);
        assert!(!finished.agent.result.expect("result").applied_to_workspace);
    }
}

#[tokio::test]
async fn spawn_status_message_and_untrusted_result() {
    let dir = tempfile::tempdir().expect("fixture");
    let workspaces = WorkspaceRegistry::new(dir.path(), &[]).expect("registry");
    let manager = AgentManager::new(workspaces, Arc::new(FakeBackend));
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "inspect".into(),
            role: AgentRole::Explorer,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    assert!(info.untrusted);
    for _ in 0..200 {
        if manager.result(&info.id).await.expect("status").state == AgentState::Completed {
            break;
        }
        tokio::task::yield_now().await;
    }
    let info = manager.result(&info.id).await.expect("status");
    assert_eq!(info.state, AgentState::Completed);
    assert!(!info.result.expect("result").applied_to_workspace);
    let continued = manager.message(&info.id, "next").await.expect("followup");
    assert_eq!(continued.state, AgentState::Running);
}
struct PausedBackend {
    created: tokio::sync::Notify,
    continue_create: tokio::sync::Notify,
    sent: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl AgentBackend for PausedBackend {
    fn name(&self) -> &'static str {
        "test-paused"
    }
    fn model(&self) -> &'static str {
        "fake"
    }
    async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
        self.created.notify_one();
        self.continue_create.notified().await;
        Ok(())
    }
    async fn send(
        &self,
        _id: &str,
        _prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        self.sent.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(("not expected".into(), None))
    }
    async fn cancel(&self, _id: &str) {}
    async fn diagnostics(&self, _id: &str) -> serde_json::Value {
        serde_json::json!({})
    }
}

#[tokio::test]
async fn cancellation_during_create_prevents_later_send() {
    let dir = tempfile::tempdir().expect("fixture");
    let backend = Arc::new(PausedBackend {
        created: tokio::sync::Notify::new(),
        continue_create: tokio::sync::Notify::new(),
        sent: std::sync::atomic::AtomicUsize::new(0),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        backend.clone(),
    );
    let agent = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "check".into(),
            role: AgentRole::Tester,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    backend.created.notified().await;
    manager.cancel(&agent.id).await.expect("cancel");
    backend.continue_create.notify_one();
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    assert_eq!(backend.sent.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(
        manager.result(&agent.id).await.expect("status").state,
        AgentState::Cancelled
    );
}

#[tokio::test]
async fn wait_returns_completed_output_and_rejects_invalid_requests() {
    let dir = tempfile::tempdir().expect("fixture");
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        Arc::new(FakeBackend),
    );
    assert!(matches!(
        manager.wait("agent-unknown", 1).await,
        Err(AgentError::NotFound)
    ));
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "read-only review".into(),
            role: AgentRole::Reviewer,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    assert!(matches!(
        manager.wait(&info.id, 0).await,
        Err(AgentError::Invalid)
    ));
    assert!(matches!(
        manager.wait(&info.id, MAX_WAIT_SECONDS + 1).await,
        Err(AgentError::Invalid)
    ));
    let finished = manager
        .wait(&info.id, 2)
        .await
        .expect("wait for completion");
    assert!(!finished.timed_out);
    assert_eq!(finished.agent.state, AgentState::Completed);
    let result = finished.agent.result.expect("untrusted result");
    assert!(result.raw_response.contains("read-only review"));
    assert!(result.untrusted);
    assert!(!result.applied_to_workspace);
    // Already completed: return immediately without waiting.
    let again = manager.wait(&info.id, 2).await.expect("already terminal");
    assert_eq!(again.agent.state, AgentState::Completed);
    assert!(!again.timed_out);
}

#[tokio::test]
async fn wait_timeout_does_not_cancel_worker_and_cancel_wakes_waiter() {
    let dir = tempfile::tempdir().expect("fixture");
    let backend = Arc::new(PausedBackend {
        created: tokio::sync::Notify::new(),
        continue_create: tokio::sync::Notify::new(),
        sent: std::sync::atomic::AtomicUsize::new(0),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        backend.clone(),
    );
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "pause".into(),
            role: AgentRole::Tester,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    backend.created.notified().await;
    let timed = manager.wait(&info.id, 1).await.expect("bounded wait");
    assert!(timed.timed_out);
    assert_eq!(timed.agent.state, AgentState::Running);
    let waiting_manager = manager.clone();
    let id = info.id.clone();
    let waiting = tokio::spawn(async move {
        waiting_manager
            .wait(&id, 5)
            .await
            .expect("wait for cancellation")
    });
    tokio::task::yield_now().await;
    manager.cancel(&info.id).await.expect("cancel");
    let cancelled = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .expect("woken promptly")
        .expect("task join");
    assert_eq!(cancelled.agent.state, AgentState::Cancelled);
    assert!(!cancelled.timed_out);
    backend.continue_create.notify_one();
}

struct ManualBackend;
#[async_trait]
impl AgentBackend for ManualBackend {
    fn name(&self) -> &'static str {
        "manual-test"
    }
    fn model(&self) -> &'static str {
        "fixture"
    }
    async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
        Ok(())
    }
    async fn send(
        &self,
        _id: &str,
        _prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        Err(BackendFailure::Manual("verification"))
    }
    async fn cancel(&self, _id: &str) {}
    async fn diagnostics(&self, _id: &str) -> serde_json::Value {
        serde_json::json!({})
    }
}

#[tokio::test]
async fn wait_stops_on_human_action_without_claiming_completion() {
    let dir = tempfile::tempdir().expect("fixture");
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        Arc::new(ManualBackend),
    );
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "requires human".into(),
            role: AgentRole::Explorer,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    let snapshot = manager.wait(&info.id, 2).await.expect("wait for manual");
    assert!(!snapshot.timed_out);
    assert_eq!(snapshot.agent.state, AgentState::Waiting);
    assert!(snapshot.agent.result.is_none());
    assert!(
        snapshot
            .agent
            .error
            .as_deref()
            .is_some_and(|s| s.contains("verification"))
    );
}

struct HeldSendBackend {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
#[async_trait]
impl AgentBackend for HeldSendBackend {
    fn name(&self) -> &'static str {
        "held-send"
    }
    fn model(&self) -> &'static str {
        "fixture"
    }
    async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
        Ok(())
    }
    async fn send(
        &self,
        _id: &str,
        _prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(("late browser result".into(), None))
    }
    async fn cancel(&self, _id: &str) {}
    async fn diagnostics(&self, _id: &str) -> serde_json::Value {
        serde_json::json!({})
    }
}

#[tokio::test]
async fn waiting_worker_can_report_activity_without_changing_wait_semantics() {
    let dir = tempfile::tempdir().expect("fixture");
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        Arc::new(ManualBackend),
    );
    let worker = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "inspect".into(),
            role: AgentRole::Explorer,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    let lifecycle = manager.wait(&worker.id, 2).await.expect("wait");
    assert_eq!(lifecycle.agent.state, AgentState::Waiting);
    let code = manager
        .inner
        .entries
        .lock()
        .await
        .get(&worker.id)
        .expect("entry")
        .completion_code
        .clone();
    let wait_manager = manager.clone();
    let id = worker.id.clone();
    let waiter = tokio::spawn(async move {
        wait_manager
            .wait_activity(&id, 0, Some(1), 5)
            .await
            .expect("activity wait")
    });
    tokio::task::yield_now().await;
    let event = manager
        .progress(&worker.id, &code, "working")
        .await
        .expect("waiting worker progress");
    let returned = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("wake")
        .expect("join");
    assert!(!returned.timed_out);
    assert_eq!(returned.activity.events[0].sequence, event.sequence);
    assert_eq!(returned.activity.state, AgentState::Waiting);
    manager.cancel(&worker.id).await.expect("cancel");
}

#[tokio::test]
async fn progress_is_authenticated_and_shares_the_completion_rate_limit() {
    let dir = tempfile::tempdir().expect("fixture");
    let backend = Arc::new(HeldSendBackend {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        backend.clone(),
    );
    let worker = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "inspect".into(),
            role: AgentRole::Reviewer,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    backend.entered.notified().await;
    let code = manager
        .inner
        .entries
        .lock()
        .await
        .get(&worker.id)
        .expect("entry")
        .completion_code
        .clone();
    let wrong = if code == "AAAAAA" { "BBBBBB" } else { "AAAAAA" };
    for _ in 0..MAX_COMPLETION_ATTEMPTS {
        assert!(matches!(
            manager.progress(&worker.id, wrong, "working").await,
            Err(AgentError::ActivityDenied)
        ));
    }
    assert!(matches!(
        manager.finished(&worker.id, &code, "too late").await,
        Err(AgentError::CompletionDenied)
    ));
    assert!(matches!(
        manager.progress(&worker.id, &code, "working").await,
        Err(AgentError::ActivityDenied)
    ));
    assert!(
        manager
            .activity(&worker.id)
            .await
            .unwrap()
            .events
            .is_empty()
    );
    manager.cancel(&worker.id).await.expect("cancel");
    backend.release.notify_one();
}

#[tokio::test]
async fn progress_buffer_is_bounded_and_followup_rotates_epoch_and_code() {
    let dir = tempfile::tempdir().expect("fixture");
    let backend = Arc::new(HeldSendBackend {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        backend.clone(),
    );
    let worker = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "inspect".into(),
            role: AgentRole::Tester,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    backend.entered.notified().await;
    let old = manager
        .inner
        .entries
        .lock()
        .await
        .get(&worker.id)
        .expect("entry")
        .completion_code
        .clone();
    assert!(matches!(
        manager
            .progress(&worker.id, &old, "https://private.invalid")
            .await,
        Err(AgentError::Invalid)
    ));
    for _ in 0..MAX_ACTIVITY_REPORTS {
        manager
            .progress(&worker.id, &old, "working")
            .await
            .expect("progress");
    }
    assert!(matches!(
        manager.progress(&worker.id, &old, "working").await,
        Err(AgentError::ActivityDenied)
    ));
    let feed = manager.activity(&worker.id).await.expect("feed");
    assert_eq!(feed.events.len(), MAX_ACTIVITY_EVENTS);
    assert_eq!(feed.cursor, u64::from(MAX_ACTIVITY_REPORTS));
    assert_eq!(
        feed.dropped_before,
        u64::from(MAX_ACTIVITY_REPORTS) - MAX_ACTIVITY_EVENTS as u64
    );
    assert!(
        feed.events
            .iter()
            .all(|e| e.timestamp_ms > 0 && e.description == "Working on the assigned task")
    );
    manager
        .finished(&worker.id, &old, "complete")
        .await
        .expect("finish");
    assert!(matches!(
        manager.progress(&worker.id, &old, "working").await,
        Err(AgentError::ActivityDenied)
    ));
    manager
        .message(&worker.id, "next task")
        .await
        .expect("followup");
    let rotated = manager
        .wait_activity(&worker.id, feed.cursor, Some(feed.epoch), 1)
        .await
        .expect("epoch change");
    assert!(!rotated.timed_out);
    assert_eq!(rotated.activity.epoch, feed.epoch + 1);
    backend.entered.notified().await;
    let new = manager
        .inner
        .entries
        .lock()
        .await
        .get(&worker.id)
        .expect("entry")
        .completion_code
        .clone();
    assert_ne!(old, new);
    let reset = manager.activity(&worker.id).await.expect("reset");
    assert_eq!(reset.cursor, feed.cursor);
    assert_eq!(reset.epoch, feed.epoch + 1);
    assert!(reset.events.is_empty());
    assert!(matches!(
        manager.progress(&worker.id, &old, "working").await,
        Err(AgentError::ActivityDenied)
    ));
    let event = manager
        .progress(&worker.id, &new, "testing")
        .await
        .expect("new run");
    assert_eq!(event.sequence, feed.cursor + 1);
    manager.cancel(&worker.id).await.expect("cancel");
    assert!(matches!(
        manager.progress(&worker.id, &new, "testing").await,
        Err(AgentError::ActivityDenied)
    ));
    backend.release.notify_one();
}

#[tokio::test]
async fn activity_wait_wakes_one_worker_only_and_then_on_cancellation() {
    let dir = tempfile::tempdir().expect("fixture");
    let backend = Arc::new(HeldSendBackend {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        backend.clone(),
    );
    let mut ids = Vec::new();
    let mut codes = Vec::new();
    for _ in 0..2 {
        let worker = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: "inspect".into(),
                role: AgentRole::Explorer,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        backend.entered.notified().await;
        codes.push(
            manager
                .inner
                .entries
                .lock()
                .await
                .get(&worker.id)
                .expect("entry")
                .completion_code
                .clone(),
        );
        ids.push(worker.id);
    }
    let first_manager = manager.clone();
    let first_id = ids[0].clone();
    let first = tokio::spawn(async move {
        first_manager
            .wait_activity(&first_id, 0, Some(1), 5)
            .await
            .expect("first wait")
    });
    let second_manager = manager.clone();
    let second_id = ids[1].clone();
    let second = tokio::spawn(async move {
        second_manager
            .wait_activity(&second_id, 0, Some(1), 5)
            .await
            .expect("second wait")
    });
    tokio::task::yield_now().await;
    manager
        .progress(&ids[0], &codes[0], "reading")
        .await
        .expect("first report");
    let updated = tokio::time::timeout(Duration::from_secs(2), first)
        .await
        .expect("first wake")
        .expect("join");
    assert!(!updated.timed_out);
    assert_eq!(updated.activity.events.len(), 1);
    assert!(!second.is_finished(), "other worker must not wake");
    manager.cancel(&ids[1]).await.expect("cancel second");
    let cancelled = tokio::time::timeout(Duration::from_secs(2), second)
        .await
        .expect("cancel wake")
        .expect("join");
    assert_eq!(cancelled.activity.state, AgentState::Cancelled);
    assert!(!cancelled.timed_out);
    assert!(matches!(
        manager.wait_activity(&ids[0], 1, Some(1), 0).await,
        Err(AgentError::Invalid)
    ));
    manager.cancel(&ids[0]).await.expect("cancel first");
    backend.release.notify_one();
    backend.release.notify_one();
}

#[tokio::test]
async fn finished_uses_six_character_private_code_and_wakes_waiter() {
    let dir = tempfile::tempdir().expect("fixture");
    let backend = Arc::new(HeldSendBackend {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        backend.clone(),
    );
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "inspect source".into(),
            role: AgentRole::Reviewer,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    backend.entered.notified().await;
    let (code, prompt) = {
        let entries = manager.inner.entries.lock().await;
        let entry = entries.get(&info.id).expect("entry");
        (entry.completion_code.clone(), entry.pending_prompt.clone())
    };
    assert_eq!(code.len(), COMPLETION_KEY_LEN);
    assert!(
        code.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    );
    assert!(prompt.contains(&format!("code={code}")));
    assert!(prompt.contains(&format!("id={}", info.id)));
    assert!(
        !serde_json::to_string(&info)
            .expect("serialize info")
            .contains(&code)
    );
    assert!(
        !serde_json::to_string(&manager.status(Some(&info.id)).await.expect("status"))
            .expect("serialize status")
            .contains(&code)
    );

    let other = if code == "AAAAAA" { "BBBBBB" } else { "AAAAAA" };
    assert!(matches!(
        manager.finished(&info.id, other, "forged").await,
        Err(AgentError::CompletionDenied)
    ));
    let waiting_manager = manager.clone();
    let id = info.id.clone();
    let waiter = tokio::spawn(async move {
        waiting_manager
            .wait(&id, 5)
            .await
            .expect("wait for callback")
    });
    tokio::task::yield_now().await;
    let ack = manager
        .finished(&info.id, &code, "verified report")
        .await
        .expect("finish");
    assert_eq!(ack.id, info.id);
    assert_eq!(ack.state, AgentState::Completed);
    let done = tokio::time::timeout(Duration::from_secs(2), waiter)
        .await
        .expect("wake")
        .expect("task");
    assert!(!done.timed_out);
    assert_eq!(done.agent.state, AgentState::Completed);
    assert_eq!(
        done.agent.result.expect("output").raw_response,
        "verified report"
    );
    assert!(matches!(
        manager.finished(&info.id, &code, "duplicate").await,
        Err(AgentError::CompletionDenied)
    ));
    backend.release.notify_one();
    tokio::task::yield_now().await;
    let result = manager.result(&info.id).await.expect("result");
    assert_eq!(
        result.result.expect("report").raw_response,
        "verified report"
    );
}

#[tokio::test]
async fn implementer_applies_only_assigned_files_with_verified_receipts() {
    let dir = tempfile::tempdir().expect("fixture");
    std::fs::write(dir.path().join("one.rs"), "before one\n").unwrap();
    std::fs::write(dir.path().join("two.rs"), "before two\n").unwrap();
    std::fs::write(dir.path().join("other.rs"), "private\n").unwrap();
    let backend = Arc::new(HeldSendBackend {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("workspace"),
        backend.clone(),
    );
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "edit assigned files".into(),
            role: AgentRole::Implementer,
            paths: vec!["one.rs".into(), "two.rs".into()],
            context: None,
        })
        .await
        .expect("spawn");
    backend.entered.notified().await;
    let code = manager.inner.entries.lock().await[&info.id]
        .completion_code
        .clone();
    let first = "--- a/one.rs\n+++ b/one.rs\n@@ -1 +1 @@\n-before one\n+after one\n";
    let second = "diff --git a/two.rs b/two.rs\n--- a/two.rs\n+++ b/two.rs\n@@ -1 +1 @@\n-before two\n+after two\n";
    let forbidden = "--- a/other.rs\n+++ b/other.rs\n@@ -1 +1 @@\n-private\n+corrupted\n";
    let wrong = if code == "AAAAAA" { "BBBBBB" } else { "AAAAAA" };
    assert!(matches!(
        manager.apply_patch(&info.id, wrong, first).await,
        Err(AgentError::PatchDenied)
    ));
    assert!(matches!(
        manager.apply_patch(&info.id, &code, forbidden).await,
        Err(AgentError::PatchDenied)
    ));
    let first_receipt = manager
        .apply_patch(&info.id, &code, first)
        .await
        .expect("real first edit");
    assert_eq!(first_receipt.paths, ["one.rs"]);
    assert_eq!(first_receipt.patches_applied, 1);
    let second_receipt = manager
        .apply_patch(&info.id, &code, second)
        .await
        .expect("real second edit");
    assert_eq!(second_receipt.paths, ["two.rs"]);
    assert_eq!(second_receipt.patches_applied, 2);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("one.rs")).unwrap(),
        "after one\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("two.rs")).unwrap(),
        "after two\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("other.rs")).unwrap(),
        "private\n"
    );
    assert!(matches!(
        manager.apply_patch(&info.id, &code, first).await,
        Err(AgentError::PatchRejected)
    ));
    let state = manager
        .status(Some(&info.id))
        .await
        .unwrap()
        .agents
        .remove(0);
    assert_eq!(state.patches_applied, 2);
    assert_eq!(state.patched_paths, ["one.rs", "two.rs"]);
    assert_eq!(state.state, AgentState::Running);
    manager
        .finished(&info.id, &code, "Verified receipts for both edits")
        .await
        .unwrap();
    let done = manager.wait(&info.id, 2).await.unwrap();
    assert!(done.agent.result.unwrap().applied_to_workspace);
    assert!(matches!(
        manager.apply_patch(&info.id, &code, second).await,
        Err(AgentError::PatchDenied)
    ));
    backend.release.notify_one();
}

#[tokio::test]
async fn patch_rejects_read_only_roles_old_codes_and_cancelled_runs() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source.rs"), "old\n").unwrap();
    let backend = Arc::new(HeldSendBackend {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).unwrap(),
        backend.clone(),
    );
    let patch = "--- a/source.rs\n+++ b/source.rs\n@@ -1 +1 @@\n-old\n+new\n";
    let reviewer = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "inspect".into(),
            role: AgentRole::Reviewer,
            paths: vec!["source.rs".into()],
            context: None,
        })
        .await
        .unwrap();
    backend.entered.notified().await;
    let review_code = manager.inner.entries.lock().await[&reviewer.id]
        .completion_code
        .clone();
    assert!(matches!(
        manager.apply_patch(&reviewer.id, &review_code, patch).await,
        Err(AgentError::PatchDenied)
    ));
    manager.cancel(&reviewer.id).await.unwrap();
    assert!(matches!(
        manager.apply_patch(&reviewer.id, &review_code, patch).await,
        Err(AgentError::PatchDenied)
    ));
    let worker = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "update".into(),
            role: AgentRole::Implementer,
            paths: vec!["source.rs".into()],
            context: None,
        })
        .await
        .unwrap();
    backend.entered.notified().await;
    let first_code = manager.inner.entries.lock().await[&worker.id]
        .completion_code
        .clone();
    assert!(matches!(
        manager.apply_patch(&worker.id, &review_code, patch).await,
        Err(AgentError::PatchDenied)
    ));
    manager
        .finished(&worker.id, &first_code, "not applied")
        .await
        .unwrap();
    manager.message(&worker.id, "next execution").await.unwrap();
    backend.entered.notified().await;
    let second_code = manager.inner.entries.lock().await[&worker.id]
        .completion_code
        .clone();
    assert_ne!(first_code, second_code);
    assert!(matches!(
        manager.apply_patch(&worker.id, &first_code, patch).await,
        Err(AgentError::PatchDenied)
    ));
    assert_eq!(
        manager
            .apply_patch(&worker.id, &second_code, patch)
            .await
            .unwrap()
            .patches_applied,
        1
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("source.rs")).unwrap(),
        "new\n"
    );
    manager.cancel(&worker.id).await.unwrap();
    assert!(matches!(
        manager.apply_patch(&worker.id, &second_code, patch).await,
        Err(AgentError::PatchDenied)
    ));
    backend.release.notify_waiters();
}

#[tokio::test]
async fn implementer_creates_and_then_edits_any_safe_new_file_without_preselection() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("existing.rs"), "keep\n").unwrap();
    let backend = Arc::new(HeldSendBackend {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).unwrap(),
        backend.clone(),
    );
    let worker = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "create new source files".into(),
            role: AgentRole::Implementer,
            paths: vec![],
            context: None,
        })
        .await
        .unwrap();
    backend.entered.notified().await;
    let code = manager.inner.entries.lock().await[&worker.id]
        .completion_code
        .clone();
    let wrong = if code == "AAAAAA" { "BBBBBB" } else { "AAAAAA" };
    assert!(matches!(
        manager
            .create_file(&worker.id, wrong, "src/new.rs", "created\n")
            .await,
        Err(AgentError::PatchDenied)
    ));
    let created = manager
        .create_file(&worker.id, &code, "src/deep/generated.rs", "created\n")
        .await
        .unwrap();
    assert_eq!(created.path.replace('\\', "/"), "src/deep/generated.rs");
    assert_eq!(created.files_created, 1);
    assert_eq!(created.bytes, "created\n".len());
    assert_eq!(created.sha256.len(), 64);
    assert!(matches!(
        manager
            .create_file(&worker.id, &code, "src/deep/generated.rs", "overwrite\n")
            .await,
        Err(AgentError::CreateRejected)
    ));
    assert!(matches!(
        manager
            .create_file(&worker.id, &code, ".env", "secret")
            .await,
        Err(AgentError::CreateRejected)
    ));
    assert!(matches!(
        manager
            .create_file(&worker.id, &code, "../outside.txt", "escape")
            .await,
        Err(AgentError::CreateRejected)
    ));
    let change = "--- a/src/deep/generated.rs\n+++ b/src/deep/generated.rs\n@@ -1 +1 @@\n-created\n+updated\n";
    assert_eq!(
        manager
            .apply_patch(&worker.id, &code, change)
            .await
            .unwrap()
            .patches_applied,
        1
    );
    let other = "--- a/existing.rs\n+++ b/existing.rs\n@@ -1 +1 @@\n-keep\n+destroyed\n";
    assert!(matches!(
        manager.apply_patch(&worker.id, &code, other).await,
        Err(AgentError::PatchDenied)
    ));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/deep/generated.rs")).unwrap(),
        "updated\n"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("existing.rs")).unwrap(),
        "keep\n"
    );
    let status = manager.status(Some(&worker.id)).await.unwrap();
    assert_eq!(status.agents[0].files_created, 1);
    assert_eq!(
        status.agents[0]
            .created_paths
            .iter()
            .map(|path| path.replace('\\', "/"))
            .collect::<Vec<_>>(),
        ["src/deep/generated.rs"]
    );
    manager
        .finished(&worker.id, &code, "Created and edited")
        .await
        .unwrap();
    assert!(
        manager
            .result(&worker.id)
            .await
            .unwrap()
            .result
            .unwrap()
            .applied_to_workspace
    );
    backend.release.notify_one();
}

#[tokio::test]
async fn file_creation_is_denied_for_other_roles_cancelled_and_old_runs() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(HeldSendBackend {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).unwrap(),
        backend.clone(),
    );
    let reviewer = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "review".into(),
            role: AgentRole::Reviewer,
            paths: vec![],
            context: None,
        })
        .await
        .unwrap();
    backend.entered.notified().await;
    let reviewer_code = manager.inner.entries.lock().await[&reviewer.id]
        .completion_code
        .clone();
    assert!(matches!(
        manager
            .create_file(&reviewer.id, &reviewer_code, "reviewer.txt", "no")
            .await,
        Err(AgentError::PatchDenied)
    ));
    manager.cancel(&reviewer.id).await.unwrap();
    let worker = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "create".into(),
            role: AgentRole::Implementer,
            paths: vec![],
            context: None,
        })
        .await
        .unwrap();
    backend.entered.notified().await;
    let first_code = manager.inner.entries.lock().await[&worker.id]
        .completion_code
        .clone();
    assert!(matches!(
        manager
            .create_file(&worker.id, &reviewer_code, "cross-agent.txt", "no")
            .await,
        Err(AgentError::PatchDenied)
    ));
    manager
        .finished(&worker.id, &first_code, "nothing created")
        .await
        .unwrap();
    manager
        .message(&worker.id, "create new file")
        .await
        .unwrap();
    backend.entered.notified().await;
    let second_code = manager.inner.entries.lock().await[&worker.id]
        .completion_code
        .clone();
    assert_ne!(first_code, second_code);
    assert!(matches!(
        manager
            .create_file(&worker.id, &first_code, "stale.txt", "no")
            .await,
        Err(AgentError::PatchDenied)
    ));
    assert_eq!(
        manager
            .create_file(&worker.id, &second_code, "new.txt", "yes")
            .await
            .unwrap()
            .files_created,
        1
    );
    manager.cancel(&worker.id).await.unwrap();
    assert!(matches!(
        manager
            .create_file(&worker.id, &second_code, "after-cancel.txt", "no")
            .await,
        Err(AgentError::PatchDenied)
    ));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("new.txt")).unwrap(),
        "yes"
    );
    for name in [
        "reviewer.txt",
        "cross-agent.txt",
        "stale.txt",
        "after-cancel.txt",
    ] {
        assert!(!dir.path().join(name).exists(), "unexpected file: {name}");
    }
    backend.release.notify_waiters();
}

#[tokio::test]
async fn patch_attempts_are_bounded_even_when_git_rejects_every_diff() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("source.rs"), "before\n").unwrap();
    let backend = Arc::new(HeldSendBackend {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).unwrap(),
        backend.clone(),
    );
    let worker = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "bounded edits".into(),
            role: AgentRole::Implementer,
            paths: vec!["source.rs".into()],
            context: None,
        })
        .await
        .unwrap();
    backend.entered.notified().await;
    let code = manager.inner.entries.lock().await[&worker.id]
        .completion_code
        .clone();
    let conflict = "--- a/source.rs\n+++ b/source.rs\n@@ -1 +1 @@\n-nonexistent\n+after\n";
    let valid = "--- a/source.rs\n+++ b/source.rs\n@@ -1 +1 @@\n-before\n+after\n";
    for _ in 0..MAX_PATCH_ATTEMPTS {
        assert!(matches!(
            manager.apply_patch(&worker.id, &code, conflict).await,
            Err(AgentError::PatchRejected)
        ));
    }
    assert!(matches!(
        manager.apply_patch(&worker.id, &code, valid).await,
        Err(AgentError::PatchDenied)
    ));
    assert_eq!(
        manager.status(Some(&worker.id)).await.unwrap().agents[0].patches_applied,
        0
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("source.rs")).unwrap(),
        "before\n"
    );
    manager.cancel(&worker.id).await.unwrap();
    backend.release.notify_one();
}

#[tokio::test]
async fn short_code_is_rate_limited_rotated_and_invalidated_after_cancel() {
    let dir = tempfile::tempdir().expect("fixture");
    let backend = Arc::new(HeldSendBackend {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        backend.clone(),
    );
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "first".into(),
            role: AgentRole::Tester,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    backend.entered.notified().await;
    let old_code = {
        let entries = manager.inner.entries.lock().await;
        entries
            .get(&info.id)
            .expect("entry")
            .completion_code
            .clone()
    };
    let wrong = if old_code == "AAAAAA" {
        "BBBBBB"
    } else {
        "AAAAAA"
    };
    for _ in 0..MAX_COMPLETION_ATTEMPTS {
        assert!(matches!(
            manager.finished(&info.id, wrong, "bad guess").await,
            Err(AgentError::CompletionDenied)
        ));
    }
    assert!(matches!(
        manager.finished(&info.id, &old_code, "late correct").await,
        Err(AgentError::CompletionDenied)
    ));
    // Browser result is still permitted even after the callback is rate-limited.
    backend.release.notify_one();
    assert_eq!(
        manager
            .wait(&info.id, 3)
            .await
            .expect("browser fallback")
            .agent
            .state,
        AgentState::Completed
    );
    manager
        .message(&info.id, "second task")
        .await
        .expect("followup");
    backend.entered.notified().await;
    let new_code = {
        let entries = manager.inner.entries.lock().await;
        entries
            .get(&info.id)
            .expect("entry")
            .completion_code
            .clone()
    };
    assert_ne!(old_code, new_code);
    assert_eq!(new_code.len(), 6);
    assert!(matches!(
        manager.finished(&info.id, &old_code, "old task").await,
        Err(AgentError::CompletionDenied)
    ));
    manager.cancel(&info.id).await.expect("cancel");
    assert!(matches!(
        manager
            .finished(&info.id, &new_code, "cancelled task")
            .await,
        Err(AgentError::CompletionDenied)
    ));
    backend.release.notify_one();
}

#[tokio::test]
async fn waiting_manual_run_may_be_completed_with_its_current_code() {
    let dir = tempfile::tempdir().expect("fixture");
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        Arc::new(ManualBackend),
    );
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "manual".into(),
            role: AgentRole::Explorer,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    let waiting = manager.wait(&info.id, 2).await.expect("wait");
    assert_eq!(waiting.agent.state, AgentState::Waiting);
    let code = {
        let entries = manager.inner.entries.lock().await;
        entries
            .get(&info.id)
            .expect("entry")
            .completion_code
            .clone()
    };
    let ack = manager
        .finished(&info.id, &code, "completed via MCP")
        .await
        .expect("finish");
    assert_eq!(ack.state, AgentState::Completed);
    assert_eq!(
        manager
            .wait(&info.id, 1)
            .await
            .expect("finished")
            .agent
            .state,
        AgentState::Completed
    );
    assert!(matches!(
        manager.resume(&info.id, true).await,
        Err(AgentError::NotWaiting)
    ));
}

struct DelayedCompleteBackend {
    sending: tokio::sync::Notify,
    completing: tokio::sync::Notify,
    release_complete: tokio::sync::Notify,
}

#[async_trait]
impl AgentBackend for DelayedCompleteBackend {
    fn name(&self) -> &'static str {
        "delayed-complete"
    }
    fn model(&self) -> &'static str {
        "fixture"
    }
    async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
        Ok(())
    }
    async fn send(
        &self,
        _id: &str,
        _prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        self.sending.notify_one();
        std::future::pending().await
    }
    async fn cancel(&self, _id: &str) {}
    async fn complete(&self, _id: &str) {
        self.completing.notify_one();
        self.release_complete.notified().await;
    }
    async fn diagnostics(&self, _id: &str) -> serde_json::Value {
        serde_json::json!({})
    }
}

#[tokio::test]
async fn completion_callback_blocks_new_messages_until_backend_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(DelayedCompleteBackend {
        sending: tokio::sync::Notify::new(),
        completing: tokio::sync::Notify::new(),
        release_complete: tokio::sync::Notify::new(),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).unwrap(),
        backend.clone(),
    );
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "review implementation".into(),
            role: AgentRole::Reviewer,
            paths: vec![],
            context: None,
        })
        .await
        .unwrap();
    backend.sending.notified().await;
    let code = manager
        .inner
        .entries
        .lock()
        .await
        .get(&info.id)
        .unwrap()
        .completion_code
        .clone();
    let worker = manager.clone();
    let id = info.id.clone();
    let callback =
        tokio::spawn(async move { worker.finished(&id, &code, "callback result").await });
    backend.completing.notified().await;
    assert!(matches!(
        manager.message(&info.id, "next task").await,
        Err(AgentError::Busy)
    ));
    assert!(matches!(
        manager.result(&info.id).await.unwrap().state,
        AgentState::Running
    ));
    backend.release_complete.notify_one();
    assert_eq!(
        callback.await.unwrap().unwrap().state,
        AgentState::Completed
    );
    assert_eq!(
        manager
            .result(&info.id)
            .await
            .unwrap()
            .result
            .unwrap()
            .raw_response,
        "callback result"
    );
    assert_eq!(
        manager.message(&info.id, "next task").await.unwrap().state,
        AgentState::Running
    );
    backend.sending.notified().await;
}

#[tokio::test]
async fn disconnected_completion_request_does_not_strand_worker() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(DelayedCompleteBackend {
        sending: tokio::sync::Notify::new(),
        completing: tokio::sync::Notify::new(),
        release_complete: tokio::sync::Notify::new(),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).unwrap(),
        backend.clone(),
    );
    let info = manager.spawn(tab_task(0)).await.unwrap();
    backend.sending.notified().await;
    let code = manager.inner.entries.lock().await[&info.id]
        .completion_code
        .clone();
    let worker = manager.clone();
    let id = info.id.clone();
    let callback =
        tokio::spawn(async move { worker.finished(&id, &code, "accepted result").await });
    backend.completing.notified().await;
    callback.abort();
    assert!(callback.await.unwrap_err().is_cancelled());
    assert!(matches!(
        manager.message(&info.id, "next").await,
        Err(AgentError::Busy)
    ));
    backend.release_complete.notify_one();
    let done = manager.wait(&info.id, 2).await.unwrap();
    assert_eq!(done.agent.state, AgentState::Completed);
    assert_eq!(done.agent.result.unwrap().raw_response, "accepted result");
}

#[tokio::test]
async fn cancelled_agent_cannot_be_resumed() {
    let dir = tempfile::tempdir().expect("fixture");
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).expect("root"),
        Arc::new(FakeBackend),
    );
    let info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "inspect".into(),
            role: AgentRole::Reviewer,
            paths: vec![],
            context: None,
        })
        .await
        .expect("spawn");
    manager.cancel(&info.id).await.expect("cancel");
    assert!(matches!(
        manager.resume(&info.id, true).await,
        Err(AgentError::Cancelled)
    ));
}
/// Each delegated worker independently parks on its own response gate.
/// Releasing them out of spawn order must never swap their page/result ID.
struct OrderedWorkers {
    entered: tokio::sync::mpsc::UnboundedSender<(String, tokio::sync::oneshot::Sender<()>)>,
    cancelled: Mutex<Vec<String>>,
    closed: Mutex<Vec<String>>,
}

#[async_trait]
impl AgentBackend for OrderedWorkers {
    fn name(&self) -> &'static str {
        "ordered-workers"
    }
    fn model(&self) -> &'static str {
        "chatgpt-web-fixture"
    }
    async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
        Ok(())
    }
    async fn send(
        &self,
        id: &str,
        _prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        let (release, waiting) = tokio::sync::oneshot::channel();
        self.entered
            .send((id.to_owned(), release))
            .map_err(|_| BackendFailure::Unavailable)?;
        waiting.await.map_err(|_| BackendFailure::Unavailable)?;
        Ok((
            format!("response for {id}"),
            Some(format!("https://chatgpt.com/c/{id}")),
        ))
    }
    async fn cancel(&self, id: &str) {
        self.cancelled.lock().await.push(id.to_owned());
    }
    async fn close(&self, id: &str) {
        self.closed.lock().await.push(id.to_owned());
    }
    async fn diagnostics(&self, id: &str) -> serde_json::Value {
        serde_json::json!({"agent_id":id})
    }
}

#[tokio::test]
async fn five_concurrent_workers_out_of_order_with_isolated_cancellation() {
    use std::collections::{HashMap, HashSet};
    use tokio::sync::mpsc;
    let root = tempfile::tempdir().expect("owned test workspace");
    let sentinel = root.path().join("source.rs");
    std::fs::write(&sentinel, "user-owned source is unchanged").expect("fixture source");
    let (entered, mut received) = mpsc::unbounded_channel();
    let backend = Arc::new(OrderedWorkers {
        entered,
        cancelled: Mutex::new(Vec::new()),
        closed: Mutex::new(Vec::new()),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(root.path(), &[]).expect("workspace"),
        backend.clone(),
    );
    let mut ids = Vec::new();
    for role in [
        AgentRole::Explorer,
        AgentRole::Reviewer,
        AgentRole::Tester,
        AgentRole::Implementer,
        AgentRole::Reviewer,
    ] {
        let info = manager
            .spawn(SpawnAgent {
                workspace_id: None,
                task: format!("independent task for {role:?}"),
                role,
                paths: vec![],
                context: None,
            })
            .await
            .expect("spawn");
        assert_eq!(info.state, AgentState::Running);
        ids.push(info.id);
    }
    let mut gates = HashMap::new();
    for _ in 0..5 {
        let (id, release) = tokio::time::timeout(Duration::from_secs(3), received.recv())
            .await
            .expect("all sessions started")
            .expect("channel open");
        assert!(gates.insert(id, release).is_none());
    }
    assert_eq!(ids.iter().collect::<HashSet<_>>().len(), 5);
    let cancelled_id = ids[1].clone();
    manager
        .cancel(&cancelled_id)
        .await
        .expect("cancel one agent");
    assert_eq!(
        manager
            .result(&cancelled_id)
            .await
            .expect("cancel snapshot")
            .state,
        AgentState::Cancelled
    );
    // Complete the other four in reverse registration order.
    for id in ids.iter().rev().filter(|id| *id != &cancelled_id) {
        gates
            .remove(id)
            .expect("owned response gate")
            .send(())
            .expect("worker still waiting");
        let output = manager.wait(id, 3).await.expect("worker completed");
        assert!(!output.timed_out);
        assert_eq!(output.agent.state, AgentState::Completed);
        assert_eq!(
            output.agent.external_conversation.as_deref(),
            Some(format!("https://chatgpt.com/c/{id}").as_str())
        );
        let result = output.agent.result.expect("untrusted response");
        assert_eq!(result.raw_response, format!("response for {id}"));
        assert!(result.untrusted);
        assert!(!result.applied_to_workspace);
    }
    // A cancelled worker returning late must never overwrite its state.
    gates
        .remove(&cancelled_id)
        .expect("cancelled response gate")
        .send(())
        .expect("cancelled worker remains test parked");
    tokio::task::yield_now().await;
    let cancelled = manager.wait(&cancelled_id, 2).await.expect("cancelled");
    assert_eq!(cancelled.agent.state, AgentState::Cancelled);
    assert!(cancelled.agent.result.is_none());
    assert_eq!(
        backend.cancelled.lock().await.as_slice(),
        std::slice::from_ref(&cancelled_id)
    );
    assert_eq!(backend.closed.lock().await.as_slice(), &[cancelled_id]);
    assert_eq!(
        std::fs::read_to_string(&sentinel).expect("source"),
        "user-owned source is unchanged"
    );
}

#[tokio::test]
async fn followups_and_spawns_share_the_same_active_capacity() {
    let root = tempfile::tempdir().unwrap();
    let (entered, mut received) = tokio::sync::mpsc::unbounded_channel();
    let manager = AgentManager::new(
        WorkspaceRegistry::new(root.path(), &[]).unwrap(),
        Arc::new(OrderedWorkers {
            entered,
            cancelled: Mutex::new(Vec::new()),
            closed: Mutex::new(Vec::new()),
        }),
    );
    let previous = manager.spawn(tab_task(0)).await.unwrap();
    let (_, release) = received.recv().await.unwrap();
    release.send(()).unwrap();
    assert_eq!(
        manager.wait(&previous.id, 2).await.unwrap().agent.state,
        AgentState::Completed
    );
    let mut active = Vec::new();
    let mut releases = Vec::new();
    for i in 0..MAX_AGENTS {
        active.push(manager.spawn(tab_task(i + 1)).await.unwrap().id);
        releases.push(received.recv().await.unwrap().1);
    }
    assert!(matches!(
        manager.message(&previous.id, "followup").await,
        Err(AgentError::Capacity)
    ));
    assert!(matches!(
        manager.spawn(tab_task(99)).await,
        Err(AgentError::Capacity)
    ));
    releases.pop().unwrap().send(()).unwrap();
    manager.wait(active.last().unwrap(), 2).await.unwrap();
    assert_eq!(
        manager
            .message(&previous.id, "followup")
            .await
            .unwrap()
            .state,
        AgentState::Running
    );
}

struct TabLifecycle {
    closed: Mutex<Vec<String>>,
    fail_send: bool,
}

#[async_trait]
impl AgentBackend for TabLifecycle {
    fn name(&self) -> &'static str {
        "tab-fixture"
    }
    fn model(&self) -> &'static str {
        "fixture"
    }
    async fn create(&self, _id: &str) -> Result<(), BackendFailure> {
        Ok(())
    }
    async fn send(
        &self,
        id: &str,
        _prompt: &str,
    ) -> Result<(String, Option<String>), BackendFailure> {
        if self.fail_send {
            Err(BackendFailure::Unavailable)
        } else {
            Ok((format!("response for {id}"), None))
        }
    }
    async fn cancel(&self, _id: &str) {}
    async fn close(&self, id: &str) {
        self.closed.lock().await.push(id.to_owned());
    }
    async fn diagnostics(&self, _id: &str) -> serde_json::Value {
        serde_json::json!({})
    }
}

fn tab_task(index: usize) -> SpawnAgent {
    SpawnAgent {
        workspace_id: None,
        task: format!("tab lifecycle task {index}"),
        role: AgentRole::Reviewer,
        paths: vec![],
        context: None,
    }
}

#[tokio::test]
async fn completed_tabs_are_evicted_at_the_live_cap_and_failed_tabs_close() {
    let root = tempfile::tempdir().expect("owned test workspace");
    let backend = Arc::new(TabLifecycle {
        closed: Mutex::new(Vec::new()),
        fail_send: false,
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(root.path(), &[]).expect("workspace"),
        backend.clone(),
    )
    .with_live_tab_cap(2);
    let mut ids = Vec::new();
    for index in 0..3 {
        let info = manager.spawn(tab_task(index)).await.expect("spawn");
        let wait = manager.wait(&info.id, 3).await.expect("wait");
        assert_eq!(wait.agent.state, AgentState::Completed);
        ids.push(info.id);
        if index < 2 {
            assert!(
                backend.closed.lock().await.is_empty(),
                "tabs stay open below the cap"
            );
        }
    }
    assert_eq!(
        backend.closed.lock().await.as_slice(),
        std::slice::from_ref(&ids[0]),
        "the oldest completed tab is evicted at the cap"
    );
    // A follow-up to an evicted conversation must create a new tab rather
    // than targeting the closed one; the agent still completes.
    let reopened = manager
        .message(&ids[0], "follow up")
        .await
        .expect("message");
    assert_eq!(reopened.state, AgentState::Running);
    let wait = manager.wait(&ids[0], 3).await.expect("follow-up");
    assert_eq!(wait.agent.state, AgentState::Completed);

    let failing = Arc::new(TabLifecycle {
        closed: Mutex::new(Vec::new()),
        fail_send: true,
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(root.path(), &[]).expect("workspace"),
        failing.clone(),
    );
    let info = manager.spawn(tab_task(9)).await.expect("spawn");
    let wait = manager.wait(&info.id, 3).await.expect("wait");
    assert_eq!(wait.agent.state, AgentState::Failed);
    assert_eq!(
        failing.closed.lock().await.as_slice(),
        std::slice::from_ref(&info.id),
        "a failed execution closes its tab"
    );
}

#[tokio::test]
async fn history_recovers_interrupted_worker_without_replaying() {
    let dir = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let registry = WorkspaceRegistry::new(dir.path(), &[]).unwrap();
    let path = dir.path().join("history.json");
    let manager = AgentManager::new(registry.clone(), Arc::new(FakeBackend))
        .with_history(path.clone())
        .unwrap();
    let mut info = manager
        .spawn(SpawnAgent {
            workspace_id: None,
            task: "private prompt fixture".into(),
            role: AgentRole::Tester,
            paths: vec![],
            context: None,
        })
        .await
        .unwrap();
    manager.wait(&info.id, 5).await.unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while Arc::strong_count(&manager.inner) > 1 {
        assert!(std::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    info.epoch += 1;
    info.state = AgentState::Running;
    manager.remember(&info).await.unwrap();
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("private prompt fixture")
    );
    assert!(
        AgentManager::new(registry.clone(), Arc::new(FakeBackend))
            .with_history(path.clone())
            .is_err()
    );
    let history = Arc::downgrade(manager.inner.history.get().unwrap());
    drop(manager);
    while history.strong_count() > 0 {
        assert!(std::time::Instant::now() < deadline);
        tokio::task::yield_now().await;
    }
    let recovered = loop {
        match AgentManager::new(registry.clone(), Arc::new(FakeBackend)).with_history(path.clone())
        {
            Ok(manager) => break manager,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(std::time::Instant::now() < deadline);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => panic!("{error}"),
        }
    };
    let record = recovered.result(&info.id).await.unwrap();
    assert_eq!(record.state, AgentState::Failed);
    assert!(record.result.is_none());
    assert!(record.error.unwrap().contains("restart"));
    assert!(!recovered.wait(&info.id, 1).await.unwrap().timed_out);
    assert!(recovered.message(&info.id, "do not replay").await.is_err());
}

#[tokio::test]
async fn shutdown_closes_admission_and_joins_worker() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(PausedBackend {
        created: tokio::sync::Notify::new(),
        continue_create: tokio::sync::Notify::new(),
        sent: std::sync::atomic::AtomicUsize::new(0),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(dir.path(), &[]).unwrap(),
        backend.clone(),
    );
    let info = manager.spawn(tab_task(42)).await.unwrap();
    backend.created.notified().await;
    manager.begin_shutdown();
    assert!(matches!(
        manager.spawn(tab_task(43)).await,
        Err(AgentError::ShuttingDown)
    ));
    backend.continue_create.notify_one();
    manager.shutdown().await;
    assert!(matches!(
        manager.result(&info.id).await.unwrap().state,
        AgentState::Cancelled | AgentState::Completed
    ));
    assert!(manager.inner.tasks.lock().unwrap().is_empty());
}

/// Model the real adapter's two independent boundaries: startup acknowledgement
/// and response completion. A new tab must not open across the first boundary.
struct SubmissionGatedBackend {
    created: tokio::sync::mpsc::UnboundedSender<String>,
    acknowledge: tokio::sync::Semaphore,
    finish: tokio::sync::Semaphore,
}

#[async_trait]
impl AgentBackend for SubmissionGatedBackend {
    fn name(&self) -> &'static str {
        "submission-gated"
    }
    fn model(&self) -> &'static str {
        "fixture"
    }
    async fn create(&self, id: &str) -> Result<(), BackendFailure> {
        self.created
            .send(id.to_owned())
            .map_err(|_| BackendFailure::Unavailable)
    }
    async fn send(&self, id: &str, _: &str) -> Result<(String, Option<String>), BackendFailure> {
        self.finish
            .acquire()
            .await
            .map_err(|_| BackendFailure::Unavailable)?
            .forget();
        Ok((id.to_owned(), None))
    }
    async fn send_with_submission(
        &self,
        id: &str,
        prompt: &str,
        submitted: tokio::sync::oneshot::Sender<()>,
    ) -> Result<(String, Option<String>), BackendFailure> {
        self.acknowledge
            .acquire()
            .await
            .map_err(|_| BackendFailure::Unavailable)?
            .forget();
        let _ = submitted.send(());
        self.send(id, prompt).await
    }
    async fn cancel(&self, _: &str) {}
    async fn diagnostics(&self, _: &str) -> serde_json::Value {
        serde_json::json!({})
    }
}

#[tokio::test]
async fn five_workers_start_one_at_a_time_but_replies_remain_concurrent() {
    let root = tempfile::tempdir().unwrap();
    let (created, mut next) = tokio::sync::mpsc::unbounded_channel();
    let backend = Arc::new(SubmissionGatedBackend {
        created,
        acknowledge: tokio::sync::Semaphore::new(0),
        finish: tokio::sync::Semaphore::new(0),
    });
    let manager = AgentManager::new(
        WorkspaceRegistry::new(root.path(), &[]).unwrap(),
        backend.clone(),
    );
    let mut ids = Vec::new();
    for i in 0..5 {
        ids.push(manager.spawn(tab_task(i)).await.unwrap().id);
    }
    for _ in 0..5 {
        let id = timeout(Duration::from_secs(2), next.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(ids.contains(&id));
        assert!(
            timeout(Duration::from_millis(50), next.recv())
                .await
                .is_err(),
            "no second tab before acknowledgement"
        );
        backend.acknowledge.add_permits(1);
    }
    for id in &ids {
        assert_eq!(manager.result(id).await.unwrap().state, AgentState::Running);
    }
    backend.finish.add_permits(5);
    for id in &ids {
        assert_eq!(
            manager.wait(id, 3).await.unwrap().agent.state,
            AgentState::Completed
        );
    }
}
