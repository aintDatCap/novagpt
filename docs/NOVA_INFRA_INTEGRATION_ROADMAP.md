# Nova Infrastructure integration roadmap

**Status:** Partial local implementation (2026-11-18). Virtual-display wrapper and optional authenticated chat HTTP API have source and offline tests; live Chrome/ChatGPT, complete typed orchestration and VPS acceptance remain unverified.
**Repository boundary:** This document covers **NovaGPT only**. Provisioning, Forgejo, Windmill workflows, web UI, repository knowledge policies and gateway belong in the separate `nova-infra` repository (`docs/ROADMAP.md`).
**Goal:** Extend the existing independent Rust MCP server so a secure external orchestrator and Nova Chat browser frontend can reuse its *existing* ChatGPT Web, subagents and VM implementation without duplicating them.

Read [AGENTS.md](../AGENTS.md), [ARCHITECTURE.md](ARCHITECTURE.md), [CAPABILITIES.md](CAPABILITIES.md), [ENGINEERING_STANDARDS.md](ENGINEERING_STANDARDS.md), and [QUALITY_GATES.md](QUALITY_GATES.md) before implementation. This is coordinated with, but distinct from, [Agent Context Efficiency Roadmap](AGENT_CONTEXT_EFFICIENCY_ROADMAP.md) phase F. Avoid creating competing scheduling/queue systems.

## Existing baseline and invariants

- Existing Rust-native browser automation, ChatGPT Web subagents, TUI chat and session index are the **single source of implementation** for conversations and browser ownership.
- Existing multi-workspace MCP, Git/file/command tooling and optional VM module must continue to work without nova-infra, Forgejo, Windmill or a web frontend.
- Existing capability notes explicitly distinguish local browser/VM mock tests from unverified real ChatGPT account behavior. Do not convert a proposal into an implementation claim.
- One server may handle multiple authorized workspaces and clients; an MCP transport session is **not** a ChatGPT conversation identity.
- Browser profile/account credentials and ChatGPT cookies are never returned through the integration API or stored in the infrastructure repository.
- NovaGPT host-shell operations are not sandboxed; an external user-facing gateway must never expose an unrestricted raw MCP token or shell capability.

## N1 — VPS Linux virtual display and persistent browser lifecycle (P0)

**Problem:** Current browser input requires an owned, visible/focused Chrome tab; a VPS has no physical desktop.

- [x] Investigate running existing managed Chrome/ChromeDriver with a lightweight **virtual graphical display** (Xvfb + minimal WM, or a demonstrably equivalent setup), not replacing the working browser automation or assuming headless Chrome behaves identically.
- [x] Provide opt-in configuration for display/browser/profile lifecycle, owner lock, failure classification, safe startup and shutdown, dedicated Linux user and profile permissions.
- [ ] Preserve existing exact-tab targeting, focus gate, manual-login/manual-challenge behavior and *no automatic replay* of a possibly sent message after interruption.
- [ ] Support an owner-protected remote inspection path for initial manual login and troubleshooting; never publish DevTools, VNC or browser debugging endpoints directly.
- [ ] Handle process restarts, stale ChromeDriver handles, browser crash and reconnection with bounded, truthful state. Do not claim ChatGPT.com works unattended unless live manual testing verifies it.
- [x] Provide a small setup/runbook for the VPS service owner, including limits of headless environments and provider usage restrictions.

**Acceptance:** a disposable Linux virtual-display smoke test starts and manages the dedicated browser, handles a controlled page interaction and restarts without leaking state. Authentication to an actual ChatGPT account remains a *separately recorded manual check*; no bypass of verification challenges.

## N2 — Versioned external task-execution interface (P0)

**Problem:** Windmill needs stable task orchestration and status without turning NovaGPT into an orchestrator.

