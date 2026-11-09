# NovaGPT terminal interface

NovaGPT's interactive terminal interface (TUI) has four pages: **Overview**, **Activity**, **Settings** and **Chat**. Open it with `novagpt`, or run `novagpt --setup` for guided first-time configuration. Use `--root /absolute/path/to/project` to select a different workspace.

## Navigation

| Key | Action |
| --- | --- |
| `Tab` / `Shift+Tab` | Switch between pages |
| `↑` / `↓` | Select actions/settings or scroll events/messages |
| `Enter` | Run an action, edit a field, or confirm (in Chat: send) |
| `Esc` | Close a dialog or cancel an edit (in Chat: return to Overview) |
| `s` | Save settings (with confirmation; in Chat the key is typed) |
| `a` | Show or hide advanced actions on Overview |
| `?` | Show help (in Chat the key is typed) |
| `q` / `Ctrl+C` | Quit and restore the terminal (in Chat use `Ctrl+C`) |
| `PgUp` / `PgDn` | Scroll Activity events |
| `h` (Activity) | Toggle live activity and the last 2,000 saved audit events |
| `e` (Activity) | Confirm ZSTD support-log export |
| `Ctrl+N` (Chat) | Start a new ChatGPT conversation |
| `Ctrl+O` (any page) | List saved ChatGPT sessions and reopen one |

## Overview

- **Start MCP HTTP:** Start the local authenticated server on the saved port. When **Automatic ngrok** is enabled, this also starts the owned tunnel.
- **Start MCP + ngrok:** Explicitly start the server and tunnel, even if automatic ngrok is disabled in Settings.
- **Stop MCP HTTP:** Stop the server owned by this TUI instance, after confirmation.
- **Show / copy ChatGPT URL:** Reveal the complete private URL only after the server and tunnel are ready. Press **C** in the dialog to copy it. NovaGPT selects Wayland (`wl-copy`), X11 (`xclip`/`xsel`), WSL (`clip.exe`), macOS (`pbcopy`) or Windows (`clip.exe`) as appropriate. On Linux, install the appropriate clipboard utility if it is absent; the dialog displays an error instead of reporting a false success.
- **Diagnostics and optional tools:** Verify workspace status, inspect VM images when enabled, or launch the dedicated browser login flow.
- **Show subagent Chrome:** When MCP is running with ChatGPT Browser enabled, explicitly bring one existing subagent tab to the foreground without navigating or changing its work. If none exists, open a separate blank browsing tab in the same owned Chrome. This requires the desktop to allow window focus; it never opens a second browser profile. The separate manual-login action remains available only while MCP is stopped.
- **ChatGPT sessions (Ctrl+O):** List the conversations NovaGPT opened and reopen one. A TUI chat continues in the Chat page; a subagent conversation opens in the browsing tab. Reopening never resubmits an earlier prompt.

The advanced Overview actions also offer **Browse saved audit logs** and **Export support logs (ZSTD)**. Export asks for consent (No by default), creates a private file in the application-state exports directory, and never uploads or copies it automatically. Review the archive before sharing.

If ngrok is **missing** when starting a tunnel, NovaGPT asks whether to install it using an available supported system package manager. The confirmation defaults to **No**. The installer needs the user's explicit consent, and ngrok account authentication remains a separate manual step. See the [ngrok setup guide](INSTALLATION.md#configure-ngrok-for-chatgpt-access).

## Chat

The Chat page asks ChatGPT directly from the terminal. It requires **Enable ChatGPT browser** in Settings and a visible Chrome window on the dedicated NovaGPT profile, because the prompt is typed into the real, authenticated page — NovaGPT never handles your cookies or account tokens.

- Type a message and press **Enter**. The reply appears in the page when ChatGPT finishes; `↑`/`↓` scroll the local transcript.
- **Ctrl+N** asks for a new conversation. **Ctrl+O** opens the session list: `↑`/`↓` select, **Enter** reopens.
- The local transcript lasts for the current run only. ChatGPT keeps the real history, and a conversation URL is stored in the private session index so it can be reopened later.
- If the MCP server is already running, Chat reuses its dedicated browser and tab set. Otherwise NovaGPT opens the browser lazily; when MCP is later started it reuses that browser instead of launching a second Chrome on the same profile.
- A visible, focused tab is required for typing. If the desktop refuses focus, the send is rejected without submitting anything.

The session index (`chat-sessions.json`) is owner-private and contains only opaque ids, conversation URLs and timestamps — never prompts, replies, cookies or tokens. A damaged or oversized index is ignored instead of being used.

## Settings

Settings are shared across the current operating-system user, regardless of the selected workspace. The configuration is stored under the user's private NovaGPT state directory, not inside the project. On the first interactive launch, NovaGPT automatically creates the shared HTTP token in private user state if it does not already exist; subsequent launches reuse it. No token value appears in Activity or the terminal, and this does not start MCP or ngrok during setup.

- **Interface language:** Automatic (default), English or Italian. Automatic follows the system locale when Italian is detected, otherwise falls back to English. Missing language preferences also default to Automatic.
- **HTTP port:** Defaults to `8787`.
- **Start MCP automatically when NovaGPT opens:** Off by default. When enabled, normal TUI startup launches the server with the saved connection preferences. Guided `--setup` never auto-starts MCP.
- **Automatic ngrok:** When enabled, starting MCP normally also starts an owned ngrok tunnel.
- **ngrok hostname:** Optional hostname already assigned to your ngrok account; only the hostname is accepted, without a scheme or path.
- **Browser and virtual machines:** Disabled by default; enable only when needed.
- **ChatGPT subagent project URL:** Optional project page such as `https://chatgpt.com/g/g-p-abc123/project`. When set, every new subagent conversation is created inside that project, so deleting the project in ChatGPT deletes the whole group. Create the project in ChatGPT first; NovaGPT never creates or deletes it. If ChatGPT leaves the project before the first prompt, the send is refused instead of creating a conversation outside it.
- **Authorized directories:** Add or remove existing directories explicitly.
- **Import NovaGPT workspace settings:** Preview and import selected connection preferences from another NovaGPT workspace configuration, with confirmation. Secrets and workspace permissions are not imported.

Changes remain in a draft until saved with `s`. During guided setup, pressing **s** immediately opens the save confirmation, even before the review step; **Enter** accepts this non-destructive save (or select **No** to cancel). Other sensitive confirmations, including installing ngrok, still default to **No**. Enabling Automatic ngrok and/or Start MCP automatically never starts either service during setup and does not prevent saving. If saving fails, the reason appears in a dialog without changing the saved settings. Most changes that affect the running server require stopping and restarting MCP; changing the TUI language or auto-start setting does not alter the already-running server.

## Activity and diagnostics

Activity shows bounded lifecycle and operational events with timestamps. **Logs and diagnostics are always in English**, regardless of the selected interface language. TUI labels and prompts may be translated into Italian.

Use `novagpt logs tail --lines 500` to inspect retained events, `novagpt logs export --output /new/path/support.log.zst` to create a compressed support archive, or `novagpt doctor --json` for diagnostics. The old `audit.1.log` and other legacy raw segments are not read or exported. Tokens, full connector URLs, and private file contents should never appear in logs.

## Security

NovaGPT's MCP server allows authorized clients to work on local files and invoke host-shell commands. Host commands are not sandboxed; the connector URL grants powerful access. Reveal or copy the protected ChatGPT URL only when needed, and never share it with untrusted people.

To connect ChatGPT, follow [Install and connect NovaGPT](INSTALLATION.md). For architecture and known limitations, see [ARCHITECTURE.md](ARCHITECTURE.md).
