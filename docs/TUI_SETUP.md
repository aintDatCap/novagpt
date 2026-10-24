# Local TUI and guided setup

NovaGPT has an interactive terminal interface built with Ratatui. It is **per workspace**: without `--root` the current working directory is used; `--root` points to an existing directory instead. NovaGPT does not alter the original CodexPro installation.

## Launch on Windows

Build first:

```powershell
cd C:\Projects\novagpt
cargo build --release -p novagpt-cli
```

Inside any workspace:

```powershell
C:\Projects\novagpt\target\release\novagpt.exe
C:\Projects\novagpt\target\release\novagpt.exe --setup
C:\Projects\novagpt\target\release\novagpt.exe --root C:\Projects\my-workspace --setup
```

Plain `novagpt` opens the 3-tab TUI; `--setup` opens that **same TUI** on the Config/Setup tab. `Tab`/`Shift+Tab` move among **Comandi**, **Stato/Log**, **Config/Setup**. Press `q` to quit. A real interactive terminal is required; no configuration is written automatically in headless sessions.

### Commands

On Comandi, `1` starts the local authenticated HTTP MCP server, `2` requests its shutdown, `3` lists approved VM images (if enabled), and `4` reports local diagnostics. Stato/Log displays bounded, recent lifecycle events and errors. These are currently launcher/server lifecycle events, **not a complete capture of every MCP tool call or tracing span**.

HTTP requires a sufficiently long `NOVAGPT_HTTP_TOKEN` environment variable and only listens on `127.0.0.1`. No web control panel, public tunnel, or remote ChatGPT connector is installed. Normal non-interactive MCP clients must explicitly run `novagpt stdio`; for HTTP outside the TUI use `novagpt http --port 8787`. Omitting `--root` works with both transports.

### Setup and saved settings

On Config/Setup:

- `e` edits the local HTTP port; `v` enables/disables the isolated VM store.
- `b` enables/disables the dedicated ChatGPT browser; `c` enters the **absolute path to an existing Chrome executable**. This does not reuse any existing Chrome profile.
- `a` adds an explicit additional allowed workspace root; `x` clears that list.
- `p` cycles through read-only CodexPro workspace profiles; `i` imports the selected profile's compatible values **into memory**, with the chosen source and proposed port shown before import.
- `s` saves the current local settings. In `--setup` mode it also installs a local launch script and a copy of the NovaGPT executable (if missing). `Esc` cancels field editing; `Enter` accepts a field.

Everything belonging to this setup goes under `<workspace>/.novagpt/`:

```text
.novagpt/
  config.json
  bin/novagpt.exe        # Windows (copied only when absent)
  start.cmd              # Windows launcher; launch from any directory
  vms/                   # created when VM is enabled/used
  browser/               # dedicated browser data when enabled/used
```

On Unix the launcher is `start.sh` and the executable is `bin/novagpt`. The configuration directory is excluded from Git by the NovaGPT repository's own `.gitignore` and **blocked by NovaGPT's MCP workspace file tools**. Other project repositories may need to add `.novagpt/` to their own `.gitignore`. Existing installed executables are not overwritten; to upgrade an installed launcher, replace its binary intentionally after stopping NovaGPT.

The saved file contains **only non-secret settings**: port, additional roots, and opt-in VM/browser toggles/Chrome executable path. No authentication credentials are persisted. `NOVAGPT_HTTP_TOKEN` is supplied at runtime. The CLI reads the local file on startup; explicit `--vm-home`, `--chatgpt-chrome`, `--chatgpt-profile`, and `http --port` override corresponding saved values. Explicit `--allow-root` flags augment configured allowed roots. Restart a running HTTP server after changing these settings.

### Optional CodexPro import

By default NovaGPT searches the current user's CodexPro profile directory `~/.codexpro/profiles` (or the explicitly configured `CODEXPRO_HOME`). You may instead specify `--codexpro-home <directory>`. Profiles are read **only**; CodexPro is never modified.

**Current import compatibility is intentionally narrow:** only the saved HTTP **port** can be imported. All other original CodexPro settings (including tunnel configurations, authorized roots, auth material, browser sessions, VM stores and unsupported runtime modes) are ignored, not copied or silently treated as working NovaGPT features. Choose a profile with `p`, inspect its name and port in the TUI, press `i` to accept that value, then `s` to persist it. Import is optional; you can configure NovaGPT from scratch.

This UI is a first implementation, not full CodexPro CLI parity. Terminal visuals have not yet undergone manual end-to-end acceptance on every supported terminal/Windows version. See [parity](PARITY.md) for remaining limitations.
