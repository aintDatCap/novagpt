# ADR 0006 — Embeddable agent runtime
Status: accepted for the first implementation slice.

The coding product remains NovaGPT. Other Rust applications can use the
provider-independent task runtime from `novagpt-agents` without a workspace,
VM, browser, MCP server, or coding privileges.

- Default features: none. `AgentRuntime` accepts an injected authorized
  `RuntimeBackend`; bounded concurrency, cancellation and deadlines are
  independent of the coding manager.
- `browser`, `workspaces`, and `vm` expose optional existing components.
- `coding` enables the existing workspace-aware agent manager and its
  dependencies; `full` enables it and the optional MCP client. CLI and MCP opt in explicitly.
- Feature selection is a build-time dependency decision, not authorization.
  External tool capabilities must be checked at runtime.

The existing coding manager still uses the existing browser/workspace/VM
contract. The optional `mcp-client` feature offers bounded, allowlisted calls over an
explicit caller-owned asynchronous transport. Provider implementations, durable
job persistence and isolated multi-client browser sessions are future slices;
the new runtime does not claim to implement them. Use authorized inference
providers for automated consumers.

An external Cargo project can import `novagpt-agents` from the repository
using a pinned published Git revision and `default-features = false`.
Private repositories require normal Git authentication. Do not commit keys
or a local absolute path. Run minimal, browser-only and full feature checks
separately because Cargo feature unification can mask accidental coupling.
