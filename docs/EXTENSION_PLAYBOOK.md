# Extension playbook — adding functionality safely

This is a practical procedure for implementing **one vertical slice** of NovaGPT. Read [AGENTS.md](../AGENTS.md), [ENGINEERING_STANDARDS.md](ENGINEERING_STANDARDS.md), and [QUALITY_GATES.md](QUALITY_GATES.md) first. Current source code is an early Rust port; examples here describe **target practice** and must not be mistaken for already-implemented features.

## Decide where work belongs

| Change | Start here | External surface |
| --- | --- | --- |
| Workspace IDs, allowed roots, path checks | `novagpt-core/src/workspace.rs` (later `workspace/`) | Expose through an existing core method |
| File read/write/CAS, bounded search | `novagpt-core/src/fs_ops.rs` (later `filesystem/`, `search/`) | Thin `novagpt-mcp` tool handler |
| MCP tool name/JSON schema/response | `novagpt-mcp/src/lib.rs` (later `tools/`, `response/`) | Official `rmcp` `#[tool_router]` API |
| HTTP auth/SSE/session state or stdio | `novagpt-mcp` transport boundary | Client-visible wire protocol |
| Flags, config, startup and profiles | `novagpt-cli/src/main.rs` (later `config/` and `commands/`) | `novagpt --help` / env / config |
| QEMU/Hyper-V, browser, subagents, widgets, history | Isolated optional module or adapter | Enable only through explicit configuration and compatible descriptors |

Create a focused private module first; do **not** create a new crate, registry, shared mutable singleton or trait just because another tool is being added.

## Recipe A — new core feature

1. **Write the behavior contract:** exact inputs, validated root and path, output structure, capacity/size/time caps, errors and whether the action changes files or other global state.
2. **Determine the single owner:** if behavior concerns safe filesystem operations, it belongs in core. Transport wrappers do not implement safety checks.
3. **Define the smallest typed API** around actual consumers. Prefer a request struct for 4+ logically related arguments; use distinct error variants rather than human-facing strings in domain code.
4. **Validate at the trust boundary** and again at dangerous mutation boundaries; all resolutions must remain under approved canonical roots. Write through a per-target lock + hash precondition when necessary. Never return secret data or expose arbitrary paths.
5. **Add tests before exposure:** success, invalid input, denied path, blocked files, large/binary input, retry/conflict and multi-thread interactions when stateful.
6. **Expose at MCP only after core tests pass:** handler should contain argument conversion, service call and result mapping, not business logic.
7. **Check diff and gates:** run targeted tests plus Cargo lint/test baseline. Update parity evidence for any CodexPro equivalent.

Keep error messages suitable for the caller without exposing internal paths or secrets. An unrecognized optional argument should follow a conscious validation/compatibility rule.

## Recipe B — add or migrate one MCP tool

Use this checklist for every tool, even an apparently trivial read-only one:

1. **Inventory the TS oracle:** locate its `registerCodexTool` definition in CodexPro `src/server.ts`, the called implementation and one or more smoke tests; capture exact name, input fields (types, defaults, required), description, output envelope and negative cases. If no TS equivalent exists, label it a NovaGPT extension and give it a documented compatibility policy.
2. **Choose a domain operation** in `novagpt-core`; define request/response types and typed errors. Keep `rmcp` and HTTP out of its public API.
3. **Add one thin `rmcp` registration** using the existing handler/router pattern; use `serde`/`schemars` and the official SDK. The tool's public name, visible description, JSON input schema and `structuredContent` are versioned contracts.
4. **Map both outcomes:** success has bounded `content` and typed `structuredContent`; failure has `isError` and a safe, stable error structure. Distinguish tool errors from protocol-level malformed JSON-RPC. Never echo secrets or verbose host diagnostics.
5. **Add a real protocol integration test** analogous to `crates/novagpt-cli/tests/protocol.rs`: initialize, list, invoke success, invoke rejection, assert JSON fields and filesystem postconditions, and avoid relying on invocation of Rust functions alone.
6. **Test mode exposure** (once mode support exists) and ensure an intentionally disabled tool cannot be called through an alternate dispatch path.
7. **Update** `docs/PARITY.md`: from NOT STARTED to INITIAL when real Rust behavior exists; to VERIFIED only with comparable TS/Rust fixtures and command output.

