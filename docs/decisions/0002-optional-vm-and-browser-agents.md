# ADR 0002 — Optional managed VMs and dedicated ChatGPT Browser subagents

**Date:** 2026-10-24  
**Status:** Accepted for an INITIAL implementation (behavioral parity pending)

## Context

CodexPro's native VM backend chooses Windows Hyper-V or QEMU on other supported systems. Its subagents use an asynchronous backend, with the ChatGPT Browser as one option. NovaGPT should not make the complexity, privileges or secrets of either feature a prerequisite of ordinary workspace/file/Git tools.

## Decision

Use separate `novagpt-vm`, `novagpt-agents` and `novagpt-browser` crates. They share no hidden per-chat identity. The MCP adapter uses the official `rmcp` API and exposes their operations only when a server operator supplies explicit startup configuration; disabled calls fail without side effects.

VM image import is **human CLI only**, to an ownership-marked, isolated NovaGPT VM home with a SHA-256 manifest. MCP can only select approved names. Hyper-V/QEMU provision disposable overlays/instances, never import arbitrary host paths through MCP. A destroy operation only operates on a validated owned instance ID. Guest control remains unavailable until a secure credential-scoped and guest-agent design passes tests.

Subagent state is server-scoped and persistent across tool calls, with strict concurrency/record bounds. Browser automation uses a separate visible Chrome profile under the user's control, communicates through local CDP, and never reads cookies or attempts to solve human challenges. Manual checks must be completed by the user; resume requires their confirmation. Agent output is untrusted and `implementer` patches are not auto-applied. No file contents are transmitted by default.

## Alternatives

- Reuse the CodexPro VM home or Chrome profile: **rejected**; production data and credentials must remain isolated.
- Start or destroy VMs from arbitrary user-provided MCP host paths: **rejected**; require human-approved local import and owned IDs.
- Require VM, browser or API keys at normal NovaGPT startup: **rejected**; keep both opt-in.
- Auto-apply subagent changes in the shared real workspace: **rejected** pending isolated worktree implementation and independent verification.
- Automatically clear Cloudflare/login challenges: **rejected**; all such verification is manual.
- Use a Node/Patchright bridge for a nominal Rust rewrite: **not chosen** in this slice; direct CDP is implemented with independent tests required for its web compatibility.

## Consequences

Advantages: explicit privilege boundaries, no CodexPro overwrite, clearly owned resource lifecycles, no silent host source-path copying, and testable actor state. Risks: Chrome DOM automation is brittle; Hyper-V/QEMU and guest operations require real platform integration; command invocation and lifecycle are not fully transactional. Do not mark full parity or release readiness based on initial compilation.

See [VM_AND_SUBAGENTS.md](../VM_AND_SUBAGENTS.md), [PARITY.md](../PARITY.md), and [QUALITY_GATES.md](../QUALITY_GATES.md).
