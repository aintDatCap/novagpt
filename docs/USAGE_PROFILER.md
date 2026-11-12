# Private Usage Profiler (Phase A)

The opt-in NovaGPT usage profiler records **only bounded counters and opaque identifiers**. It does not record prompt bodies, assistant responses, shell output, browser page contents, file paths, connector URLs, credentials, or raw tool arguments. The existing audit trail is independent and remains enabled as before.

## Enable and inspect

1. In the terminal UI, open **Settings > Usage profiler (private)**. Press **Enter** to enable it, **Save**, and restart the MCP server for the new setting to apply. It is **disabled by default**, including for existing configurations.
2. Open **Usage** (Italian: **Consumi**) in the TUI. The page displays a summary, a cumulative token chart (separate series for provider and estimates), and calls per tool. Press **R** to reload retained usage from disk. The charts have an explicit empty state.
3. To inspect from a noninteractive terminal, use `novagpt --root <approved-workspace> usage report`. Add `--json` for a sanitized, typed JSON summary.

No external analytics service or new dependency is required. The command reads the private per-workspace state directory, outside the repository, without needing an MCP connection. Enabling requires a saved per-user preference; simply viewing a report never enables collection.

## What the numbers mean

- `measurement_source = provider`: a trusted API-backed integration supplied token usage. Provider totals, cache input, cache writes, reasoning output, and billed cost are counted **only when reported**.
- `measurement_source = local_estimate`: the backend does **not** expose token usage. For ChatGPT Web subagent turns, NovaGPT estimates the prompt and returned-text token counts as **ceil(characters / 4)**. For MCP tools, it estimates the serialized response length with the same heuristic. Estimates are **not billing data** and must not be added to provider totals when discussing cost.
- `measurement_source = unavailable`: the caller has no measured or estimated token data. Missing provider fields remain missing in a `UsageRecord`, not silently guessed.
- A `cost_micros` field is accepted only with `provider` measurements and a three-letter uppercase currency. Values are integer micro-units to avoid floating point pricing errors. The profiler does not maintain pricing tables, infer prices, or estimate ChatGPT Web billing.
- Subagent turns have opaque task, agent and repository IDs. Ordinary MCP tool responses have a repository ID and a tool stage, but **do not claim an agent, task or ChatGPT conversation**: transport connections do not provide reliable conversation identity.
- A duplicate `receipt_id` is accepted once within retained history. The typed core ledger aggregates per stage, task, repository, agent, and workflow. There is no new public MCP tool or client-facing schema change.

**Limitations:** This first integration collects MCP response-length estimates and ChatGPT Web subagent turn-length estimates. NovaGPT does not yet have an API-backed model execution path that supplies billed provider usage, so the provider series and real cost can remain empty. The standalone TUI chat does not currently report a task profile. An empty provider chart means **no provider records**, not zero model cost.

## Privacy, retention and failure modes

- The profiler is opt-in. When disabled, it creates no profiler files, queues, or background writer threads.
- The enabled profiler retains at most **2,048** records in memory, with a **512 KiB** metadata-only file quota and a **30-day** time window. It rewrites the file on opt-in startup to expire old data and compacts it on quota pressure.
- The profiler shares the audit directory's per-workspace isolation and owner-private directory validation, but persists to a separate `usage.jsonl` file. It refuses redirected (symlink) files, oversized storage and unsafe Unix file permissions.
- Writes use a bounded asynchronous writer queue. If writes fail or the queue overflows, counts can be incomplete; a test report is not a billing receipt. A crash can lose queued events. The UI deliberately does not display fake zero-cost or invented provider usage.
- Turning the setting off stops **new** collection after the server restarts; it does not silently erase previously retained records. To delete old records, stop the server and remove only the per-workspace `usage.jsonl` in the private path shown by `novagpt --root <approved-workspace> logs path`. Do not remove the audit history unless intended.

## Reproducing and extending the baseline

The typed contract lives in `crates/novagpt-core/src/usage.rs`, persistence and ingestion in `crates/novagpt-mcp/src/usage.rs`, and the CLI/TUI configuration and report in `crates/novagpt-cli`. API-backed agents can call the trusted in-process `UsageProfiler::record` with exact provider receipts. Do not try to infer provider usage from ChatGPT Web.

Unit fixtures cover exact aggregations, duplicate receipts, invalid identifiers and impossible token subsets, opt-out, persistence/reopen, storage checks and bounded rotation. Ratatui's chart/dataset and bar-chart widgets are used without additional dependencies:
- [Ratatui Chart](https://docs.rs/ratatui/0.30.0/ratatui/widgets/struct.Chart.html)
- [Ratatui datasets](https://docs.rs/ratatui/0.30.0/ratatui/widgets/struct.Dataset.html)
- [Ratatui BarChart](https://docs.rs/ratatui/0.30.0/ratatui/widgets/struct.BarChart.html)

For a credible token/cost improvement claim, follow the protocol in [AGENT_CONTEXT_EFFICIENCY_ROADMAP.md](AGENT_CONTEXT_EFFICIENCY_ROADMAP.md): at least 20 pinned representative tasks, independently verified results, comparable trials, and measured total cost per accepted task. This profiler itself is **not** evidence that token consumption has fallen.
