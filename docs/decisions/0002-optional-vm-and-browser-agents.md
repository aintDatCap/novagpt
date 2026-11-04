# ADR 0002 — Optional VM and dedicated browser integrations

**Date:** 2026-10-24  
**Status:** Accepted; live platform validation remains incomplete.

## Context

Virtual machines, browser automation and subagents require additional privileges, platform dependencies and lifecycle ownership. They must not become prerequisites for ordinary local file and Git tools.

## Decision

Keep `novagpt-vm`, `novagpt-agents` and `novagpt-browser` separately owned and opt-in.

VM image import is explicitly user initiated and records ownership in a dedicated NovaGPT store. VM operations must accept validated owned IDs, not arbitrary host paths. Guest control is unavailable until scoped credentials and guest-agent behavior are implemented and tested.

Browser automation uses a dedicated visible Chrome profile under the user's control, accessed through local CDP. It does not attempt to bypass sign-in or human verification challenges or import cookies/passwords. Subagent state is bounded and server scoped; returned data is untrusted, and no patch is auto-applied without the required workflow.

## Rejected alternatives

- Reuse another application's VM store or a personal Chrome profile: rejected to preserve ownership and credentials.
- Start or destroy arbitrary VM host paths through MCP: rejected in favor of approved imports and owned IDs.
- Require browser, VM or API keys on every startup: rejected; keep them optional.
- Automatically apply subagent changes to a shared repository: rejected until isolation, review and conflict behavior are verified.
- Automatically bypass login or verification challenges: rejected.

## Consequences

Optional capabilities require focused integration tests and clear failure/cleanup behavior. CDP automation can break when website UI changes, and Hyper-V/QEMU tests require disposable environments. Do not advertise functionality as universally stable based on mocked tests.

See [VM_AND_SUBAGENTS.md](../VM_AND_SUBAGENTS.md), [CAPABILITIES.md](../CAPABILITIES.md) and [QUALITY_GATES.md](../QUALITY_GATES.md).
