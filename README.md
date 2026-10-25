# NovaGPT

**Separate, in-progress Rust rewrite of CodexPro.** It does not replace or modify the CodexPro TypeScript installation.

Architecture: `novagpt-core` (workspace/security/file operations), `novagpt-mcp` (official [Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk), transports), and `novagpt-cli` (binary/configuration).

## Build and Ratatui TUI

`novagpt` without a subcommand opens an interactive, three-tab terminal UI for the current directory (or `--root`). Navigate its stateful command/config menus with arrows and `Enter`; `?` shows help. `novagpt --setup` opens the same TUI with a guided, four-step configuration flow. Choose Save (`s` is optional) and **explicitly confirm** before writing local settings and installing a non-overwriting launcher under `<workspace>/.novagpt/`. CodexPro import is **read-only and port-only**, with preview and separate confirmation; no credentials or browser/VM data are copied. HTTP tokens are automatically generated per workspace, kept in per-user private state outside the repository, and revealed only on explicit `novagpt token show`. See [TUI setup instructions](docs/TUI_SETUP.md).

```powershell
cargo build --workspace
cargo test --workspace
cargo run -p novagpt-cli -- --root C:\path\to\repo stdio
```

For Streamable HTTP on localhost, NovaGPT automatically generates and reuses a strong per-workspace token when `NOVAGPT_HTTP_TOKEN` is unset (the environment variable still takes precedence):

```powershell
cargo run -p novagpt-cli -- --root C:\path\to\repo http --port 8787
```

The HTTP endpoint is `http://127.0.0.1:8787/mcp`. HTTP always requires a token; stdio does not. Retrieve the active token locally with `novagpt token show` (secret output) or view its path with `novagpt token path`. Do not paste the token into logs, screenshots or Git.

## Status

This is an **early vertical slice**, not a complete port. The normal tool surface includes workspace open, tree, search, read, write and edit. Initial optional tool surfaces cover VM host lifecycle and asynchronous ChatGPT Browser subagents. Real guest control, browser projects, richer CodexPro tool behavior, CLI/release parity and all differential checks remain pending. See [VM and subagent guide](docs/VM_AND_SUBAGENTS.md), [parity](docs/PARITY.md) and [architecture](docs/ARCHITECTURE.md).

CodexPro remains the reference oracle and production implementation until parity and rollback have been validated.

## Optional VMs and ChatGPT Browser subagents

Both are **off by default**, can be enabled either by explicit runtime flags or by a saved per-workspace TUI configuration, and are not part of ordinary file operations. For managed Hyper-V/QEMU images use `--vm-home <NEW_DIRECTORY>` and the human-only `vm-import` CLI command. For browser subagents use `--chatgpt-chrome <CHROME_EXE> --chatgpt-profile <FRESH_PROFILE_DIRECTORY>`; login and verification must be manual. See [setup, capabilities, security and gaps](docs/VM_AND_SUBAGENTS.md).

## Contributing and scaling

Start with [AGENTS.md](AGENTS.md) for agent-friendly repository rules. The engineering reference is split by purpose:

- [Engineering standards](docs/ENGINEERING_STANDARDS.md) — crate ownership, types, module/API design, dependency and concurrency boundaries.
- [Extension playbook](docs/EXTENSION_PLAYBOOK.md) — steps for adding safe core operations, MCP tools, settings and optional subsystems.
- [Quality gates](docs/QUALITY_GATES.md) — tests, security cases, compatibility evidence and review checklist.
- [Architecture decisions](docs/decisions/README.md) — why the important boundaries exist.
- [Parity ledger](docs/PARITY.md) — verified status compared with CodexPro, not assumed success.

When several agents contribute simultaneously, assign nonoverlapping files/modules and coordinate manifest, public schema and Git index changes.
