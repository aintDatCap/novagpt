# NovaGPT architecture and migration decisions

For normative extension rules see [ENGINEERING_STANDARDS.md](ENGINEERING_STANDARDS.md), [EXTENSION_PLAYBOOK.md](EXTENSION_PLAYBOOK.md), [QUALITY_GATES.md](QUALITY_GATES.md), [VM_AND_SUBAGENTS.md](VM_AND_SUBAGENTS.md) and the [decision log](decisions/README.md). The independent project and optional VM/ChatGPT Browser choices are recorded in [ADR 0001](decisions/0001-independent-workspace-and-mcp-sdk.md) and [ADR 0002](decisions/0002-optional-vm-and-browser-agents.md).

**NovaGPT lives at `C:\Projects\novagpt`, separate from CodexPro.** No overwrite, rename or update-in-place of the TypeScript server, Node launcher or user profiles. CodexPro remains the compatibility oracle.

## Technology decision

The MCP layer uses the [official modelcontextprotocol/rust-sdk](https://github.com/modelcontextprotocol/rust-sdk), published as `rmcp`. This repository initially selects `rmcp = "3.7.0"`. The SDK supplies tool registration and serialization, stdio and Streamable HTTP; NovaGPT implements workspace policy itself instead of relying on transport-session identity.

**Protocol warning:** `rmcp` 3.x implements the MCP 2026-07-28 stateless HTTP behavior for newly negotiated clients and legacy session support for earlier negotiated versions. CodexPro's current TypeScript transport uses stateful `Mcp-Session-Id` for its historical clients; prove compatibility, especially long-lived SSE and concurrent sessions, with differential tests before switching any client.

## Crate dependency graph

```text
novagpt-cli
  ├─ novagpt-mcp ── novagpt-core
  │    ├─ novagpt-vm
  │    └─ novagpt-agents ── novagpt-browser
  ├─ novagpt-vm, novagpt-agents, novagpt-browser (composition)
  └─ novagpt-core
```

- **novagpt-core:** workspace registry and root-derived IDs, guard/blocked paths, text reads, bounded tree/search, optimistic SHA-256 writes, initial guarded Git/command/patch/attachment import and raster-image read operations, per-file mutation locks and per-root async Git locks. Must never depend on MCP, HTTP or CLI.
- **novagpt-mcp:** official rmcp adapters, tool schemas, result envelopes and HTTP/stdin transports. Stateless per-call workspace targeting with a shared registry; no chat IDs.
- **novagpt-cli:** CLI parsing, per-workspace `.novagpt/config.json`, local Ratatui 3-tab console, guided setup/optional read-only CodexPro port import, stdio and token-protected loopback HTTP. CLI-only ngrok exposure is experimental and strictly opt-in; the ngrok subprocess is owned by the HTTP launcher and its request inspector is disabled. Bash modes are configured at startup. Full MCP tracing and CLI feature parity are not yet implemented. MCP stdio logs go to stderr, never protocol stdout.
- **novagpt-vm (opt-in):** independent NovaGPT VM home, approved image import, VM image listing and host create/status/destroy with Windows Hyper-V or QEMU; guest control is not implemented. See [VM_AND_SUBAGENTS.md](VM_AND_SUBAGENTS.md).
- **novagpt-agents + novagpt-browser (opt-in):** bounded asynchronous subagent state + a dedicated visible Chrome CDP driver. Browser login/verification requires the user; live UI behavior and project automation are not verified. A separate optional read-only `BrowserBridge` attaches to an explicitly selected loopback debugging port and approved HTTPS origin, never to the subagent Chrome by default. It is not yet the generic CodexPro Playwright browser.
- **Future modules:** semantic repository analysis beyond bounded inventory, generic browser automation, verified VM guest controls, executed handoff workflows, broader skills/Pro/history, widgets, Cloudflare/Tailscale tunnels and platform management. Initial metadata-only HUMAN/LLM audit now persists bounded per-workspace JSONL outside project roots, but full diagnostic tracing, independent multiple-process safety and TS parity remain pending. Complete the guarded Git/command/patch/import and ngrok slices before considering migration. Keep optional frameworks out of the core domain dependency graph.

## Safety and concurrency model

One server can serve many independent MCP transports. Each core operation receives optional `workspace_id`, with omission resolving the configured default root only. Canonical real roots are hashed to `ws_`-prefixed SHA-256 identifiers; alternate roots must be in the CLI allow-list.

A `WorkspaceRegistry` is shared across HTTP service instances via clones of an `Arc`. Locks are per-file, not a global serialize-all mutex. The optimistic `expected_sha256` is checked within the lock; two stale concurrent writers cannot both succeed. Root and path checks reject `..` traversal, disallowed canonical paths, sensitive directories/files, and symlink write targets. Read/search bound size and skip binary files. HTTP requires a >=24-character bearer token and binds only to loopback. Explicit CLI `http --ngrok` forwards that protected loopback listener through a child ngrok process; public access and actual ChatGPT MCP interoperability are NOT VERIFIED.

**Known limitations, not accepted parity:** `write`/`edit` currently modify existing files in place on Windows and are not crash-atomic; symlink-swap TOCTOU resistance and a more complete blocked-glob contract need implementation and adversarial tests. Git restore now requires explicit discard consent, commits reject protected staged paths and worktree create/remove only manages process-registered records in a controlled OS temp location; cleanup after crash/restart and Windows reparse-path race resistance remain unverified. Raw Git patches require one safe file; patch transactions support one ordinary update hunk target, not multi-file atomics. Safe Bash uses a fixed allow-list rather than CodexPro's richer policy; full Bash is host execution, not a sandbox, and child-process-tree termination is unproven. Import permits HTTPS approved hosts with pinned IPv4 and rejects redirects; real Apps SDK attachment import/MIME checks have not been verified. The ngrok CLI can launch a tunnel but public MCP POST/GET/DELETE and SSE/ChatGPT connection have not been verified. Output shapes, per-tool limits/defaults and errors differ from CodexPro. There is not yet a verified SSE capacity/TTL/reconnect contract. Do not use NovaGPT as a production CodexPro replacement.

## Implementation phases

1. **Bootstrap (in progress):** minimal Cargo workspace, official MCP SDK, guarded root and file operations, tests, README and ledger.
2. **Freeze TS fixtures:** exact tools/list mode configurations, tool schemas, output envelopes/error codes, HTTP wire sessions and CLI config. Capture stable fixtures against a disposable checkout, never production data.
3. **Full safe core:** structured results, CAS/atomic file writes, safe patches/import, Git, command safety, analysis and output store with unit/integration/property testing.
4. **Transport reliability:** legacy/modern HTTP, SSE/GET/DELETE, simultaneous transport sessions, capacity and TTL, cancellation, auth, reconnect, paused Windows clock, graceful shutdown, redacted event logs.
5. **CLI/optional parity:** profiles and setup/doctor, tunnels, release/Windows liveness, VM and Hyper-V, dedicated browser/subagents, handoff, widgets and opt-in history. Keep dependencies feature-gated.
6. **Verification and cutover:** differential TS/Rust tests, independent review, Windows and cross-platform release validation, rollback on a separate system. Only switch distribution after G0-G8 in PARITY.md pass.

## Commands and evidence

```powershell
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
cargo build --release --workspace
```

Use CodexPro's `docs/PRODUCT_USE_CASE.md`, `src/server.ts`, `src/http.ts`, `src/stdio.ts`, `src/config.ts` and `scripts/codexpro.mjs` as references. The new project must not silently remove protected TS behavior or migrate user state. Update PARITY.md with concrete output rather than marking a capability complete by inspection alone.
