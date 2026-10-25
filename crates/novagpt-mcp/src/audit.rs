//! Process-scoped, bounded metadata-only observation for HUMAN/LLM views.
//! No arguments, prompts, paths, tool output, headers, credentials or file contents.
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

const MAX_EVENTS: usize = 128;

#[derive(Clone, Debug, Serialize)]
pub struct AuditEvent {
    pub sequence: u64,
    pub unix_seconds: u64,
    pub tool: &'static str,
    pub status: &'static str,
}

#[derive(Debug, Serialize)]
pub struct AuditSnapshot {
    pub mode: &'static str,
    pub total_calls: u64,
    pub returned_events: usize,
    pub human: Vec<String>,
    pub llm: Vec<AuditEvent>,
}

#[derive(Default)]
struct AuditState {
    count: u64,
    events: VecDeque<AuditEvent>,
}

#[derive(Clone, Default)]
pub struct AuditTrail {
    state: Arc<Mutex<AuditState>>,
}

impl AuditTrail {
    pub fn record(&self, tool: &'static str, failed: bool) {
        if let Ok(mut state) = self.state.lock() {
            state.count = state.count.saturating_add(1);
            let sequence = state.count;
            if state.events.len() == MAX_EVENTS {
                state.events.pop_front();
            }
            state.events.push_back(AuditEvent {
                sequence,
                unix_seconds: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |value| value.as_secs()),
                tool,
                status: if failed { "error" } else { "ok" },
            });
        }
    }

    pub fn snapshot(&self, limit: usize) -> AuditSnapshot {
        let Ok(state) = self.state.lock() else {
            return AuditSnapshot {
                mode: "metadata-only-memory",
                total_calls: 0,
                returned_events: 0,
                human: vec!["Audit metadata unavailable".into()],
                llm: Vec::new(),
            };
        };
        let entries: Vec<_> = state
            .events
            .iter()
            .rev()
            .take(limit.clamp(1, MAX_EVENTS))
            .rev()
            .cloned()
            .collect();
        AuditSnapshot {
            mode: "metadata-only-memory",
            total_calls: state.count,
            returned_events: entries.len(),
            human: entries
                .iter()
                .map(|item| format!("#{} {} {}", item.sequence, item.tool, item.status))
                .collect(),
            llm: entries,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn records_only_metadata_and_bounds_ring() {
        let audit = AuditTrail::default();
        for _ in 0..180 {
            audit.record("read", false);
        }
        audit.record("write", true);
        let snapshot = audit.snapshot(999);
        assert_eq!(snapshot.total_calls, 181);
        assert_eq!(snapshot.llm.len(), 128);
        assert_eq!(snapshot.llm.last().expect("latest").status, "error");
        let json = serde_json::to_string(&snapshot).expect("serialize");
        assert!(!json.contains("token"));
        assert!(!json.contains("root"));
    }
}
