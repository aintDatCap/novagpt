# Agent Context Efficiency Roadmap

**Status:** Proposal; no feature in this roadmap is considered implemented by this document.
**Scope:** NovaGPT core, MCP adapters, optional browser and agent modules, and an optional external workflow orchestrator.
**Goal:** Reduce the total cost of *correctly completed* agent development tasks. Preserve correctness, security, auditability, and access to necessary evidence.

This is a planning document. It does not authorize unattended execution, remote access, new dependencies, or changes to other repositories. Follow [AGENTS.md](../AGENTS.md), [ARCHITECTURE.md](ARCHITECTURE.md), [ENGINEERING_STANDARDS.md](ENGINEERING_STANDARDS.md), [QUALITY_GATES.md](QUALITY_GATES.md), and [CAPABILITIES.md](CAPABILITIES.md). Implement each phase through small, independently reviewed changes.

## 1. Principles and current baseline

1. Filter irrelevant information **before** it enters a model request. Do not ask an expensive model to summarize a large result if a deterministic parser can select the relevant information first.
2. Preserve evidence. A concise view is not proof that omitted data is unimportant. Keep a bounded, access-controlled way to request more context, and mark any irreversible truncation.
3. Measure whole-task outcomes. Fewer tokens in one tool response can cause extra tool calls, repeated work, regressions, or higher total cost.
4. Separate deterministic work from language-model decisions. Use parsers, compilers, tests, linters, and policy checks first.
5. Use existing crate boundaries. Put domain behavior in the owner module, expose narrow typed APIs, and keep the MCP layer thin. Do not create a new crate without a justified dependency boundary and an ADR.
6. Roll out behind explicit options. Keep existing tool behavior and schemas compatible unless a separately approved versioned change is justified.
7. Do not export private source, browser content, secrets, token strings, or raw command output to telemetry or an external compression service.

Existing capabilities to reuse:

| Capability | Current location | Limitation or opportunity |
| --- | --- | --- |
| Bounded shell capture | `crates/novagpt-core/src/process_output.rs`, `command_ops.rs` | Captures up to 64 KiB per stream and can discard the rest; no command-aware summary. |
| Output paging | `crates/novagpt-mcp/src/output_store.rs` | Process-local, four retained entries, bounded capture; not a durable full-output archive. |
| Instruction fingerprints | `crates/novagpt-core/src/context_ops.rs` | Can omit unchanged AGENTS.md text when the caller supplies the same fingerprint. |
| Explicit context export | `crates/novagpt-core/src/pro_context.rs` | Limited, explicitly selected files; not a semantic repository map. |
| Browser reading | `crates/novagpt-browser/src/lib.rs` (`browse_read`) | Returns an early slice of visible body text, including page chrome and navigation. |
| Optional browser and subagents | `crates/novagpt-browser`, `crates/novagpt-agents` | Browser automation is not a reliable unattended scheduling service; keep it optional. |
| Private metadata audit | `crates/novagpt-mcp/src/audit.rs` | Appropriate starting point for bounded counts, not prompt or content capture. |

These observations are a snapshot of the local code. Recheck each one before implementation because the repository is under active development.

## 2. Evaluation protocol used by every phase

### 2.1 Establish a baseline first

- Prepare at least **20 representative tasks**: bug fixes, Rust refactoring, test failures, document maintenance, dependency updates, and repository investigation. Include small and large repositories, successful and failed commands, and tasks requiring follow-up reads.
- Prepare **fixed fixtures** for tool-level tests: Git diffs, Cargo/Clippy/test logs, HTML article pages, app screens, Markdown documentation, and symbol-rich source trees.
- Record the task description, base commit, model and version, settings, tool schemas, token budget, OS, run date, and expected acceptance checks. Never benchmark on a mutable worktree.
- For stochastic agent runs, repeat each variant on matched fixtures (target three runs per case, or report why not). Compare baseline with each feature *alone* and then combined.
- Use provider-reported usage for API runs when available; distinguish input, cached input, cache writes, output, reasoning tokens if reported, and monetary cost. For ChatGPT Web, label local tokenizer/character counts as **estimates**, not billing data.

### 2.2 Measures and decision rules

