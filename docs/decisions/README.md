# Architecture decision records (ADRs)

A short ADR preserves **why** a stable architectural decision exists when future agents need to change the system. Do not write an ADR for a normal implementation detail or a routine bug fix.

Use an ADR for changes to crate/dependency boundaries, MCP protocol or schema compatibility, canonical workspace IDs, security model, persistence/profile formats, supported platforms, optional subsystem ownership, or release/rollback guarantees.

## Procedure

1. Create `NNNN-short-name.md` (one concern per decision). Use the next number.
2. Record date, status (**Proposed**, **Accepted**, **Superseded**), context/problem, decision, alternatives considered, consequences, compatibility/security/test requirements and related code/docs.
3. For an accepted ADR, link it from [ARCHITECTURE.md](../ARCHITECTURE.md). In a changed design, add a new ADR rather than quietly editing the history; mark the earlier record Superseded and link it.
4. Review actual implementation separately: an Accepted decision is **not** a passing test or completed migration.

## Records

| ADR | Status | Decision |
| --- | --- | --- |
| [0001 — Independent Rust workspace and official MCP SDK](0001-independent-workspace-and-mcp-sdk.md) | Accepted | Separate NovaGPT repository, initial three crates, official `rmcp`, preserve shared multi-chat workspace |
| [0002 — Optional VM and ChatGPT Browser subagents](0002-optional-vm-and-browser-agents.md) | Accepted | Separate opt-in crates with owned VM images, bounded agents and dedicated Chrome profile |
| [0003 — Rust-native persistent browser driver](0003-native-browser-driver.md) | Accepted | Isolate Chrome lifecycle and CDP in Rust, keep ChatGPT-specific policy and MCP tools in existing adapters |
| [0004 — Browser-mediated TUI chat, session index and subagent project](0004-browser-mediated-tui-chat.md) | Accepted (project setup superseded by 0005) | Chat from the TUI through the owned browser, index conversation URLs privately, reject unofficial HTTP clients |
| [0005 — Automatic subagent ChatGPT project](0005-automatic-subagent-project.md) | Accepted | Reuse or create NovaGPT - subagents in visible Chrome before sending worker prompts; fail closed |
| [0006 — Windows development supervisor](0006-windows-dev-supervisor.md) | Accepted | Windows-only loopback restart control with capability token and owned process groups; release remains disabled |
