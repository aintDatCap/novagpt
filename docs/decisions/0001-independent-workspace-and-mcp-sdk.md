# ADR 0001 — Independent Rust workspace and official MCP SDK

**Date:** 2026-10-24  
**Status:** Accepted; updated for independent product direction in November 2026.

## Context

NovaGPT serves multiple ChatGPT conversations against the same explicitly allowed local workspace. Security, concurrency, maintainability, clear protocol boundaries and independent product decisions are the project's priorities.

## Decision

Maintain NovaGPT as an **independent Rust MCP server** with no blanket requirement to reproduce another application's CLI, storage formats, user interface or tool schema.

The Cargo workspace separates responsibilities:

- `novagpt-core`: canonical workspace IDs, path safety, file/Git/command policy and domain operations, independent of network transport.
- `novagpt-mcp`: official [Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk), protocol handlers, authentication and tool schemas.
- `novagpt-cli`: configuration, CLI, terminal UI, tunnel lifecycle and process composition.

Optional browser, VM, agent and handoff subsystems must not be prerequisites for core file/Git tasks. An MCP transport session, IP or browser tab is never a ChatGPT conversation identity. Missing `workspace_id` resolves to the configured default root; other roots require explicit authorization.

## Alternatives

A hand-built MCP wire protocol was rejected in favor of a maintained SDK. A monolithic crate would mix trust boundaries. Mandatory per-chat worktrees would undermine the shared-root product model.

## Consequences

NovaGPT may evolve its own tool contracts, CLI and storage formats. Changes affecting real users require explicit release notes, tests, migration guidance and rollback consideration, rather than an assumed compatibility promise.

Different MCP protocol versions require testing of SSE, initialization, sessions, reconnect, authentication and errors. File safety, CAS and Windows path race resistance need adversarial tests. A passing compile cannot substitute for these tests.

See [ARCHITECTURE.md](../ARCHITECTURE.md), [QUALITY_GATES.md](../QUALITY_GATES.md), [CAPABILITIES.md](../CAPABILITIES.md) and [AGENTS.md](../../AGENTS.md).