- [ ] Agree with `nova-infra` on a compact, versioned typed task envelope: opaque task ID/idempotency key, explicit workspace/repo identity, immutable base commit, workflow/version and rule refs, authorized operation scope, deadline/resource budget, review policy and artifact references.
- [ ] Reuse existing MCP/HTTP transport and typed core primitives. Implement the **narrowest authenticated dispatch/status/cancel contract** needed for one end-to-end pilot. Do not add an always-on queue or new crate unless justified by an actual ownership boundary.
- [ ] Represent transitions `queued|running|awaiting_review|succeeded|failed|blocked|cancelled|ambiguous`; report actual tool/test receipts rather than model assertions.
- [ ] Persist only minimal operation state needed to reconcile retries and restarts. Scope it per task/workspace and prevent collisions between transport sessions, browser conversations and worker identities.
- [ ] Require explicit worktree/repo authorization and immutable revision preconditions before modifications. Respect current path guards, file CAS and host-shell limits.
- [ ] Keep subagent credential/receipt rules intact. Reuse existing VM lease, cleanup and cancellation mechanisms; prefer a separate worker host for heavy builds/Windows testing.
- [ ] Treat an unknown outcome after possible submission/push/PR creation as `ambiguous`: reconcile externally before retrying. Do not repeat destructive actions automatically.
- [ ] Expose the observed final artifact and PR reference to the orchestrator, but let Forgejo/Windmill own actual PR lifecycle and scheduling.

**Acceptance:** a disposable local caller submits one authorized task, can poll status/cancel, receives trustworthy results, and cannot mutate a disallowed workspace or duplicate an already-submitted operation. Test the contract with nova-infra, not through a separate competing workflow engine.

## N3 — Reusable chat engine/API for Nova Chat (P0)

**Problem:** The external web interface must use the **same** underlying engine as NovaGPT's TUI Chat; it must not embed a second ChatGPT browser automation stack.

- [ ] Extract the existing TUI chat/browser/session capabilities behind a small, internal, typed service interface with stable identity and bounded resource ownership. Keep the TUI as a consumer.
- [x] Design an optional authenticated chat API for `create|list|resume|get_status|send|receive|cancel` (exact protocol chosen after inspecting existing modules), with client-safe stable errors and versioning.
- [ ] Model user authorization, chat conversation ID, browser tab ID, execution/task ID and MCP session ID as **distinct concepts**. No global mutable current chat; never trust client-supplied tab URLs to authorize a session.
- [x] Reuse managed Chrome's dedicated profile, ChatGPT Web send/observe logic, conversation index and optional subagent project grouping. Do not read or export cookies or provider credentials.
- [x] Return verified progress/completion states; optionally stream bounded updates by SSE/WebSocket if supported cleanly. Do not fabricate token streaming from a browser DOM or promise exact token usage.
- [x] Allow multiple sessions with safe admission limits, cancellation and recovery. A possible send after a crash must be `ambiguous`, not silently replayed.
- [x] Ensure secrets, full prompt contents and user conversation text are not included in generic status/audit logs. Any optional durable chat history must be opt-in, private, owner-scoped and retention-limited; ChatGPT remains authoritative for its own history.
- [x] Provide an integration example for nova-infra's gateway; auth, CSRF protection, user-facing login and UI live **outside** NovaGPT.

**Acceptance:** one existing TUI chat path and one authenticated API client use the same underlying browser/session implementation without regressions. A denied client cannot enumerate or act on another client's chat, and reconnect does not duplicate the latest message.

## N4 — Repository knowledge tools (P1)

**Problem:** agents should retrieve reliable project learning from versioned repository docs without treating transient LLM statements as truth.

- [x] Adopt the schema and folder conventions defined by `nova-infra/standards/knowledge/`; do not create competing schema authority here.
- [ ] Add small, **optional** workspace-scoped read/search tools or an adapter for the nova-infra Git-backed/SQLite FTS5 index. Keep core file/Git operations independent when the knowledge extension is disabled.
- [ ] Retrieve targeted docs by repo, module, status and evidence and include source path plus commit/revision. Load only relevant context, not the entire knowledge tree.
- [ ] Support **propose update**, not unrestricted silent auto-edit: knowledge extraction/consolidation is a nova-infra workflow, with PR review and evidence checking.
- [ ] Resolve conflicts conservatively: source and observed tests win over stale findings, project AGENTS/engineering docs over optional skills, global security gates cannot be disabled by repository profiles.
- [x] On adoption, update this repository's `AGENTS.md` once to reference `docs/knowledge/README.md` and migrate existing relevant lessons without copying architecture and ADR text into duplicate files.