| Metric | How to measure | Why |
| --- | --- | --- |
| Cost per accepted task | Total provider cost of every attempt, including retries, divided by tasks passing independent checks | Primary economic measure |
| End-to-end tokens | Sum all model request and response usage per task, not one response alone | Detects retries and context re-reads |
| Task success | Independent tests, diff review, or a predefined human rubric | Prevents false savings |
| Extra tool calls | Calls needed to restore hidden or missing context | Detects over-compression |
| Latency and CPU/RAM | Wall-clock task duration and worker resource use | Avoids moving costs to local compute |
| Diagnostic retention | Essential failure messages and source locations preserved versus fixture ground truth | Safety and usability |
| Compatibility | MCP contract tests, clients, and explicit error behavior | Avoids breaking current users |
| Sensitive data exposure | Redaction tests and review of all stored and emitted fields | Blocks unsafe telemetry |

Decision policy:

- **Ship** only if independent acceptance checks do not regress, security/compatibility gates pass, and the paired trial shows a credible improvement in cost, latency, or user effort.
- **Investigate** if token savings come with more retries, missing diagnostics, or unstable outcomes.
- **Reject or disable** an optimization that hides failures, loses required evidence, exposes secrets, or makes previously correct tasks fail.
- Do not state a universal percentage of token savings. Set quantitative rollout targets only after the baseline exists; publish confidence intervals or raw distributions for small samples.
- Use explicit PASS, FAIL, NOT RUN, and BLOCKED states. An agent's written claim is never test evidence.

### 2.3 Evidence and rollback

Every implementation PR must include: fixture identifiers, before/after usage table, actual command/test results, negative cases, compatibility effects, new dependencies, failure modes, feature-flag default, and rollback instructions. Use disposable worktrees, no production or personal data. Update capability status only when verification exists.

## 3. Phase A (P0): Token and Cost Profiler

**Need.** Without usage measurement, we cannot determine whether compact output, document rewrites, or prompt caching reduce end-to-end cost. NovaGPT currently records bounded activity metadata, but API billing usage and total agent-task cost are not a unified contract.

**Design.**

1. Introduce an opt-in, typed `UsageRecord` with run ID, task ID, opaque model/backend identifier, tool or stage name, timestamps, measured or estimated token counts, cache usage when reported, cost currency and pricing revision where known, call count, and outcome. Include a `measurement_source` field: `provider`, `local_estimate`, or `unavailable`.
2. Accept usage from API-backed agents and deterministic local measurements. Do not fabricate provider usage for ChatGPT Web.
3. Aggregate per tool, task, repository, agent, and workflow. Keep per-task accounting separate from transport sessions: an MCP connection is not a ChatGPT conversation identity.
4. Emit fixed-schema, bounded **metadata only**. Never log prompt bodies, browser text, file contents, raw shell output, connector URLs, or credentials.
5. Keep pricing/configuration outside the telemetry records. Make storage private, capped by age and size, and disabled by default until retention and access policy are reviewed.
6. Export a sanitized summary (JSON/CSV may be added after typed data and retention work). A minimal CLI report is enough initially; do not build a new dashboard first.

**Likely owners.** `novagpt-core` for domain types if shared; `novagpt-mcp` for protocol metadata; `novagpt-cli` for configuration and user-facing reports. Use the existing audit architecture where appropriate without storing sensitive arguments.

**Verification.** Test exact sums and attribution, missing provider fields, repeated receipts/idempotency, concurrent tasks, restart/rotation limits, redaction, invalid or negative prices, and Web-versus-API measurement labeling. Compare reports with known fixture usage. Confirm zero raw-content fields in stored records.

**Done when.** One reproducible baseline report covers a full task; provider numbers reconcile with the supplied usage fixture; estimates are visibly labeled; private retention and opt-out work; existing audit behavior remains unchanged.

## 4. Phase B (P0): Structured Output Engine

**Need.** Verbose command output can consume context. Blindly truncating logs can hide the exact error an agent needs. NovaGPT already has bounded capture and short-lived output paging, but does not select diagnostics by command semantics.

**Design.**

