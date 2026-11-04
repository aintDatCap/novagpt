# NovaGPT terminal interface

NovaGPT's interactive terminal interface (TUI) has three pages: **Overview**, **Activity** and **Settings**. Open it with `novagpt`, or run `novagpt --setup` for guided first-time configuration. Use `--root /absolute/path/to/project` to select a different workspace.

## Navigation

| Key | Action |
| --- | --- |
| `Tab` / `Shift+Tab` | Switch between pages |
| `↑` / `↓` | Select actions/settings or scroll events |
| `Enter` | Run an action, edit a field, or confirm |
| `Esc` | Close a dialog or cancel an edit |
| `s` | Save settings (with confirmation) |
| `a` | Show or hide advanced actions on Overview |
| `?` | Show help |
| `q` / `Ctrl+C` | Quit and restore the terminal |
| `PgUp` / `PgDn` | Scroll Activity events |

## Overview

- **Start MCP HTTP:** Start the local authenticated server on the saved port. When **Automatic ngrok** is enabled, this also starts the owned tunnel.
- **Start MCP + ngrok:** Explicitly start the server and tunnel, even if automatic ngrok is disabled in Settings.
- **Stop MCP HTTP:** Stop the server owned by this TUI instance, after confirmation.
- **Show / copy ChatGPT URL:** Reveal the complete private URL only after the server and tunnel are ready. Press **C** in the dialog to copy it.
- **Diagnostics and optional tools:** Verify workspace status, inspect VM images when enabled, or launch the dedicated browser login flow.

If ngrok is **missing** when starting a tunnel, NovaGPT asks whether to install it using an available supported system package manager. The confirmation defaults to **No**. The installer needs the user's explicit consent, and ngrok account authentication remains a separate manual step. See the [ngrok setup guide](INSTALLATION.md#configure-ngrok-for-chatgpt-access).

## Settings

Settings are shared across the current operating-system user, regardless of the selected workspace. The configuration is stored under the user's private NovaGPT state directory, not inside the project.

- **Interface language:** Automatic (default), English or Italian. Automatic follows the system locale when Italian is detected, otherwise falls back to English. Missing language preferences also default to Automatic.
- **HTTP port:** Defaults to `8787`.
- **Start MCP automatically when NovaGPT opens:** Off by default. When enabled, normal TUI startup launches the server with the saved connection preferences. Guided `--setup` never auto-starts MCP.
- **Automatic ngrok:** When enabled, starting MCP normally also starts an owned ngrok tunnel.
- **ngrok hostname:** Optional hostname already assigned to your ngrok account; only the hostname is accepted, without a scheme or path.
- **Browser and virtual machines:** Disabled by default; enable only when needed.
- **Authorized directories:** Add or remove existing directories explicitly.
- **Import NovaGPT workspace settings:** Preview and import selected connection preferences from another NovaGPT workspace configuration, with confirmation. Secrets and workspace permissions are not imported.

Changes remain in a draft until saved with `s`. Most changes that affect the running server require stopping and restarting MCP; changing the TUI language or auto-start setting does not alter the already-running server.

## Activity and diagnostics

Activity shows bounded lifecycle and operational events with timestamps. **Logs and diagnostics are always in English**, regardless of the selected interface language. TUI labels and prompts may be translated into Italian.

Use `novagpt logs tail --lines 50` for persistent metadata-only audit events, or `novagpt doctor --json` for local diagnostics. Tokens, full connector URLs, and private file contents should never appear in logs.

## Security

NovaGPT's MCP server allows authorized clients to work on local files and invoke host-shell commands. Host commands are not sandboxed; the connector URL grants powerful access. Reveal or copy the protected ChatGPT URL only when needed, and never share it with untrusted people.

To connect ChatGPT, follow [Install and connect NovaGPT](INSTALLATION.md). For architecture and known limitations, see [ARCHITECTURE.md](ARCHITECTURE.md).