**Example (pseudocode, not a copy-paste compile target):**

```text
MCP input (serde/schemars)
    ↓ decode / validate
rmcp tool handler
    ↓ typed request
novagpt-core domain operation
    ↓ guarded path + policy + file lock/CAS
shared workspace
    ↓ typed result / typed error
rmcp response mapping (bounded, redacted)
    ↓ content + structuredContent + isError
ChatGPT client
```

If adding the tool requires edits to the CLI, core guard, HTTP router, session manager and widget renderer simultaneously, reconsider ownership boundaries. A regular tool should not need all of them.

## Recipe C — optional subsystem (VM, browser, agent, handoff)

Treat optional capabilities as independent bounded **resources**, not global background daemons coupled to the core.

- Define an explicit resource ID distinct from `workspace_id` and MCP session ID. The ID must not identify a ChatGPT conversation.
- Specify lifecycle as a state machine such as `Created → Starting → Running → Stopping → Stopped/Failed`, including legal retries and idempotent shutdown.
- Own cancellation, process/guest handles, timeouts, concurrency limits, cleanup and retained diagnostics in one subsystem manager; avoid detached Tokio tasks.
- Put platform-specific code in named adapters (Windows Hyper-V vs QEMU, browser backend variants). Keep the public typed interface stable as implementations vary.
- Decide how the feature is disabled. A **core-only build and workspace tools must work without it**; no implicit browser login, VM installation or agent configuration.
- Test create/use/cancel/failure/cleanup on disposable resources. Gate potentially destructive or paid external actions behind explicit human authorization. Never copy host credentials into guest or browser automation.
- If a temporary Node/platform bridge is unavoidable, record owner, security boundary, deployment footprint, direct tests and conditions for removal. Do not build a second cross-cutting orchestration core.

## Recipe D — add a configuration setting

1. Give the option one typed field and ownership; define default, validation bounds, source precedence, CLI/environment spelling, and whether it can change at runtime.
2. Propagate a validated immutable value into the owning service. Domain code should not read `std::env` in arbitrary methods.
3. Define how the setting interacts with disabled modes, profiles and secrets; prefer stricter defaults if new risk is introduced.
4. Test absent, valid, malformed, limit, precedence and compatibility with existing profiles.
5. Update `--help`, README and parity fixtures for a user-visible option. Never write secrets into a generated config, log or test output.

## Recipe E — refactor a growing area

Start with a **behavior-preserving move**, not an API redesign and feature addition in the same patch. Define before/after dependency graph and tests; extract small private modules while keeping public names stable. Move focused tests with their domain logic; keep integration tests at the application boundary.

Introduce an interface only if at least two real callers/backends or a deterministic test seam justify it. Migrate one consumer at a time and remove deprecated paths promptly once parity is proven. If changing an external contract, create an ADR and explicit version/migration plan first.

## Completion checklist for a vertical slice

- [ ] Ownership, public API and failure cases written down.
- [ ] Bounded input/output, auth/path checks and redaction reviewed.
- [ ] Cancellation/timeout/cleanup defined for any long-lived resource.
- [ ] Tests include real protocol integration and negative cases when externally exposed.
- [ ] Cargo fmt, Clippy with warnings as errors, check and relevant tests actually run.
- [ ] Compatible TS fixtures compared or outstanding differences explicitly marked in PARITY.
- [ ] Documentation and CLI/tool discoverability updated if public-facing.
- [ ] Diff reviewed for unintended changes, credentials, dependencies and stale docs.
