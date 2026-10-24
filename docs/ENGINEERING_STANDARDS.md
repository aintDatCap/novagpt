# Engineering standards — growing NovaGPT without growing complexity

**Type:** normative engineering guidance for future changes. This document describes intended boundaries; it is **not a claim** that all existing code already conforms. Follow [AGENTS.md](../AGENTS.md) and consult [ARCHITECTURE.md](ARCHITECTURE.md) for what is implemented today.

## Design principles and complexity budget

1. **Correctness before speed or reuse.** Model invariants in types and tests; optimize only against measured evidence. Prefer simple sequential code to unproven concurrency improvements.
2. **A module owns a capability, not an arbitrary file count.** A module should answer one question with a small public API; most implementation details stay private.
3. **Deep modules, thin entrypoints.** Put validation and behavior in domain services, not in `rmcp` tool handlers or CLI branches. Handlers should decode, call, encode.
4. **No speculative infrastructure.** Avoid home-grown plugin registries, universal request contexts, generic service-locator frameworks, macros or trait hierarchies until concrete integrations demonstrate a need.
5. **Prefer coherent change locality.** Adding a file operation should primarily touch its domain module, one MCP adapter, contract tests and documentation—not a dozen unrelated crates.
6. **Make invisible costs visible.** Every background task, resource limit, mutex, cache, subprocess, HTTP stream and file write must have an explicit owner and cleanup/failure policy.

### Keep or split a module?

Keep an implementation together while it has one cohesive invariant, a small set of stable callers and understandable tests. Split it when two responsibilities change independently (e.g. read path guard vs HTTP response serialization), when cross-platform behavior needs distinct backend adapters, or when the module's public API exposes unrelated concepts. **Line count alone does not require a new crate or trait.** First extract private submodules. A new crate requires a concrete dependency boundary, its own tests and an [ADR](decisions/README.md).

Use a domain-oriented layout as functionality grows:

```text
crates/
  novagpt-core/src/
    workspace/        # root registry, IDs, path guard
    filesystem/       # read/write/edit, CAS and atomicity
    search/           # bounded query and analysis
    git/              # optional Git command adapter
    command/          # optional shell execution and policy
  novagpt-mcp/src/
    tools/            # schemas and thin adapters for core services
    transport/        # HTTP/SSE and stdio lifecycle
    response/         # public redaction, result/error normalization
  novagpt-cli/src/
    config/           # precedence and profiles
    commands/         # command dispatch and application bootstrap
```

These paths are a **suggested evolution**, not existing directories. Keep actual modules small enough to review and names aligned with the domain.

## Dependency, ownership and API contracts

| Layer | Owns | May use | Must not own |
| --- | --- | --- | --- |
| `novagpt-core` | Root/guard policy, domain errors, file/Git/command semantics, lifecycle of domain resources | standard Rust + focused audited crates | Protocol transport, `rmcp`, HTTP status codes, CLI flags, ChatGPT tabs |
| `novagpt-mcp` | Versioned MCP tool schema, `rmcp` routing, request/response serialization, sessions/auth/HTTP | core via typed APIs; official `rmcp` | Path authorization policy, direct file writes, CLI profile parsing |
| `novagpt-cli` | Argument/config parsing, CLI presentation, runtime composition and distribution adapters | core and mcp | Domain behaviors or bespoke MCP protocol |
| Optional subsystems | Bounded VM/browser/agent/handoff adapters with independent enablement | narrow core services and selected transports | Obligatory dependencies for core workspace/file/Git operations |

Expose only types needed by other modules. Default to `pub(crate)`/private; promote to `pub` only for an identified consumer. Reject generic `Map<String, Value>` as a domain API when a typed struct can express the contract. Make invalid states difficult to construct: `WorkspaceId`, `CanonicalRoot`, `GuardedPath`, `ExpectedSha256`, `OutputLimit`, `Deadline` are candidate newtypes **when multiple call sites benefit**, not mandatory wrappers everywhere. Keep serialized field names stable (e.g. `workspace_id`, `expected_sha256`).

When adding behavior, put *policy* in core and *protocol formatting* in MCP. Avoid a core dependency on `serde_json::Value` unless persisting structured domain data truly demands it. Inject time, filesystem, subprocess and external backends only at seams needed by tests or multiple implementations. Do not mix `reqwest`, `axum` or `rmcp` types into core interfaces.

## Fallible operations, output and compatibility

Use typed `Result<T, E>` with actionable variants for forbidden root/path, missing resource, malformed input, conflict/CAS, permission, timeout/cancellation, output cap, IO and backend error. Preserve internal causes for redacted tracing but map them to a **stable, non-sensitive** MCP error envelope at the boundary. Keep JSON-RPC protocol errors distinct from normal MCP tool failures (`isError`). Never serialize Rust debug errors, stack traces, secrets or raw environment variables to a client.

