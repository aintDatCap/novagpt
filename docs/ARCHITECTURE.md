# NovaGPT architecture

NovaGPT is an independent Rust MCP coding server. Product design, protocol support and release decisions belong to this repository; backward compatibility is not guaranteed for every change. See [ENGINEERING_STANDARDS.md](ENGINEERING_STANDARDS.md), [EXTENSION_PLAYBOOK.md](EXTENSION_PLAYBOOK.md), and [QUALITY_GATES.md](QUALITY_GATES.md).

## Crate boundaries

```text
novagpt-cli
  ├─ novagpt-mcp ── novagpt-core
  │    ├─ novagpt-vm
  │    └─ novagpt-agents ── novagpt-browser
  ├─ novagpt-vm, novagpt-agents, novagpt-browser
  └─ novagpt-core
```

- **novagpt-core:** owned workspaces, allowed-root and path guards, file reads and writes, optimistic SHA-256 updates, safe Git operations, commands and structured workspace operations. It must not depend on CLI, HTTP or MCP.
- **novagpt-mcp:** tools, schemas, protocol dispatch, stdio and HTTP transport, and request authentication through the official Rust MCP SDK (`rmcp`).
- **novagpt-cli:** argument parsing, shared private per-user configuration and token, terminal UI, process ownership, ngrok tunnel management, and optional integrations.
- **novagpt-vm:** optional approved VM images and platform-specific VM controls; real guest execution support is incomplete.
- **novagpt-agents** and **novagpt-browser:** optional bounded subagent management and a dedicated visible Chrome/CDP integration.

## Workspace and security model

One server can serve multiple simultaneous MCP clients on the same filesystem. MCP session IDs represent transport sessions, not ChatGPT conversations. Each operation uses the default configured root unless another explicitly approved workspace ID is supplied.

Canonical workspace roots and path guards reject disallowed paths and selected sensitive files. Optimistic SHA-256 changes are checked under per-file locks. Git index operations coordinate per canonical root. OS-level hostile path races still require further testing.

The MCP HTTP server binds to loopback and requires the shared user token. It accepts a bearer header or the private URL query parameter `novagpt_token`; the query secret is removed before MCP protocol handling. The token is private user state outside the repository. A previously issued query key is accepted temporarily for existing private connections, but is not emitted in new URLs.

The CLI/TUI can manage its own ngrok process and verify the matching upstream port and hostname before disclosing the HTTPS endpoint. The ngrok inspector and tunnel logs are disabled. The full connector URL is shown only on user request. Credentials and full URLs are not written to audit logs.

**Host-shell execution is not sandboxed.** A client with the full private connector URL can run commands with the server user's OS permissions. Explicitly allowed workspaces are not a sandbox around shell commands. Browser, VM and subagent features are opt-in.

## User experience and lifecycle

`novagpt` opens a three-page terminal UI: Overview, Activity and Settings. Guided `--setup` requires confirmation before it saves settings or creates a workspace-local launcher. The default UI language is Automatic, resolved from the system locale, with English fallback; all logs and diagnostics are in English.

The saved `auto_start_mcp` preference is disabled by default. If enabled, normal TUI launch requests MCP startup using saved HTTP and ngrok preferences; guided setup does not. Missing ngrok prompts for explicit installer consent, with No as the default answer. The installer uses only supported system package managers and does not handle ngrok account credentials.

Graceful HTTP shutdown handles Ctrl+C and SIGTERM on Unix and flushes audit events. CLI-owned ngrok is stopped and reaped on normal termination. Forced process termination, power failures and all in-flight requests are not guaranteed to be recoverable.

For local Rust iteration, `script/dev.py` watches source and manifests, builds without stopping the last working server, then restarts its owned server and tunnel on successful compilation. It attempts rollback if a new binary fails readiness. Each successful restart briefly interrupts active MCP sessions and may change a dynamically assigned ngrok hostname.

## Known limitations

- The full live public ChatGPT-to-ngrok integration has not been independently verified on every supported platform.
- TUI shutdown currently aborts some async server tasks; complete graceful resource cleanup needs an end-to-end audit.
- Shell commands run with host permissions; subprocess-tree cleanup and full sandboxing are not guaranteed.
- VM guest control, browser automation, and some optional integrations require live testing and are not equivalent to a fully managed cloud environment.
- Some filesystem writes are not crash-atomic on all platforms; path-swap defenses and adversarial concurrency require further review.
- Public MCP transport session TTL, reconnect and load boundaries need additional integration tests.

## Development checks

From the repository root:

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo check --workspace
cargo test --workspace
python3 -m unittest discover -s script -p 'test_dev.py'
```

All runtime behavior and security changes require relevant positive and negative integration tests, plus recorded verification evidence. See [CAPABILITIES.md](CAPABILITIES.md), [QUALITY_GATES.md](QUALITY_GATES.md) and the [architecture decisions](decisions/README.md).
