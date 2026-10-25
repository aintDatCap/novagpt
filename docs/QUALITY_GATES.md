# Quality gates and review protocol

These are **required acceptance criteria**, not claims that a full CI pipeline or CodexPro parity harness already exists. They apply to future NovaGPT changes together with [AGENTS.md](../AGENTS.md), [ENGINEERING_STANDARDS.md](ENGINEERING_STANDARDS.md) and [PARITY.md](PARITY.md).

## Levels of verification

| Change category | Minimum required now | Additional gate before marking parity VERIFIED |
| --- | --- | --- |
| Documentation only | Verify referenced paths/links, claims vs actual implementation, scope and Git diff | No invented runtime result; link to evidence already collected |
| Pure core code | `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo check --workspace`, `cargo test --workspace`; targeted negative/corner tests | Equivalent behavior and postconditions against CodexPro fixtures |
| MCP schema/tool/output | Core checks + end-to-end stdio protocol initialization, `tools/list`, `tools/call`, invalid arguments, error/result envelope | Exact TS/Rust names, schemas, required/optional fields, defaults, status and redaction |
| Transport/auth/session | Core checks + HTTP client integration tests for initialization, GET/SSE, POST, DELETE, auth errors and shutdown | Parallel transports, session capacity/TTL, live SSE, reconnection, retry/cancel, clock pause, protocol-version negotiation |
| Shared state, CAS, Git, shell | Core checks + deterministic two-writer conflict tests; integration tests with disposable Git trees | Multi-chat real shared-root, strict default-root semantics, expected_sha256 rejection, shared Git index / process hazards |
| Security-sensitive feature | Core checks + explicit traversal, symlink, blocked path, permissions, output caps and secret-redaction tests | Independent adversarial review and OS-specific cases (e.g. junctions/reparse points) |
| Optional VM/browser/subagent | Core checks + feature-off core tests + lifecycle and cleanup tests | Backend-specific integration in disposable guest/browser environment with explicit user approval |
| CLI/config/packaging | Core checks + help, defaults, precedence, invalid config and entrypoint tests | Install/upgrade/rollback and platform-specific release checks; no CodexPro launcher overwrite |

If a gate cannot be executed, report **BLOCKED or NOT RUN**, not PASS. No exception is silently accepted.

## Standard local command sequence

