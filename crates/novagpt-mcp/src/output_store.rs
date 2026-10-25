//! Workspace-scoped, process-local retained Bash output (never persisted to disk).
//! Four entries maximum; paging uses UTF-16 offsets matching the TS wire contract.
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use uuid::Uuid;

const MAX_ENTRIES: usize = 4;
const MAX_CAPTURE: usize = 64 * 1024;

#[derive(Debug)]
struct Entry {
    id: String,
    workspace_id: String,
    stdout: String,
    stderr: String,
    incomplete: bool,
}

#[derive(Debug, Serialize)]
pub struct OutputPage {
    pub text: String,
    pub output_resource_id: String,
    pub stream: String,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub total_chars: usize,
    pub incomplete: bool,
}

#[derive(Clone, Default)]
pub struct OutputStore {
    entries: Arc<Mutex<VecDeque<Entry>>>,
}

fn bounded(value: &str) -> String {
    value.chars().take(MAX_CAPTURE).collect()
}

fn page(
    entry: &Entry,
    stream: &str,
    offset: usize,
    max_chars: usize,
) -> Result<OutputPage, &'static str> {
    if !(2..=8000).contains(&max_chars) {
        return Err("Invalid output page bounds");
    }
    let source = match stream {
        "stdout" => &entry.stdout,
        "stderr" => &entry.stderr,
        _ => return Err("Output stream must be stdout or stderr"),
    };
    let units: Vec<u16> = source.encode_utf16().collect();
    let len = units.len();
    let start = offset.min(len);
    if start > 0 && start < len && (0xDC00..=0xDFFF).contains(&units[start]) {
        return Err("Output page offset splits a Unicode surrogate pair");
    }
    let mut end = (start + max_chars).min(len);
    if end < len && end > start && (0xD800..=0xDBFF).contains(&units[end - 1]) {
        end -= 1;
    }
    let text = String::from_utf16(&units[start..end]).map_err(|_| "Invalid Unicode page")?;
    Ok(OutputPage {
        text,
        output_resource_id: entry.id.clone(),
        stream: stream.into(),
        offset: start,
        next_offset: (end < len).then_some(end),
        total_chars: len,
        incomplete: entry.incomplete,
    })
}

impl OutputStore {
    pub fn save(
        &self,
        workspace_id: &str,
        stdout: &str,
        stderr: &str,
        truncated: bool,
    ) -> Result<String, &'static str> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| "Output store unavailable")?;
        let id = format!("output_{}", Uuid::new_v4());
        entries.push_back(Entry {
            id: id.clone(),
            workspace_id: workspace_id.into(),
            stdout: bounded(stdout),
            stderr: bounded(stderr),
            incomplete: truncated
                || stdout.chars().count() > MAX_CAPTURE
                || stderr.chars().count() > MAX_CAPTURE,
        });
        while entries.len() > MAX_ENTRIES {
            entries.pop_front();
        }
        Ok(id)
    }

    pub fn read(
        &self,
        workspace_id: &str,
        id: &str,
        stream: &str,
        offset: usize,
        max_chars: usize,
    ) -> Result<OutputPage, &'static str> {
        let entries = self
            .entries
            .lock()
            .map_err(|_| "Output store unavailable")?;
        let found = entries
            .iter()
            .find(|entry| entry.id == id && entry.workspace_id == workspace_id)
            .ok_or("Output unavailable for this workspace or expired")?;
        page(found, stream, offset, max_chars)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_scoped_bounded_retention_and_unicode_paging() {
        let store = OutputStore::default();
        let id = store
            .save("ws_a", "start🔑end", "error", false)
            .expect("save");
        assert!(store.read("ws_b", &id, "stdout", 0, 8).is_err());
        let p1 = store.read("ws_a", &id, "stdout", 0, 6).expect("page");
        assert_eq!(p1.text, "start");
        assert_eq!(p1.next_offset, Some(5));
        let p2 = store.read("ws_a", &id, "stdout", 5, 2).expect("emoji");
        assert_eq!(p2.text, "🔑");
        assert_eq!(p2.next_offset, Some(7));
        assert!(store.read("ws_a", &id, "stdout", 6, 2).is_err());
        assert!(store.read("ws_a", &id, "stderr", 0, 1).is_err());
        for _ in 0..4 {
            store.save("ws_a", "next", "", false).expect("save");
        }
        assert!(store.read("ws_a", &id, "stdout", 0, 100).is_err());
    }

    #[test]
    fn large_values_are_marked_incomplete() {
        let store = OutputStore::default();
        let id = store
            .save("ws_a", &"a".repeat(MAX_CAPTURE + 500), "", false)
            .expect("save");
        let first = store.read("ws_a", &id, "stdout", 0, 8000).expect("page");
        assert_eq!(first.total_chars, MAX_CAPTURE);
        assert!(first.incomplete);
    }
}
