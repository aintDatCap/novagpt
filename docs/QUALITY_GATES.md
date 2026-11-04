# NovaGPT quality gates

These are minimum acceptance requirements, not a claim that every platform and integration is fully verified. Read alongside [AGENTS.md](../AGENTS.md), [ENGINEERING_STANDARDS.md](ENGINEERING_STANDARDS.md) and [CAPABILITIES.md](CAPABILITIES.md).

## Required checks

| Area | Minimum evidence |
| --- | --- |
| Documentation | Check local links, accurate behavior descriptions, English text and a focused Git diff |
| Core / workspace policy | Formatting, Clippy, Cargo check/tests, denial and concurrency tests |
| MCP tools | Real stdio and/or HTTP initialize, tools/list and tool calls, including invalid input and redacted errors |
| Authentication and network transport | Tests for local POST/GET/SSE/DELETE, missing and invalid credentials, accepted hosts, concurrency and graceful shutdown |
| Git, file mutations and host shell | Disposable workspaces, optimistic conflict checks, blocked/sensitive path tests and cleanup under failure |
| CLI, TUI and installer | Default values, explicit confirmation, saved settings, absent/invalid configuration, platform startup and shutdown checks |
| ngrok and private ChatGPT URL | Account for tunnel ownership and endpoint verification, real or mocked ngrok behavior, credential omission from logs and safe cancellation |
| Browser/VM/agents | Off-by-default behavior; explicit lifecycle, separate user-owned data and disposable integration tests |
| Release | Supported platform binaries, install/update/rollback smoke tests, independent security review for privileged changes |

For Rust changes, run from repository root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
```

For changes to the POSIX source-development supervisor:

```bash
python3 -m unittest discover -s script -p 'test_dev.py'
```

A compile or lint pass alone is not a runtime pass. Record commands, OS, Rust version, fixture/input, exit status, actual observed behavior and known limitations. When blocked or skipped, record **BLOCKED** or **NOT RUN**, never PASS.

## Required multi-client scenarios

1. Two independent MCP transports access the same approved workspace simultaneously.
2. Different files are read/changed without a global lock, and both clients see shared changes.
3. Two clients read the same file, then a stale `expected_sha256` write fails without destroying the newer version.
4. Explicit alternate workspace IDs cannot change another client's default root; unauthorized IDs fail.
5. Git index and shell actions affect shared OS state, not hidden per-conversation copies.
6. Optional browser, agents and VM can be disabled without breaking ordinary coding tools.
7. HTTP reconnect, streaming, capacity, clock interruptions and cancellation behave safely under test.

## Security review

Every change affecting filesystem access, process execution, network exposure or credentials must review canonical-root policy, symlink/path-swap threats, credential leakage, timeouts, bounded outputs, cancellation, and cross-platform behavior. Review real outcomes rather than trusting comments. Host shell access is **not sandboxed**, even when a file tool is restricted to an approved workspace.

Never test destructive Git/VM/browser/network operations on real user data when a disposable fixture is possible. Make public-contract changes explicit and document migration steps; backwards compatibility is a product decision, not an unconditional guarantee.

## Release readiness

A release requires successful platform-specific packaging and startup/shutdown tests, clear known limitations, credential-safe audit behavior, and evidence of a tested rollback path. Advanced browser and VM features must not be presented as verified on platforms where only mocks or unit fixtures were executed.

Update [CAPABILITIES.md](CAPABILITIES.md) with the actual status of each relevant area, and maintain [ARCHITECTURE.md](ARCHITECTURE.md) when the lifecycle or trust boundary changes.
