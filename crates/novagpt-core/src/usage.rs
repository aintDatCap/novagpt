//! Opt-in, metadata-only accounting for model and tool usage.
//! No prompts, replies, command output, paths, URLs or free-form labels.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet, VecDeque};

pub const MAX_USAGE_RECORDS: usize = 2048;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasurementSource {
    Provider,
    LocalEstimate,
    Unavailable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsageOutcome {
    Ok,
    Error,
    Cancelled,
    Unknown,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenCounts {
    pub input: Option<u64>,
    pub output: Option<u64>,
    /// Subsets of input/output where reported, not additional token usage.
    pub cached_input: Option<u64>,
    pub cache_write: Option<u64>,
    pub reasoning_output: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UsageRecord {
    pub receipt_id: String,
    pub run_id: String,
    pub task_id: Option<String>,
    pub agent_id: Option<String>,
    pub workflow_id: Option<String>,
    pub repository_id: Option<String>,
    pub backend_id: String,
    pub stage: String,
    pub started_unix: u64,
    pub ended_unix: u64,
    pub measurement_source: MeasurementSource,
    pub tokens: TokenCounts,
    /// Cost in millionths of a currency unit, supplied by a provider only.
    pub cost_micros: Option<u64>,
    pub currency: Option<String>,
    pub pricing_revision: Option<String>,
    pub call_count: u32,
    pub outcome: UsageOutcome,
}

fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
}

impl UsageRecord {
    pub fn validate(&self) -> Result<(), &'static str> {
        let ids = [
            Some(self.receipt_id.as_str()),
            Some(self.run_id.as_str()),
            self.task_id.as_deref(),
            self.agent_id.as_deref(),
            self.workflow_id.as_deref(),
            self.repository_id.as_deref(),
            Some(self.backend_id.as_str()),
            Some(self.stage.as_str()),
            self.pricing_revision.as_deref(),
        ];
        if ids.into_iter().flatten().any(|id| !identifier(id)) {
            return Err("invalid opaque usage identifier");
        }
        if self.call_count == 0 || self.call_count > 10000 || self.ended_unix < self.started_unix {
            return Err("invalid usage count or timestamp");
        }
        if self.cost_micros.is_some() != self.currency.is_some() {
            return Err("cost and currency must both be present");
        }
        if let Some(currency) = self.currency.as_deref()
            && (currency.len() != 3 || !currency.bytes().all(|b| b.is_ascii_uppercase()))
        {
            return Err("invalid currency");
        }
        if self.measurement_source != MeasurementSource::Provider && self.cost_micros.is_some() {
            return Err("non-provider usage cannot claim a billed cost");
        }
        if self.measurement_source == MeasurementSource::Unavailable
            && (self.tokens.input.is_some()
                || self.tokens.output.is_some()
                || self.tokens.cached_input.is_some()
                || self.tokens.cache_write.is_some()
                || self.tokens.reasoning_output.is_some())
        {
            return Err("unavailable token usage must not include estimates");
        }
        if self.tokens.cached_input > self.tokens.input {
            return Err("cached input exceeds input tokens");
        }
        if self.tokens.reasoning_output > self.tokens.output {
            return Err("reasoning output exceeds output tokens");
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageTotals {
    pub calls: u64,
    pub records: u64,
    pub errors: u64,
    pub provider_input: u64,
    pub provider_output: u64,
    pub cached_input: u64,
    pub cache_write: u64,
    pub reasoning_output: u64,
    pub estimated_input: u64,
    pub estimated_output: u64,
    pub unavailable_calls: u64,
    pub provider_cost_micros: BTreeMap<String, u64>,
}
impl UsageTotals {
    fn add(&mut self, record: &UsageRecord) {
        self.calls = self.calls.saturating_add(u64::from(record.call_count));
        self.records = self.records.saturating_add(1);
        if record.outcome == UsageOutcome::Error {
            self.errors = self.errors.saturating_add(1);
        }
        match record.measurement_source {
            MeasurementSource::Provider => {
                self.provider_input = self
                    .provider_input
                    .saturating_add(record.tokens.input.unwrap_or(0));
                self.provider_output = self
                    .provider_output
                    .saturating_add(record.tokens.output.unwrap_or(0));
                self.cached_input = self
                    .cached_input
                    .saturating_add(record.tokens.cached_input.unwrap_or(0));
                self.cache_write = self
                    .cache_write
                    .saturating_add(record.tokens.cache_write.unwrap_or(0));
                self.reasoning_output = self
                    .reasoning_output
                    .saturating_add(record.tokens.reasoning_output.unwrap_or(0));
                if let (Some(micros), Some(currency)) = (record.cost_micros, &record.currency) {
                    let total = self
                        .provider_cost_micros
                        .entry(currency.clone())
                        .or_default();
                    *total = total.saturating_add(micros);
                }
            }
            MeasurementSource::LocalEstimate => {
                self.estimated_input = self
                    .estimated_input
                    .saturating_add(record.tokens.input.unwrap_or(0));
                self.estimated_output = self
                    .estimated_output
                    .saturating_add(record.tokens.output.unwrap_or(0));
            }
            MeasurementSource::Unavailable => {
                self.unavailable_calls = self
                    .unavailable_calls
                    .saturating_add(u64::from(record.call_count));
            }
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageReport {
    pub totals: UsageTotals,
    pub by_stage: BTreeMap<String, UsageTotals>,
    pub by_task: BTreeMap<String, UsageTotals>,
    pub by_repository: BTreeMap<String, UsageTotals>,
    pub by_agent: BTreeMap<String, UsageTotals>,
    pub by_workflow: BTreeMap<String, UsageTotals>,
    pub timeline: Vec<UsagePoint>,
    pub duplicates_ignored: u64,
    pub dropped_records: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsagePoint {
    pub unix_seconds: u64,
    pub provider_total: u64,
    pub estimated_total: u64,
}

#[derive(Default)]
pub struct UsageLedger {
    records: VecDeque<UsageRecord>,
    receipts: HashSet<String>,
    duplicates_ignored: u64,
    dropped_records: u64,
}
impl UsageLedger {
    pub fn ingest(&mut self, record: UsageRecord) -> Result<bool, &'static str> {
        record.validate()?;
        if self.receipts.contains(&record.receipt_id) {
            self.duplicates_ignored = self.duplicates_ignored.saturating_add(1);
            return Ok(false);
        }
        if self.records.len() == MAX_USAGE_RECORDS
            && let Some(old) = self.records.pop_front()
        {
            self.receipts.remove(&old.receipt_id);
            self.dropped_records = self.dropped_records.saturating_add(1);
        }
        self.receipts.insert(record.receipt_id.clone());
        self.records.push_back(record);
        Ok(true)
    }
    pub fn records(&self) -> &VecDeque<UsageRecord> {
        &self.records
    }
    pub fn report(&self) -> UsageReport {
        let mut report = UsageReport {
            duplicates_ignored: self.duplicates_ignored,
            dropped_records: self.dropped_records,
            ..UsageReport::default()
        };
        let mut ordered: Vec<_> = self.records.iter().collect();
        ordered.sort_by_key(|r| (r.ended_unix, &r.receipt_id));
        for record in ordered {
            report.totals.add(record);
            report
                .by_stage
                .entry(record.stage.clone())
                .or_default()
                .add(record);
            for (key, group) in [
                (record.task_id.as_ref(), &mut report.by_task),
                (record.repository_id.as_ref(), &mut report.by_repository),
                (record.agent_id.as_ref(), &mut report.by_agent),
                (record.workflow_id.as_ref(), &mut report.by_workflow),
            ] {
                if let Some(id) = key {
                    group.entry(id.clone()).or_default().add(record);
                }
            }
            report.timeline.push(UsagePoint {
                unix_seconds: record.ended_unix,
                provider_total: report
                    .totals
                    .provider_input
                    .saturating_add(report.totals.provider_output),
                estimated_total: report
                    .totals
                    .estimated_input
                    .saturating_add(report.totals.estimated_output),
            });
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: &str, source: MeasurementSource) -> UsageRecord {
        UsageRecord {
            receipt_id: id.into(),
            run_id: "run-1".into(),
            task_id: Some("task-a".into()),
            agent_id: Some("agent-1".into()),
            workflow_id: Some("flow-1".into()),
            repository_id: Some("repo-1".into()),
            backend_id: "api".into(),
            stage: "cargo_test".into(),
            started_unix: 10,
            ended_unix: 11,
            measurement_source: source,
            tokens: TokenCounts {
                input: Some(100),
                output: Some(50),
                cached_input: Some(40),
                cache_write: Some(5),
                reasoning_output: Some(10),
            },
            cost_micros: None,
            currency: None,
            pricing_revision: None,
            call_count: 1,
            outcome: UsageOutcome::Ok,
        }
    }
    #[test]
    fn attribution_exact_sums_and_duplicate_receipts() {
        let mut ledger = UsageLedger::default();
        let mut provider = record("a", MeasurementSource::Provider);
        provider.cost_micros = Some(12500);
        provider.currency = Some("USD".into());
        assert_eq!(ledger.ingest(provider.clone()), Ok(true));
        assert_eq!(ledger.ingest(provider), Ok(false));
        ledger
            .ingest(record("b", MeasurementSource::LocalEstimate))
            .unwrap();
        let report = ledger.report();
        assert_eq!(report.totals.calls, 2);
        assert_eq!(report.totals.provider_input, 100);
        assert_eq!(report.totals.provider_output, 50);
        assert_eq!(report.totals.estimated_input, 100);
        assert_eq!(report.totals.estimated_output, 50);
        assert_eq!(report.totals.cached_input, 40);
        assert_eq!(report.totals.provider_cost_micros["USD"], 12500);
        assert_eq!(report.by_task["task-a"].calls, 2);
        assert_eq!(report.by_agent["agent-1"].records, 2);
        assert_eq!(report.by_repository["repo-1"].records, 2);
        assert_eq!(report.by_workflow["flow-1"].records, 2);
        assert_eq!(report.duplicates_ignored, 1);
        assert_eq!(report.timeline.len(), 2);
    }
    #[test]
    fn web_estimates_cannot_claim_provider_billing() {
        let mut web = record("w", MeasurementSource::LocalEstimate);
        web.backend_id = "chatgpt_web".into();
        web.cost_micros = Some(100);
        web.currency = Some("USD".into());
        assert!(web.validate().is_err());
        web.cost_micros = None;
        web.currency = None;
        assert!(web.validate().is_ok());
        web.measurement_source = MeasurementSource::Unavailable;
        assert!(web.validate().is_err());
        web.tokens = TokenCounts::default();
        assert!(web.validate().is_ok());
    }
    #[test]
    fn missing_provider_usage_and_rejected_content() {
        let mut provider = record("p", MeasurementSource::Provider);
        provider.tokens = TokenCounts::default();
        assert!(provider.validate().is_ok());
        provider.stage = "https://private.example/token".into();
        assert!(provider.validate().is_err());
        provider.stage = "cargo_test".into();
        provider.tokens.cached_input = Some(10);
        provider.tokens.input = Some(3);
        assert!(provider.validate().is_err());
    }
    #[test]
    fn bounded_ledger_rotation() {
        let mut ledger = UsageLedger::default();
        for i in 0..(MAX_USAGE_RECORDS + 4) {
            ledger
                .ingest(record(&format!("r{i}"), MeasurementSource::Provider))
                .unwrap();
        }
        assert_eq!(ledger.records().len(), MAX_USAGE_RECORDS);
        assert_eq!(ledger.report().dropped_records, 4);
    }
}