1. Add a cohesive internal output-analysis module under `novagpt-core` first. Define typed outcomes with exit code, duration, command-family classification, findings, warning/error counts, source references, `incomplete`, and a reference to captured output when permitted.
2. Recognize **verified command families**, not arbitrary shell substrings. Start with `cargo test`, `cargo check`, `cargo clippy`, `cargo fmt`, and selected Git commands. Keep a generic bounded fallback for unknown commands and unsupported/localized output.
3. Provide opt-in views: `raw` (existing behavior), `compact` (status and essential diagnostics), and `diagnostic` (errors with locations and relevant context). The existing MCP response remains the compatibility baseline.
4. Keep stdout and stderr distinct; do not turn nonzero exits into success. Show parse confidence and `incomplete`/truncation flags. Do not summarize compiler errors using a language model in the first iteration.
5. Improve output retention only with a threat model: bounded per-workspace quotas, short TTL, opaque scoped IDs, explicit lifecycle, and safe cleanup. A *full* output cannot be recovered after a capture-time cap; never claim otherwise.
6. Keep log access authenticated and workspace-scoped. Avoid arbitrary writes of potentially secret-rich output to disk. Consider memory pressure, concurrent readers, Unicode offsets, and OS-specific command formatting.
7. Add a deterministic command-plan runner later only if needed, so a group of formatter/linter/test steps can return one typed status without one model call per step.

**Likely owners.** `crates/novagpt-core/src/command_ops.rs`, `process_output.rs`; `crates/novagpt-mcp/src/output_store.rs` and thin MCP adapters.

**Verification.** Golden fixtures for passing/failing Cargo logs, compiler spans, warnings, colored/ANSI output, interleaved streams, partial UTF-8, localization, nested test errors, huge output, timeout, unknown shell command, and unexpected format changes. Measure essential-error recall against manually labeled failures. Test paging, expiration, concurrency, denied workspace IDs, and sensitive data handling.

**Done when.** All critical exit codes and failure locations survive compact mode on the fixture suite; `raw` behavior is compatible; failures are never mislabeled as success; actual task trials show less total cost or faster resolution without more failed tasks. Keep compact mode opt-in until demonstrated.

## 5. Phase C (P0): Web Content Extractor

**Need.** `browse_read` currently returns visible `document.body.innerText` with an early size limit. Menus, headers, cookie notices, and footers may displace relevant information. An article parser must not remove interactive elements during UI testing.

**Design.**

1. Offer explicit extraction modes: `article` (main prose with headings, code, tables, and links), `section` (selected heading or guarded selector), `ui` (visible controls, labels, roles, errors, and structure), and `raw` (current visible-text behavior).
2. Classify the task or require a mode; do not silently convert interactive app screens into article mode. Make source URL/title, extraction mode, content range, dropped-content warning, and truncation status clear.
3. Select meaningful content **before** enforcing a character/token budget. Preserve source anchors or stable references for targeted follow-up reads. Ensure expansion and raw fallback remain available.
4. Prefer local extraction. Evaluate Mozilla Readability, Trafilatura, and Defuddle as *design references*; do not introduce a Node.js/Python runtime or a third-party web service by default. Review dependencies and licenses first.
5. Maintain approved-origin checks, browser tab ownership, DOM visibility rules, bounded results, and prompt-injection defenses. Do not treat web text as instructions. Do not send private browsing content to external readability services.
6. Handle dynamic pages, missing main elements, code documentation, navigation-only pages, non-English text, tables, and multiple article candidates. Fail visibly when extraction confidence is low.

**Likely owner.** `novagpt-browser` behind a narrow typed interface; `novagpt-mcp` for optional parameters and compatibility tests. This is **not** a change to the ChatGPT subagent composer.

**Verification.** Offline HTML/DOM fixtures with ground-truth main content and UI controls; compare completeness, navigation-noise ratio, recovered links/code, total tokens, and requested follow-up reads. Test origin redirects, hidden/password elements, malformed pages, Unicode and truncation, and real browser behavior on disposable local pages.

**Done when.** Article mode retains required information and removes measurable irrelevant text; UI mode retains required controls and errors; raw mode remains accessible; no security policy is weakened. Do not ship article mode as a universal default.

## 6. Phase D (P1): Repository Context Engine

**Need.** Agents often open full files before locating the relevant function. A repository map can show symbols and relationships first, then provide exact source slices on request.

**Design.**

1. Reuse safe workspace tree, search, and grep operations. Add progressive responses: (1) candidate files, (2) symbols/imports/references, (3) exact spans or function bodies, (4) full files on demand.
2. Evaluate a Rust-native Tree-sitter integration and language coverage with real fixtures. Start with Rust, then extend only for projects in use. A symbol index is a navigation aid, not a compiler or a proof of unused code.
3. Build cache entries keyed by canonical workspace, file content hash, parser version, and language. Use bounded storage and invalidation on edits, worktree changes, Git checkout, rename, and parser upgrade.
4. Return stable path, line/byte range, content hash, and source provenance. Guard reads again at retrieval time. Support ambiguous symbols and unsaved/untracked changes.
5. Prefer relevance from explicit task terms, import/call relations, and deterministic search before model-based reranking. No indexing of ignored, blocked, sensitive, or external paths.
6. Retain a direct full-file route. Do not remove comments or rely on signatures alone when correctness depends on implementation details.

