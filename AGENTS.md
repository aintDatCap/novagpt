# AGENTS.md — NovaGPT agent contract

These rules apply to the entire NovaGPT repository. Read this file before changing code.
Read the more specific documents linked below when the task touches their area.

## Mission and non-negotiable behavior

NovaGPT is a **separate Rust rewrite** of CodexPro, not an in-place replacement.
**Never modify, move, overwrite, reset, or deploy the CodexPro repository, launcher, profiles, or user data as part of a NovaGPT task.**
CodexPro TypeScript is a temporary behavioral oracle; parity is tracked in [docs/PARITY.md](docs/PARITY.md).

The core product contract is **one server, many concurrent ChatGPT conversations, the same explicitly allowed real filesystem/Git tree**. An MCP transport session, token, IP, tab, or browser profile must never be treated as a ChatGPT conversation identity. Basic tools must not require client IDs, leases, dedicated copies, or worktrees. An omitted `workspace_id` always addresses the configured default root; operations on other roots must target them explicitly.

Optimize for correctness, safety, clarity, small reviewable changes, and maintainability by human and LLM contributors; runtime speed is secondary. Never claim a feature is complete merely because it compiles.

## Required workflow for each task

1. **Orient:** read the task, relevant module, [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), [docs/ENGINEERING_STANDARDS.md](docs/ENGINEERING_STANDARDS.md), and relevant tests; inspect Git status. Preserve unrelated edits from other agents.
2. **Define a small slice:** state the owning crate/module, invariants, public contract, likely failure modes, and test plan. Keep responsibilities in the correct layer.
3. **Implement only the slice:** change the smallest set of files that completes an observable behavior; use typed requests/results and domain errors. Avoid cross-crate refactors unless separately justified.
4. **Prove behavior:** add focused unit/integration tests including denial, conflict, cancellation or resource limits when applicable. Compare CodexPro fixtures when changing MCP/CLI compatibility.
5. **Verify:** run the relevant commands below; report exactly which passed, failed, were skipped or were unavailable. Never fabricate test output or treat existing test files as evidence of passes.
6. **Review and hand off:** check diff for secrets, scope creep, added dependencies, changed API/schema, concurrency hazards and outdated docs. Update [docs/PARITY.md](docs/PARITY.md) only with actual evidence; name known limitations.

If several agents work simultaneously, assign **nonoverlapping files or modules**. Communicate public API assumptions in advance, use separate branches/worktrees when helpful, and coordinate changes to shared Cargo manifests, schemas, Git index and release files. Do not assume independent writes are conflict-free.

## Ownership and dependency direction

- `novagpt-core`: domain types, canonical workspace IDs, allowed-root/path guards, safe filesystem, CAS, search, Git/command policy and bounded operations. **No `rmcp`, HTTP, CLI or browser dependency.**
- `novagpt-mcp`: official `rmcp` protocol, tool schemas, dispatch, response envelopes, HTTP/SSE and stdio, transport session lifecycle and authentication. Call core services; do not implement filesystem safety here.
- `novagpt-cli`: CLI parsing, configuration/profile policy, composition/startup, platform launch integrations. No duplicated core business logic.
- Optional VM, browser, subagent, handoff, widgets and history modules must be independently switchable; the core file/Git workflow cannot depend on them.

The dependency graph is `cli -> mcp -> core` (with `cli -> core` also permitted), never the reverse. Begin with cohesive **modules within existing crates**; create a new crate only for a genuine ownership/deployment/dependency boundary backed by an ADR. Keep APIs narrow, types meaningful, modules cohesive and dependencies acyclic. See [docs/ENGINEERING_STANDARDS.md](docs/ENGINEERING_STANDARDS.md).

## Safety and runtime rules

- Deny paths outside approved canonical roots; reject blocked paths, symlink escapes, binary/oversize reads, unsafe commands and unexpected write targets. Revalidate after acquiring mutation locks; test TOCTOU and Windows-specific reparse/drive behavior before asserting parity.
- Treat `expected_sha256` as optimistic concurrency control; fail stale writers without clobbering newer bytes. Never claim multi-file atomicity unless demonstrated.
- No global lock around unrelated work. State ownership and cancellation/timeout/cleanup of commands, sessions, streams, VMs and workers explicitly.
- Never log credentials, bearer tokens, environment secrets, private file contents or full unbounded tool outputs. Errors sent to MCP clients must be redacted and stable.
- Avoid `unwrap`, `expect`, `panic!`, unchecked indexing and opaque `anyhow` chains in production paths when a typed error is possible; tests may use `expect` for local fixtures.
- For async code: do not block Tokio tasks with filesystem or process calls; use suitable async APIs or bounded `spawn_blocking`, with a cancellation story. Never hold a synchronous mutex guard across `.await`.
- No unnecessary fallback that hides a failed invariant. No blanket `allow` lints, placeholder tests, unsafe Rust or extra dependencies without documented review.

## Checks and completion

For Rust source changes, run from repository root:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
```

Run protocol/security/concurrency integration tests and Windows-specific tests when changing the relevant area. A release or distribution change additionally requires the relevant release builds and the acceptance gates in [docs/QUALITY_GATES.md](docs/QUALITY_GATES.md). Documentation-only changes require link/path review and a diff inspection; do **not** invent a Rust test result for them.

For each change report: summary, exact files, commands with real outcomes, compatibility impact, new dependencies, risks and follow-ups. Use small meaningful commits only when explicitly asked/authorized; never force-push or rewrite unrelated history. The migration remains incomplete while required entries in `docs/PARITY.md` are `INITIAL` or `NOT STARTED`.

## Where to look next

- [Architecture](docs/ARCHITECTURE.md): actual system boundaries and known limitations.
- [Engineering standards](docs/ENGINEERING_STANDARDS.md): design, dependency, module, API and error conventions.
- [Extension playbook](docs/EXTENSION_PLAYBOOK.md): how to add MCP tools, core features and optional subsystems.
- [Quality gates](docs/QUALITY_GATES.md): testing matrix, review checklist and acceptance evidence.
- [Parity ledger](docs/PARITY.md): tracked gaps to CodexPro and evidence, never a declaration based on intention.
