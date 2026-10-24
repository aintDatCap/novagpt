# ADR 0001 — Independent Rust workspace and official MCP SDK

**Date:** 2026-10-24  
**Status:** Accepted (architecture direction; complete parity is not implemented)

## Context

The existing CodexPro TypeScript server is the compatibility oracle. The rewrite must prioritize correctness, security, maintainability, concurrency and predictable boundaries without risking an in-place replacement. A typical user runs one MCP server accessed by several ordinary ChatGPT conversations against the **same** explicitly allowed repository.

## Decision

Build a **new independent project named NovaGPT** rather than modifying or renaming the installed CodexPro codebase. Start with one small Cargo workspace and three crates:

- `novagpt-core`: canonical workspace IDs, root/path safety, domain services, file/Git/command policy; independent of wire transports.
- `novagpt-mcp`: MCP transport, tool schemas and output formatting using the **official [modelcontextprotocol/rust-sdk](https://github.com/modelcontextprotocol/rust-sdk)** (crates.io name `rmcp`).
- `novagpt-cli`: binary startup/configuration, CLI and distribution adapters composing core and MCP.

Dependencies flow from CLI through MCP to core. Optional VM/browser/subagent/handoff backends must not be required by the core workflow. The server cannot infer a conversation ID from `Mcp-Session-Id`, IP address or browser tab. Omitted `workspace_id` always resolves to the configured default root. Other roots must be explicitly allowed and targeted.

The initial SDK selection is `rmcp 3.7.0` as declared in the root Cargo manifest; later upgrades need compatibility fixtures and a dependency review.

## Considered alternatives

- **Rewrite CodexPro in place:** rejected because it could disrupt a working TypeScript server and existing profiles before Rust parity is tested.
- **Custom MCP wire protocol:** rejected in favor of the maintained official SDK; exceptions must be isolated and justified.
- **One crate for everything:** rejected because domain safety and transport concerns evolve independently.
- **Many subsystem crates from day one:** rejected because extra APIs and build boundaries would create speculative complexity. Extract only if a real integration/dependency boundary emerges.
- **Per-chat state/worktrees required by default:** rejected because it violates the actual multi-chat shared-root product contract.

## Consequences and guardrails

Benefits: safe side-by-side migration, independently testable core, small protocol boundary, idiomatic Rust modularity and clear extension points. Costs: temporary dual implementations, version-specific `rmcp` behavior, deliberate fixtures and rollback procedures.

The SDK may negotiate different MCP protocol versions, including stateful versus stateless HTTP. A successful compile does **not** establish CodexPro HTTP/session/response parity; compare live SSE, concurrent initialization, session limits, reconnect, auth, schemas and output envelopes. File CAS, path guards and Windows symlink/TOCTOU safety require adversarial testing.

**No cutover** to NovaGPT until the recorded gates in [PARITY.md](../PARITY.md) and [QUALITY_GATES.md](../QUALITY_GATES.md) pass. CodexPro remains untouched and usable until then.

## Related documents

[ARCHITECTURE.md](../ARCHITECTURE.md), [ENGINEERING_STANDARDS.md](../ENGINEERING_STANDARDS.md), [EXTENSION_PLAYBOOK.md](../EXTENSION_PLAYBOOK.md), [PARITY.md](../PARITY.md), root [AGENTS.md](../../AGENTS.md).
