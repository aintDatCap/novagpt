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

## Git, Bash, patch and attachment import (INITIAL)

NovaGPT exposes MCP tools `git`, `git_status`, `git_diff`, `show_changes`, `bash`, `apply_patch`, `import_file` and `view_image` (guarded PNG/JPEG/GIF/WebP reads, maximum 2 MiB). These are **initial guarded implementations**, not complete CodexPro parity. Git supports status/history, guarded single-file diff, branches, staging/unstaging, bounded local commit, explicit `restore` with `discard_changes=true`, and owned worktree create/list/remove using an OS temporary directory. Commit refuses already-staged protected paths; worktree removal refuses unowned or dirty worktrees. Records are process-local and worktrees may become orphaned after a server restart. Workspace-wide raw diff is refused unless an individual safe file path is supplied; `include_diff=false` returns summary stats. Git index mutations are serialized per canonical root, not per ChatGPT conversation.

Bash defaults to `safe`, a fixed list of Rust quality commands launched **without shell interpretation**. Use `--bash-mode off` to disable it. `--bash-mode full` requires the additional `--allow-full-shell` startup flag; it grants arbitrary host command execution and must only be enabled with deliberate trust. Subprocess output is bounded and timeout-controlled, but neither safe mode nor full mode constitutes a Windows sandbox.

`apply_patch` accepts one ordinary unified update diff targeting an existing guarded file, rejects stale hunks and multiple targets, and checks before applying. It does not promise crash-atomic updates or multi-file transactions. `import_file` accepts ChatGPT Apps SDK attachment objects, requires a guarded destination, approved HTTPS hosts, pinned public IPv4 DNS resolution, no redirects, a 5 MiB byte cap and an optional SHA-256 precondition. Real remote attachment transfer and MIME verification have **not** been demonstrated.

## Additional MCP and CLI slices (INITIAL)

`list_workspaces`, `workspace_snapshot`, `reconnect_workspace` and `inspect_workspace` provide explicit-root validation and bounded metadata-only inventory; they are not semantic dependency or symbol analysis. `codex_context` reads a guarded and bounded AGENTS.md instruction chain with fingerprint deduplication; it does not automatically include unrelated files, Git patches or handoff outputs. `handoff_to_agent` creates an exclusive bounded `.ai-bridge/handoffs/<ID>.md` plan **without running an agent**. `read_handoff` and `load_skill` expose only explicit guarded files. `runtime_status` includes capped metadata-only HUMAN and LLM views of selected tool outcomes, shared across HTTP sessions but not persisted. `server_config` exposes nonsecret configuration flags.

CLI commands `novagpt doctor --json`, `novagpt inspect --json`, and `novagpt review --json` are read-only diagnostics. Server diagnostics on stderr can use `--log-format human` (default) or `--log-format llm` (JSONL). Neither mode is an unrestricted transcript logger; sensitive tool inputs and file bodies are not recorded by the metadata-only audit. A real two-client MCP HTTP test validates separate sessions, shared file visibility, stale-hash rejection, GET/SSE and DELETE isolation. Full TS protocol parity and stress/TTL tests remain pending.

## Experimental ngrok exposure

The CLI can optionally run a managed ngrok agent for the **still loopback-bound** bearer-protected HTTP MCP server:

```powershell
novagpt --root C:\path\to\repo http --port 8787 --ngrok
novagpt --root C:\path\to\repo http --port 8787 --ngrok --ngrok-domain your-owned-domain.ngrok-free.app
```

This feature requires an independently installed and configured ngrok v3 client and explicit `--ngrok` opt-in. The agent's HTTP inspection is disabled, and the CLI prints a public `/mcp` URL only if its local API reports a tunnel matching the chosen loopback port. The token is not printed; never paste it into logs. **Real ChatGPT remote connection, SSE/GET/DELETE compatibility, and long-lived tunnel reliability remain unverified.** The ngrok integration is CLI-only; it is not yet configurable in the Ratatui TUI.

## Status

This remains an **early vertical slice**, not a complete port. Optional tool surfaces cover VM host lifecycle and asynchronous ChatGPT Browser subagents. Real guest control, generic browser projects, persistent and fully instrumented logging, advanced analysis, broader skills/Pro workflows, complete CLI/release behavior, and TS differential parity remain pending. See [VM and subagent guide](docs/VM_AND_SUBAGENTS.md), [parity](docs/PARITY.md) and [architecture](docs/ARCHITECTURE.md).

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