**Likely owner.** `novagpt-core` search/context modules with thin MCP interfaces. Add new language parsers only after coverage and maintenance cost are justified.

**Verification.** Reproducible multi-crate fixtures with a labeled set of target symbols, references, and regression tests. Measure top-k relevant-file recall, correct line spans, tokens used to reach the correct implementation, false negatives, cache invalidation, memory, and index time. Test symlinks/junctions, denied files, concurrent writes, conflicting worktrees, and large repos.

**Done when.** The agent finds relevant code with fewer total reads and no loss of successful fixes; all returned spans map to the current source hash; caching does not return stale source after edits; fallback searches remain available.

## 7. Phase E (P1): Agent Context Manager

**Need.** Long tasks repeat instructions, tool outputs, and repository exploration. Passing an unstructured chat transcript to every subagent wastes context and increases ambiguity.

**Design.**

1. Define a versioned task-checkpoint schema: task ID, objective, base commit/worktree, assigned scope, decisions, known risks, changed paths, verification receipts, unresolved issues, and references to permitted evidence. Keep assertions separate from independently verified facts.
2. Store checkpoints privately with bounded TTL/quota, atomic writes, ownership, and access checks. A checkpoint must be recoverable after restart without automatically replaying ambiguous actions.
3. Deduplicate repeated document/tool results using validated content hashes. Extend the existing AGENTS.md fingerprint mechanism rather than replacing it. Load only task-relevant skills and instructions; an external skill cannot override AGENTS.md or security policy.
4. Provide explicit context budgets and retrieval controls. A subagent receives a concise assignment and verified references, not every prior terminal log. If content is missing, it can request the original source.
5. For API-backed agents, investigate stable prompt prefixes, provider caching, and supported compaction. Record cached input separately from noncached input. For ChatGPT Web, do not assume access to provider cache or reliable billing counters.
6. Make task handoff idempotent and auditable: received artifact ID, source hash, task status, and acknowledgment. Never turn a model summary into evidence that tests passed.
7. Treat model-generated summaries and retrieved web pages as untrusted data; block prompt-injection attempts to change tool privileges, workspace scope, or approval policy.

**Likely owners.** Existing `novagpt-agents` and core context/handoff modules; MCP exposes narrow typed operations. The orchestrator owns cross-task scheduling, not transport identity.

**Verification.** Multi-agent fixtures with long histories, repeated instructions, failed and resumed tasks, stale checkpoints, missing evidence, conflicting writes, and intentionally misleading summaries. Compare token usage, successful task completion, repeated file reads, time to resume, and scope violations. Test redaction and cleanup after cancellation.

**Done when.** A new agent can resume a fixture from checkpoint and complete it correctly without the full transcript; the checkpoint does not claim unverified actions; context savings do not increase task failures or unauthorized reads.

## 8. Phase F (P1): Windmill and hybrid worker integration

**Need.** Scheduled maintenance should not require ChatGPT Web or keep a development PC online. The orchestration service should decide *when* a task runs; NovaGPT should execute only work explicitly authorized for its workspace.

**Design.**

1. Treat **Windmill as an optional external orchestrator**, not a mandatory NovaGPT runtime dependency. Host its controller and dashboard separately from heavy builds. If deployed on a production VPS, enforce CPU/RAM/storage limits and separate credentials/databases.
2. Define a small versioned task contract: workflow ID, opaque task ID, repository/workspace reference, requested procedure and version, allowed operations, resource budget, deadline, artifact refs, and required approval policy.
3. The first flow should be: trigger -> deterministic analysis -> optional AI proposal -> isolated worktree -> deterministic quality gates -> reviewable report or PR. Use independent evidence for gate results.
4. Prefer API-backed agents for unattended work. Keep the existing ChatGPT Web browser as an opt-in interactive backend, not a guaranteed 24/7 autonomous worker.
5. Use outbound authenticated worker registration or another explicitly approved private channel; do not expose unrestricted host-shell tools publicly. Apply least privilege, credential separation, bounded jobs, and cancellation semantics.
6. Avoid duplicate scheduling, destructive retries, and cross-workspace confusion. An uncertain submitted action must not be replayed automatically. Preserve the task state and require reconciliation.
7. Keep test/build workloads off the Ace Blocker production VPS unless a verified resource budget and isolation policy allow them. Use GitHub Actions or isolated remote workers for heavy deterministic checks.
8. Start with documentation audit and dependency-review workflows. Add security/legal-compliance assistance only with domain-specific policy and human approval; AI reports are not legal certification.

