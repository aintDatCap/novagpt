//! Authenticated activity and bounded subscriptions.
use super::{
    ActivityEvent, AgentActivity, AgentActivityWait, AgentError, AgentInfo, AgentManager,
    AgentResult, AgentState, AgentWait, MAX_ACTIVITY_REPORTS, MAX_WAIT_SECONDS,
};
use tokio::time::{Duration, timeout};

impl AgentManager {
    /// Report one safe, authenticated event for a ready, nonterminal execution.
    /// The worker cannot supply arbitrary descriptions or other metadata.
    pub async fn progress(&self, id: &str, code: &str, kind: &str) -> AgentResult<ActivityEvent> {
        let mut agents = self.inner.entries.lock().await;
        let entry = agents.get_mut(id).ok_or(AgentError::ActivityDenied)?;
        if !entry.ready
            || !matches!(entry.info.state, AgentState::Running | AgentState::Waiting)
            || entry.completion_claimed
            || entry.activity_reports >= MAX_ACTIVITY_REPORTS
            || !entry.accept_code(code)
        {
            return Err(AgentError::ActivityDenied);
        }
        let description = match kind {
            "working" => "Working on the assigned task",
            "reading" => "Reviewing relevant source material",
            "editing" => "Preparing changes",
            "testing" => "Checking behavior",
            "reviewing" => "Reviewing findings",
            "blocked" => "Worker reported a blocker",
            _ => return Err(AgentError::Invalid),
        };
        entry.activity_reports += 1;
        entry.record_activity(kind, description)
    }

    /// Read-only activity snapshot. No credential is disclosed.
    pub async fn activity(&self, id: &str) -> AgentResult<AgentActivity> {
        self.activity_since(id, 0).await
    }

    pub async fn activity_since(
        &self,
        id: &str,
        after_sequence: u64,
    ) -> AgentResult<AgentActivity> {
        let entries = self.inner.entries.lock().await;
        if let Some(entry) = entries.get(id) {
            return Ok(entry.activity_since(after_sequence));
        }
        let info = self
            .recovered()
            .iter()
            .find(|info| info.id == id)
            .ok_or(AgentError::NotFound)?;
        Ok(AgentActivity {
            id: info.id.clone(),
            epoch: info.epoch,
            state: info.state,
            cursor: 0,
            dropped_before: 0,
            events: Vec::new(),
        })
    }

    /// Cursor-based bounded wait for new progress, epoch change or terminal state.
    /// This does not alter the existing subagent_wait lifecycle contract.
    pub async fn wait_activity(
        &self,
        id: &str,
        after_sequence: u64,
        known_epoch: Option<u64>,
        max_wait_seconds: u64,
    ) -> AgentResult<AgentActivityWait> {
        if !(1..=MAX_WAIT_SECONDS).contains(&max_wait_seconds) {
            return Err(AgentError::Invalid);
        }
        if self.recovered().iter().any(|info| info.id == id)
            && !self.inner.entries.lock().await.contains_key(id)
        {
            return Ok(AgentActivityWait {
                activity: self.activity_since(id, after_sequence).await?,
                timed_out: false,
            });
        }
        let mut updates = {
            let entries = self.inner.entries.lock().await;
            entries
                .get(id)
                .ok_or(AgentError::NotFound)?
                .updates
                .subscribe()
        };
        let changed = |info: &AgentInfo| {
            info.activity_sequence > after_sequence
                || known_epoch.is_some_and(|epoch| info.epoch != epoch)
                || matches!(
                    info.state,
                    AgentState::Completed | AgentState::Failed | AgentState::Cancelled
                )
        };
        let initial = updates.borrow_and_update().clone();
        let timed_out = if changed(&initial) {
            false
        } else {
            match timeout(Duration::from_secs(max_wait_seconds), async {
                loop {
                    updates.changed().await.map_err(|_| AgentError::Backend)?;
                    if changed(&updates.borrow_and_update()) {
                        return Ok::<(), AgentError>(());
                    }
                }
            })
            .await
            {
                Ok(Ok(())) => false,
                Ok(Err(error)) => return Err(error),
                Err(_) => true,
            }
        };
        let activity = self.activity_since(id, after_sequence).await?;
        Ok(AgentActivityWait {
            timed_out: timed_out
                && matches!(activity.state, AgentState::Running | AgentState::Waiting)
                && activity.cursor <= after_sequence
                && !known_epoch.is_some_and(|epoch| activity.epoch != epoch),
            activity,
        })
    }

    /// Wait for the next terminal or human-action state using a watch channel.
    /// No polling loops or mutex guards are held across the wait; callers can
    /// repeat a timed-out request without interrupting the running subagent.
    pub async fn wait(&self, id: &str, max_wait_seconds: u64) -> AgentResult<AgentWait> {
        if !(1..=MAX_WAIT_SECONDS).contains(&max_wait_seconds) {
            return Err(AgentError::Invalid);
        }
        if let Some(info) = self.recovered().iter().find(|info| info.id == id)
            && !self.inner.entries.lock().await.contains_key(id)
        {
            return Ok(AgentWait {
                agent: info.clone(),
                timed_out: false,
            });
        }
        let mut changes = {
            let agents = self.inner.entries.lock().await;
            agents
                .get(id)
                .ok_or(AgentError::NotFound)?
                .updates
                .subscribe()
        };
        let snapshot = changes.borrow_and_update().clone();
        if snapshot.state != AgentState::Running {
            return Ok(AgentWait {
                agent: snapshot,
                timed_out: false,
            });
        }
        let update = timeout(Duration::from_secs(max_wait_seconds), async {
            loop {
                changes.changed().await.map_err(|_| AgentError::Backend)?;
                let snapshot = changes.borrow_and_update().clone();
                if snapshot.state != AgentState::Running {
                    return Ok(snapshot);
                }
            }
        })
        .await;
        match update {
            Ok(Ok(agent)) => Ok(AgentWait {
                agent,
                timed_out: false,
            }),
            Ok(Err(error)) => Err(error),
            Err(_) => {
                let agent = changes.borrow().clone();
                Ok(AgentWait {
                    timed_out: agent.state == AgentState::Running,
                    agent,
                })
            }
        }
    }
}