**Acceptance:** an agent can search and read a validated knowledge note relevant to a test task, while disabled/invalid index access fails clearly and ordinary MCP tools remain unaffected.

## N5 — Web activity/worker observability (P1)

- [ ] Expose bounded, authenticated, content-minimized status/events for browser ownership, task execution, subagents and VM lifecycle needed by the nova-infra portal.
- [ ] Provide stable task IDs, timestamps and meaningful states. The frontend should be able to link task outcomes to Forgejo PRs and Windmill runs using correlation IDs, not parse free-form logs.
- [ ] Preserve current private audit/usage budgets. No leak of tokens, connector URLs, tool arguments, personal browser data, secrets or giant raw tool output.
- [ ] Keep observability separate from task control and avoid introducing unauthenticated debug endpoints.

**Acceptance:** a gateway can display a task and subagent lifecycle from event/status endpoints and cannot access privileged contents outside its scope.

## N6 — Remote worker/VM integration and deployment polish (P1/P2)

- [ ] Allow authorized remote workers to run task-scoped operations using already implemented VM primitives, with capability probing, bounded leases, cancellation and cleanup. No assumption of nested virtualization support on the chosen VPS.
- [x] Add a minimal service-operator runbook for environment variables, display selection, durable state location, privilege separation, health checks and manual browser login.
- [ ] Integrate with nova-infra staging/provisioning only after N1–N3 interfaces are stable. A generic local NovaGPT install must not gain a dependency on a specific hostname, reverse proxy, Forgejo or Windmill.
- [ ] Validate only the needed guest/OS path per milestone; defer Windows guest runtime tests until a supported host exists.

**Acceptance:** a disposable worker can run a task, return evidence, release resources and recover from an interrupted lease without harming another workspace.

## Interface contracts and sequencing

| Order | Interface / outcome | Owner | Dependency |
| --- | --- | --- | --- |
| 1 | N2 typed dispatch/status boundary | NovaGPT + nova-infra contract | existing MCP |
| 2 | N1 virtual display local proof | NovaGPT | existing managed Chrome |
| 3 | N3 chat API using existing engine | NovaGPT | existing TUI/browser |
| 4 | N4 knowledge retrieval | NovaGPT adapter | nova-infra M1 schema/index |
| 5 | N5 task and agent status events | NovaGPT adapter | N2, N3 |
| 6 | N6 remote workers and VPS polish | NovaGPT + nova-infra | supported host / N1–N5 |

**Parallelism:** N1 and N2 can progress concurrently; N3 is independent of N2 except for shared API/auth design; N4 waits for the knowledge schema, not for VPS deployment. Assign distinct crates/files to subagents. Shared Rust manifests, transport registration and browser ownership need one integrating owner.

## Lightweight verification strategy

- For **documentation-only** work: link/path sanity and diff inspection; no Cargo builds.
- For a small Rust slice: focused module/contract tests and formatter first. Run the repository-mandated Cargo checks at an appropriate integration checkpoint; **do not rerun the whole suite per file, per subagent or per intermediate commit**.
- For N1 browser support: one controlled local virtual-display browser smoke test; do not require a real ChatGPT account/Cloudflare login or multiple OS matrices merely to build the prototype.
- For N2: basic allowed/denied/duplicate task scenarios, plus restart state as appropriate.
- For N3: one chat send/result path, one unauthorized/session mix-up case and one ambiguous recovery case.
- For N4/N5: small fixtures and scoped access tests; skip speculative stress/performance benchmarks.

All changed public/tool/auth surfaces must still have minimal negative tests. Record actual PASS/FAIL/NOT RUN/BLOCKED, avoid broad audits unless findings justify them, and never claim any unrun checks passed.

## Not authorized by this document

This roadmap does not authorize unattended access to ChatGPT.com, defeating login or anti-bot challenges, production deployment, automatic merges, access to other users' projects, live Windows VM execution, secret export, or changes to the separate nova-infra repository. Those require their own explicit approved tasks and environmental prerequisites.