Run from the NovaGPT repository root:

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
```

For a release candidate also run `cargo build --release --workspace` on target platforms, including Windows, and explicitly execute relevant HTTP and CLI integration/stress scenarios. Changing `Cargo.toml` or a dependency requires lockfile review and platform/MSRV check. Do not run potentially destructive or external side-effect tests against a real user repository, browser profile, VM or cloud account.

Track test evidence as a short reproducible record: **command**, git revision, OS/Rust version, relevant feature flags, fixture location, exit status and the meaningful output excerpt. A named test file or a compilation result is not protocol parity.

For the INITIAL Git/Bash/patch/import/ngrok slice (2026-10-25), acceptance evidence is limited to isolated temporary Git repositories, `novagpt-core` safety/negative/concurrency tests, real stdio `tools/list` and `tools/call`, unit-only attachment URL/destination checks, and a CLI ngrok argument/help check. This does **not** satisfy the differential Git/command contract, external network attachment/SSRF penetration, child-process-tree cancellation, public tunnel/ChatGPT protocol smoke test, TUI ngrok state, or security review. Record concrete command outcomes only in [PARITY.md](PARITY.md).

### Testing pyramid and deterministic cases

1. **Unit tests next to domain modules:** guard decisions, canonical IDs, blocked paths, hashing, CAS, text encoding, limits and error categories.
2. **Application boundary integration tests:** real `novagpt` stdio JSON-RPC conversation and HTTP client requests, including `tools/list`, `tools/call`, `structuredContent`, `content`, `isError`. The existing `crates/novagpt-cli/tests/protocol.rs` exercises the stdio slice; it does not establish HTTP or TS parity.
3. **Cross-session/concurrency tests:** two or more independent transports share one root and observe changes. Use barriers/controlled clocks or channels, not arbitrary sleeps, for deterministic races.
4. **Adversarial and failure injection:** deny sensitive/traversal/symlink destinations, concurrent hash conflicts, timeouts, cancellation, capacity exhaustion, disconnect/restart and output-size limits. Ensure no partial unauthorized writes or leakage.
5. **Differential oracle:** run the same safe fixtures against CodexPro TS and NovaGPT Rust, compare both wire-visible output and filesystem/Git postconditions. Record approved differences, do not normalize away security or protocol errors.
6. **Platform/release:** Windows PowerShell/WSL/Hyper-V where applicable, supported Linux/macOS behavior, binary startup/shutdown, packaging, data preservation and tested rollback.

Prioritize negative tests for every new permission or capability. Every resolved production bug should get a focused regression test that fails before the fix and passes after it, where practical.

## Multi-chat acceptance scenarios (release blockers)

The CodexPro `docs/PRODUCT_USE_CASE.md` contract must remain true:

1. One server handles two independent MCP transports pointing to the same explicitly allowed real working tree with no client ID.
2. Concurrent reads and writes to **different** files proceed; changes are observed by both transports.
3. Two clients read the same file, client A modifies it, client B writes with the old `expected_sha256`: B fails without changing A's bytes.
4. Opening another allowed root must not alter the omitted-workspace default for any caller. Incorrect IDs fail closed.
5. Git index/branch and shell actions are visible/shared; the server never claims invisible per-chat transactions.
6. Optional VM/browser/agent systems can be disabled and the core coding workflow remains functional.
7. SSE or other streaming responses remain live while requested. An initialization storm does not evict active streams; TTL, capacity, clock suspension and reconnect are handled without mapping sessions to chats.

The HTTP multi-connection behavior verified by CodexPro's `scripts/http-multiconnection-smoke.mjs` is an oracle **candidate**, not a test that automatically runs against Rust. Port it or create a differential equivalent.

## Review gates for maintainability

For each nontrivial PR, reviewer answers **yes/no with evidence**:

- Does each modification belong to the crate/module that owns the invariant? Any cycle or reverse dependency?
- Could the change be smaller? Is an added trait/crate/abstraction driven by a real need rather than anticipation?
- Are external names/schema/defaults/errors backward compatible or explicitly versioned and documented?
- Are file/root and write permissions guarded at the correct trust boundaries, including Windows special cases?
- Are concurrency scope, locking order, timeouts, cancellation and cleanup clear and tested?
- Are result/output caps and redaction enforced on both success and failure?
- Are tests meaningful, independent, deterministic and capable of catching the relevant regression?
- Did the change update the proper canonical documentation and parity status **with evidence**?
- Are new dependencies justified and optional systems truly optional?
- Does the patch touch only its assigned files, without committing secrets, caches or user data?

Critical security, protocol, workspace-ID, permission or persistence changes require independent review from someone other than the implementer/agent when available. A self-review alone is not a substitute for an unperformed security test.

## Status and exceptions

Use the following vocabulary in the parity ledger:

- **NOT STARTED:** no implementation.
- **INITIAL:** implementation exists but is not proven equivalent to CodexPro.
- **VERIFIED:** comparable fixtures, commands, output and platform have been recorded; required security and concurrency cases pass for the declared scope.
- **BLOCKED:** reproducible inability to complete a gate; give cause, owner and next action.

If a legacy behavior is intentionally changed, record the deviation in `docs/PARITY.md` and an ADR, with client impact and migration/rollback path. A change does not become VERIFIED because it was approved on paper.

### Evidence record template

```text
Area / gate:
Git revision and baseline:
Platform / rustc / features:
Command(s) executed:
Fixtures / sample input:
Observed stdout/stderr/exit:
Behavior and postcondition:
Result: PASS | FAIL | BLOCKED | NOT RUN
Unresolved differences / owner:
```

**Release criterion:** all required parity gates G0-G8 in [PARITY.md](PARITY.md) are verified for supported platforms, with independently reviewed security, no destructive migration and an explicitly tested rollback. The independent NovaGPT launcher must remain separate from CodexPro until that point.