For changes to MCP-visible behavior, specify and test:

- tool name, description, required/optional fields, defaults and schema form;
- JSON result envelope (`content`, `structuredContent`, `isError`, metadata), byte caps and truncation markers;
- the exact denied/invalid/conflict behavior and error classification;
- behavior in `minimal`, `standard`, `full` and disabled modes (target parity; some modes do not exist in NovaGPT yet);
- corresponding CodexPro baseline fixture in [PARITY.md](PARITY.md).

Treat these as versioned external APIs even if only one current client is known. A deliberate breaking change needs an ADR, migration instructions, differential tests and explicit approval. Never silently drop a CodexPro feature to simplify the port.

## Concurrent resources and lifecycle

**Scope rules:** per-call arguments are explicit; immutable configuration and allowed roots belong to server startup; transport sessions belong to `rmcp`; shared workspace state belongs to a clearly owned `Arc` service. **No per-chat global mutable 'current workspace'.**

- Prefer parallel independent reads. Serialize only conflicting operations on the same canonical target when necessary.
- `expected_sha256` is a compare-and-swap precondition for one target, **not a distributed transaction** or Git index lock.
- Do not hold `std::sync::MutexGuard` across `.await`, hold one lock while awaiting another, or spawn untracked tasks. Decide and document lock order when operations need more than one lock.
- Asynchronous handlers must not perform long synchronous IO on executor threads. Use async IO where suitable or a **bounded** blocking executor; queue and admission limits must also protect against overload.
- For every long-lived HTTP stream, process, VM, browser and delegated worker, define owner, handle, state transitions, cancellation, timeout, shutdown path and idempotent cleanup.
- Protocol session reuse/reconnect must never be interpreted as ChatGPT tab identity. Active SSE streams cannot be evicted merely because the request handler returned.
- Use explicit capacity/TTL policies backed by tests with a fake clock or deterministic synchronization rather than timing-dependent sleeps.

## Security and configuration boundaries

Reject operations outside approved **canonical** allowed roots. Apply sensitive-path rules and symlink checks on both requested and resolved paths, including parents; plan for Windows junction/reparse-point and case-insensitive behavior. Use secure creation/rename primitives and handle-based checks where available. Record remaining symlink-swap/TOCTOU issues in the parity ledger instead of overstating protections.

Separate **read**, **write**, **shell execution**, **network exposure** and **optional automated browser/VM** permissions. A safe default may be more restrictive than an eventual compatibility mode; differences must be explicit. Load secrets from environment or protected external configuration without copying values into logs, tests or fixtures. Limit input, output, search, duration, sessions and worker counts. Prefer sanitizing at output boundaries *and* never collecting unnecessary sensitive data.

Config must have one documented precedence and startup validation path. Runtime components should receive typed immutable config or narrowly scoped settings. No hidden environmental reads scattered inside domain services. Changes to persisted profiles/formats require migration and rollback tests on disposable fixtures.

## Dependencies and feature isolation

- Add a dependency **only** when standard library or current workspace crates cannot provide the required feature safely and simply.
- Explain why it is needed, affected crates, build/platform impact, security footprint, MSRV and how it will be tested. Prefer an existing audited dependency already used in the workspace.
- Use the official `rmcp` SDK for MCP; do not hand-roll a parallel wire protocol or fork it just for convenience.
- Keep optional platform/VM/browser/subagent dependencies behind feature or adapter boundaries. Verify a core-only build with optional components absent.
- Use `cargo tree` during significant dependency changes; avoid broad `features = ["full"]` unless functionality requires it.
- Do not introduce `unsafe` without a documented contract, platform argument, narrow boundary and independent safety review.

## Documentation ownership and decision records

- [ARCHITECTURE.md](ARCHITECTURE.md) describes actual crate relationships and known caveats.
- [PARITY.md](PARITY.md) tracks real port status and commands/evidence.
- [EXTENSION_PLAYBOOK.md](EXTENSION_PLAYBOOK.md) provides change steps.
- [QUALITY_GATES.md](QUALITY_GATES.md) sets verifiable acceptance criteria.
- [decisions/](decisions/README.md) captures durable decisions and rejected alternatives; a decision log is **not** a substitute for code/tests.

When changing an invariant, exported API, ownership boundary, storage format, supported platform, or MCP transport semantics: update the closest source documentation **in the same change**, and add an ADR if alternatives and migration implications deserve permanent record. Avoid repeating the entire architecture in each PR; link to the canonical document.

## Things to avoid

God services; circular dependencies; sprawling global enums containing every tool's arguments; transport-specific policies in core; per-chat implicit mutable state; unbounded output buffers; magic retry loops; threads/processes without owners; opaque `unwrap` in production; blanket `#[allow]`; feature flags without tests; synthetic passing status; and refactors touching every crate when a local change suffices.