**Likely owners.** A narrow NovaGPT adapter and workflow contracts plus version-controlled Windmill definitions in a separately approved infrastructure repository, if needed. Do not add a new NovaGPT crate or Docker service until concrete interfaces are validated.

**Verification.** Use a disposable repository and simulated scheduled triggers. Test duplicate events, offline workers, server restart, expired credentials, denial of unknown workspaces, cancellation, task timeout, Git conflicts, idempotent retries, budget exhaustion, and verified gate failure. Verify that a worker cannot modify unrelated files or merge without approval. Test load with production services isolated.

**Done when.** A full maintenance workflow completes with accurate state and reviewable evidence, can resume after failure without duplicate mutation, and never relies on an unattended ChatGPT Web tab. No deployment is authorized merely by completion of this document.

## 9. Delivery order and dependencies

| Step | Deliverable | Depends on | Rollout |
| --- | --- | --- | --- |
| 0 | Baseline task suite, fixtures, metrics and success rubric | None | Measurement only |
| A | Usage/cost profiling and sanitized reports | 0 | Opt-in |
| B | Structured shell output and safe detail retrieval | 0, preferably A | Explicit mode; raw default |
| C | Article/section/UI extraction in browser read | 0, preferably A | Explicit mode; raw fallback |
| D | Symbol-first repository navigation and cache | 0, preferably A | Opt-in |
| E | Verified task checkpoint and scoped context reuse | A, then B/D as useful | Opt-in |
| F | External scheduler and hybrid-worker contract | A + stable verification receipts | Independent pilot |

B and C can proceed in parallel after the evaluation harness is ready. Do not wait for Windmill to start work on NovaGPT's tool outputs. Revisit the order after collecting evidence.

### Suggested first implementation slice

1. Add synthetic terminal and page fixtures, plus a script/report definition for comparing baseline and candidate runs.
2. Add a **single opt-in** compact parser for one deterministic command family (such as `cargo test`) with a fallback to existing raw output and evidence of truncation.
3. Validate on passing, failing, partial, and huge-output fixtures; run one small matched agent-task benchmark.
4. Expand to `cargo clippy` and a minimal `browser_read` article extraction mode only if the first benchmark does not show harmful regressions.
5. Update quality gates, capability status and the documented defaults only after demonstrated behavior.

## 10. Explicit non-goals and cautions

- No claims of fixed token-saving percentages, universal command compression, automatic legal compliance, or autonomous security certification.
- No forced switch to STE100 for all text. Controlled English can improve technical clarity, but shorter text and fewer tokens are separate hypotheses. Compare original, STE100, concise, and STE100-plus-concise documents using the real model tokenizer **and** comprehension/task-success tests.
- No automatic deletion of comments, legacy code, docs, test logs, or evidence based solely on a relevance score.
- No third-party cloud extraction of private browser or repository content by default.
- No bypass of browser challenges, account restrictions, or provider terms.
- No forced model compaction, prompt-cache assumption, or changes to another agent's worktree.
- No silent change to existing MCP output defaults. Breaking protocol changes need an ADR, contract tests, and a migration path.

## 11. Completion report template

For each delivered slice, record:

- **Claim:** which measurable user problem it solves.
- **Files and public contract:** exact modules, schema/defaults, and compatibility impact.
- **Evidence:** fixture set, independent test results, platform, commit, and raw outcome references.
- **Baseline vs candidate:** accepted tasks, median and distribution of total tokens, cached tokens, cost per accepted task, latency, tool calls, and extra retrievals.
- **Security:** retained data, expiry, auth/guards, sensitive-data tests, and remaining risks.
- **Decision:** SHIP, ITERATE, REJECT, or BLOCKED, with reasons.
- **Rollback:** exact feature flag or previous behavior.
- **Follow-up:** remaining platforms, corner cases, and unverified claims.

Documentation-only edits require a link/path check and Git diff inspection. Do not report Rust tests as run when they were not run.
