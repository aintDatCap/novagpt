# Installing and configuring NovaGPT

NovaGPT is an experimental Rust MCP coding server. It operates on an explicitly selected local workspace and is separate from CodexPro. This guide covers installation, initial setup, starting the MCP server, and optional remote access.

## 1. Install NovaGPT

The official installation scripts download the latest published [NovaGPT GitHub release](https://github.com/aintDatCap/novagpt/releases), validate the release archive's SHA-256 checksum, and install the binary for the current user. They do not require a repository clone or Rust toolchain, and they do not modify CodexPro.

### Linux and macOS

Requirements: Bash, `curl`, `tar`, and either `sha256sum` or `shasum`; Linux/macOS on x86_64 or ARM64.

```bash
curl -fsSL https://raw.githubusercontent.com/aintDatCap/novagpt/main/script/install.sh -o novagpt-install.sh
bash novagpt-install.sh
```

The default destination is `~/.local/bin/novagpt`. The script adds the bin directory to the applicable shell profile if necessary.

To install a specific published version or choose another absolute destination:

```bash
bash novagpt-install.sh --version v0.1.0
bash novagpt-install.sh --install-dir "$HOME/.local/bin"
```

The version shown is an example; the corresponding GitHub release must exist.

### Windows

Requirements: Windows x64 and PowerShell. The current release matrix does not include a native Windows ARM64 package.

```powershell
Invoke-WebRequest -Uri "https://raw.githubusercontent.com/aintDatCap/novagpt/main/script/install.ps1" -OutFile ".\novagpt-install.ps1"
powershell -NoProfile -ExecutionPolicy Bypass -File ".\novagpt-install.ps1"
```

The default destination is `%LOCALAPPDATA%\Programs\NovaGPT\bin\novagpt.exe`. The script updates the **user** PATH, not the system PATH.

To select a specific published version or another destination:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File ".\novagpt-install.ps1" -Version v0.1.0
powershell -NoProfile -ExecutionPolicy Bypass -File ".\novagpt-install.ps1" -InstallDir "$env:LOCALAPPDATA\Programs\NovaGPT\bin"
```

**Open a new terminal after installation** so it inherits the updated PATH. Because the installers are fetched from GitHub and executed locally, review the downloaded script if you want to inspect what it will do. Both scripts use the release archives rather than compiling from source.

See [release assets and installer behavior](../script/README.md).

## 2. Select a workspace and launch setup

NovaGPT's default workspace is the current working directory. You can use any **existing** directory you are authorized to access; you do not need to run NovaGPT inside its own source repository.

On Linux/macOS:

```bash
cd /absolute/path/to/my-project
novagpt --setup
```

On Windows (PowerShell):

```powershell
novagpt --root "C:\Projects\my-project" --setup
```

The `--root` argument is optional when your terminal is already in the intended workspace. Running `novagpt` without a subcommand opens the same three-tab Ratatui console (**Overview**, **Activity**, **Settings**).

## 3. Configure the TUI

1. **Workspace:** Confirm that the directory shown in the console is the workspace you intended to expose. Add other explicitly permitted directories in **Settings** only if needed.
2. **Connection:** Keep the default HTTP port (`8787`) for a basic local setup, or change it. Leave automatic ngrok disabled unless remote ChatGPT access is required.
3. **Optional services:** ChatGPT Browser and virtual machines are opt-in. If enabling ChatGPT Browser, install Google Chrome; leave its executable path blank to use auto-discovery or provide a valid absolute path. NovaGPT uses its own browser profile, never a personal or CodexPro profile.
4. **Save:** In **Settings**, press `s` and confirm the save. The guided `--setup` flow also has an explicit final review/confirmation step. Unsaved changes do not become active server settings.

Settings are written to `<workspace>/.novagpt/config.json`; local launcher files may also be created under `<workspace>/.novagpt/` after confirmation, without overwriting an existing launcher.

The default TUI language is English. In **Settings → General → Interface language**, select English, Italian, or Automatic.

### HTTP authentication

NovaGPT automatically creates a strong, per-workspace HTTP token when needed. It is stored in private per-user state **outside the repository**, not in `config.json`.

You can display the token only when deliberately requested:

```bash
novagpt --root /absolute/path/to/my-project token show
```

Or display only the token file location:

```bash
novagpt --root /absolute/path/to/my-project token path
```

To enter a custom token securely, run `novagpt --root /absolute/path/to/my-project token set --replace` and follow the hidden input prompt. The `NOVAGPT_HTTP_TOKEN` environment variable takes precedence over the stored token. Changing the active token requires restarting the HTTP server and updating any connector URL using the old token.

**Never commit, paste into logs, or publicly share a token.**

## 4. Start the MCP server

### Local HTTP

In the TUI, use the **Start MCP HTTP** action. Alternatively run:

```bash
novagpt --root /absolute/path/to/my-project http --port 8787
```

On Windows, use a Windows path instead, such as:

```powershell
novagpt --root "C:\Projects\my-project" http --port 8787
```

The authenticated Streamable HTTP MCP endpoint is `http://127.0.0.1:8787/mcp`. In another terminal, verify that NovaGPT is responding:

```bash
novagpt --root /absolute/path/to/my-project connection-test --port 8787 --json
```

If ngrok was enabled in the saved settings but you want a local-only server, run `novagpt --root /absolute/path/to/my-project http --no-ngrok`.

### Local stdio client

For a client that launches NovaGPT as a local MCP subprocess, use:

```bash
novagpt --root /absolute/path/to/my-project stdio
```

The stdio transport does not use the HTTP token. Your MCP client must be configured to launch this executable and pass the selected workspace path.

## 5. Optional: connect ChatGPT through ngrok

Remote access is optional and more sensitive than local-only use. A remote authenticated MCP client can invoke the host shell with NovaGPT's OS-user permissions. Shell execution is **not sandboxed**; do not expose the connector to untrusted people or workspaces.

First, ensure ngrok is installed/configured for your account. Start a protected HTTP server with an explicitly owned ngrok tunnel:

```bash
novagpt --root /absolute/path/to/my-project http --port 8787 --ngrok
```

In a second terminal, request the connector URL:

```bash
novagpt --root /absolute/path/to/my-project connector-url --port 8787
```

You can instead enable automatic ngrok in the TUI, start MCP + ngrok, and choose **Show / copy ChatGPT URL** after the service is ready. For an assigned stable hostname, use `--ngrok-domain your-owned-domain.ngrok-free.app` together with `--ngrok`.

Paste the returned HTTPS URL into your ChatGPT MCP connector and select **No authentication**: the URL itself contains a secret token in its query string. Keep it private. Local connector readiness is checked, but end-to-end remote ChatGPT interoperability is **not yet fully verified**.

For additional details on the TUI and its security boundaries, see [TUI setup details](TUI_SETUP.md) and [VM/browser/subagents](VM_AND_SUBAGENTS.md).

## 6. Troubleshooting and alternatives

- **No published release / download failed:** The scripts need an accessible GitHub release containing an archive for your OS and architecture. See [GitHub releases](https://github.com/aintDatCap/novagpt/releases) or build from source below.
- **`novagpt` not found after installation:** Open a new terminal, or check that the install directory is on your PATH.
- **Workspace error:** Pass an existing directory with `--root`; paths outside approved roots are guarded.
- **Browser unavailable:** Chrome must be installed and ChatGPT Browser explicitly enabled. Use `novagpt --root /absolute/path/to/my-project browser-login` for a manual sign-in with the dedicated profile, then close the browser before starting browser agents.
- **HTTP connection error:** Confirm the configured port, start the HTTP server, check the token source, and run `connection-test`. HTTP is bound to loopback by default.
- **Diagnostics:** Run `novagpt --root /absolute/path/to/my-project doctor --json` and `novagpt --root /absolute/path/to/my-project logs tail --lines 50`.

### Build from source

For developers with Rust/Cargo installed:

```bash
git clone https://github.com/aintDatCap/novagpt.git
cd novagpt
cargo build --release -p novagpt-cli
```

The binary is written to `target/release/novagpt` (or `target/release/novagpt.exe` on Windows). Run it directly, or use `cargo run -p novagpt-cli -- --root /absolute/path/to/my-project --setup` to configure a different workspace.

See the [main README](../README.md) for current implementation status, limitations, and developer-facing technical details.
