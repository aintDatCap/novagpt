# Install and connect NovaGPT

NovaGPT runs locally and exposes MCP tools to explicitly authorized clients. The setup below is for people who want to connect their projects to ChatGPT.

## Install NovaGPT

Download an archive from [NovaGPT Releases](https://github.com/aintDatCap/novagpt/releases) or use the installation scripts.

**Linux** (requires Bash, curl, tar and SHA-256 utilities):

```bash
curl -fsSL https://raw.githubusercontent.com/aintDatCap/novagpt/main/script/install.sh | bash
```

macOS is excluded from GitHub Actions CI and automated releases. On a Mac, build from source with `cargo build --release -p novagpt-cli`; the current release workflow does not publish the macOS archives required by the Bash installer.

**Windows x64** (PowerShell):

```powershell
irm https://raw.githubusercontent.com/aintDatCap/novagpt/main/script/install.ps1 | iex
```

These commands pipe the remote installer directly into Bash or PowerShell without saving the installer script locally. They execute remote code immediately: review the [Bash script](../script/install.sh) or [PowerShell script](../script/install.ps1) beforehand, or download it separately if you want to inspect the exact bytes before execution. The installers download release binaries and verify their SHA-256 checksums; this does **not** authenticate the piped script itself. An accessible published release is required. Reopen the terminal after installation if `novagpt` is not found. See [installer details](../script/README.md) for platforms and installation paths.

Alternatively, build the binary yourself with `cargo build --release -p novagpt-cli`; its path is `target/release/novagpt` (or `novagpt.exe` on Windows).

## First launch

In a terminal, open the folder you want ChatGPT to work with:

```bash
cd /absolute/path/to/your-project
novagpt --setup
```

Or run `novagpt --root /absolute/path/to/your-project --setup`. On Windows, use the corresponding absolute Windows path.

Follow the guided setup, verify the selected workspace, and save. You can explicitly allow additional directories later in Settings. NovaGPT stores preferences per operating-system user, separately from the project folder; the default server port is `8787`.

After setup, launch the **Overview / Activity / Settings** interface with `novagpt`. Navigation: arrows and Enter to select, Tab to switch pages, `s` to save preferences, `a` for advanced actions and `?` for help.

The default interface language is **Automatic**: Italian for Italian system locales, English otherwise. You can select English or Italian manually.

### Optional automatic MCP startup

In **Settings → Connection**, enable **Start MCP automatically when NovaGPT opens**, then save. On your next normal `novagpt` launch, the TUI starts the local MCP server using the saved port and tunnel preferences. This is off by default; `novagpt --setup` does not start it.

Do not start two servers on the same port. NovaGPT will not expose some other process through its ngrok tunnel.

## Configure ngrok for ChatGPT access

NovaGPT manages an ngrok tunnel for the local MCP server. You do not need to run `ngrok http` separately.

### 1. Install ngrok

Download ngrok from [ngrok's official download page](https://ngrok.com/download), or install it with your platform's package manager:

- macOS with Homebrew: `brew install ngrok`
- Windows with WinGet: `winget install ngrok -s msstore`
- Linux with Snap: `sudo snap install ngrok` (if Snap is available)

If ngrok is absent when you select **Start MCP + ngrok**, NovaGPT displays an **installation confirmation**. After you approve, it can invoke Homebrew on macOS, WinGet on Windows, or Snap through `pkexec` on supported Linux machines. No installation command runs without approval. The package manager may require permissions or additional prompts. Where no supported manager exists, install manually from the official website.

Confirm installation:

```bash
ngrok version
```

### 2. Authenticate ngrok

Create or sign in to your [ngrok account](https://dashboard.ngrok.com/), find your **authtoken** in the dashboard and configure it in your own terminal:

```bash
ngrok config add-authtoken YOUR_NGROK_AUTHTOKEN
```

Substitute your actual ngrok authtoken. **Do not paste the token into ChatGPT, tickets or repository files.** NovaGPT does not install or configure your ngrok account token on your behalf.

If you have an ngrok hostname assigned to your account, enter **only its hostname** (such as `my-project.ngrok-free.app`, without `https://`) in **Settings → ngrok hostname**. This is recommended if you want a stable ChatGPT connection URL after restarts.

### 3. Start NovaGPT and its tunnel

In **Settings**, turn **Automatic ngrok** on and save. Then select **Start MCP HTTP** or **Start MCP + ngrok** on Overview. Wait for the authenticated local-server readiness check and tunnel readiness.

Alternatively, from a terminal:

```bash
novagpt --root /absolute/path/to/your-project http --port 8787 --ngrok
```

For a hostname assigned to your account:

```bash
novagpt --root /absolute/path/to/your-project http --port 8787 --ngrok --ngrok-domain my-project.ngrok-free.app
```

The CLI prints the public endpoint without exposing the secret. If you started HTTP from the CLI, run this command in a **second terminal** to reveal the full connector URL:

```bash
novagpt --root /absolute/path/to/your-project connector-url --port 8787
```

If you started it from the TUI, select **Show / copy ChatGPT URL** instead and press **C** to copy. The private URL is never copied automatically.

On Linux the copy command uses a clipboard helper appropriate to the desktop session: `wl-copy` on Wayland or `xclip`/`xsel` on X11 (with fallback when one fails). A fresh installation might not include these commands. Install **wl-clipboard** or **xclip**, depending on your desktop:

| Distribution | Wayland | X11 |
| --- | --- | --- |
| Ubuntu / Debian / Linux Mint | `sudo apt install wl-clipboard` | `sudo apt install xclip` |
| Fedora | `sudo dnf install wl-clipboard` | `sudo dnf install xclip` |
| Arch / Manjaro | `sudo pacman -S wl-clipboard` | `sudo pacman -S xclip` |
| openSUSE | `sudo zypper install wl-clipboard` | `sudo zypper install xclip` |

WSL uses `clip.exe` to access the Windows clipboard. If no graphical clipboard is accessible (for example, an SSH-only terminal), the TUI displays an error and keeps the URL visible for manual copying; it does not send your secret through unsupported terminal clipboard escape sequences. Confirm the selected clipboard helper works in your user session before copying sensitive URLs.

### 4. Create a ChatGPT plugin

The complete URL has this shape:

```text
https://my-project.ngrok-free.app/mcp?novagpt_token=YOUR_PRIVATE_TOKEN
```

It is **not** the plain ngrok hostname: include `/mcp` and the complete `novagpt_token` query parameter produced by NovaGPT. The example token above is not a real credential.

1. Open ChatGPT **on the web** and go to **Plugins**.
2. Click **+**, then **Add custom MCP server**.
3. Name the connection **NovaGPT** (optionally add a description).
4. Under **Connection**, paste the complete HTTPS URL copied from NovaGPT.
5. Choose **No authentication**. The URL already embeds a private NovaGPT credential.
6. Review and acknowledge the security warning, then choose **Create as a plugin**.
7. Review the discovered tools, and select the resulting NovaGPT plugin in the ChatGPT conversation where you want to use it.

These actions depend on your ChatGPT plan and workspace permissions; the exact controls may change. See the [official ChatGPT MCP connection guide](https://help.openai.com/en/articles/12584461-connecting-custom-mcp-servers-in-chatgpt) for requirements.

If you later change the NovaGPT HTTP token, regenerate the URL and update the plugin connection. If your ngrok hostname changes, update the URL as well. **After changing MCP tool schemas** (for example after adding the `probe` parameter to subagent callbacks), rebuild/restart NovaGPT and refresh/reconnect the custom ChatGPT plugin so its discovered tool list is current. The connector's display name is selected in ChatGPT; it may be `NovaGPT Laptop`, `NovaGPT PC`, or any custom label. If the actual selected label is known, pass it as the optional `connector_name` hint; otherwise omit it rather than guessing. Every MCP implementer still requires live authenticated no-write capability probes before its task is sent. A separately opened ChatGPT Web worker may not inherit the parent's plugin access; NovaGPT checks actual authenticated callback receipts before sending implementer tasks and reports missing tools instead of assuming inherited access.

### Security notes

**Anyone who has your full connector URL can authenticate to the server.** The MCP `bash` tool runs commands on your computer under your normal OS account, without a shell sandbox. Do not share the URL, paste it in logs, screenshots, or commits, or grant access to untrusted clients. ngrok acts as a tunnel provider for the HTTP traffic.

NovaGPT listens only on local loopback and connects outward through an owned ngrok process. It does not automatically make your whole machine secure or isolate shell commands. Configure only the workspaces and integrations you intend to use.

## Other ways to start MCP

To use **local HTTP without ngrok**:

```bash
novagpt --root /absolute/path/to/your-project http --no-ngrok
```

The local authenticated endpoint is `http://127.0.0.1:8787/mcp`. Check readiness locally with `novagpt --root /absolute/path/to/your-project connection-test --port 8787`.

To use an **MCP client that starts a local subprocess**:

```bash
novagpt --root /absolute/path/to/your-project stdio
```

## Managed ChromeDriver (optional)

The optional ChromeDriver compatibility backend uses a separate, versioned
driver store:

```bash
novagpt chrome-driver status
novagpt chrome-driver install              # match the installed Chrome milestone
novagpt chrome-driver install --milestone 156
```

`status` never downloads. `install` downloads only ChromeDriver from Chrome
for Testing (or the legacy ≤114 layout), verifies/patches it, and reuses it
later without network access. Set `NOVAGPT_CHROMEDRIVER_HOME` to an absolute
directory to choose a different store. These commands do not launch a browser
or open a profile. The ChatGPT adapter uses this managed ChromeDriver transport
on first use, installing the matching driver if it is missing. See
[undetected-chromedriver-parity.md](undetected-chromedriver-parity.md).

## Troubleshooting

- **ngrok command not found:** restart your terminal to refresh PATH, or install ngrok from its official download page. Supported TUI installation may prompt when you start a tunnel.
- **ngrok fails to start:** verify `ngrok version`, account setup and authtoken, and check any reserved hostname belongs to your ngrok account.
- **Port already in use:** stop the other local process or choose a different port in Settings.
- **ChatGPT cannot connect:** check MCP and tunnel readiness, confirm the full `/mcp?novagpt_token=...` URL, and use **No authentication**. Check plan/workspace permissions and refresh the plugin if its tools changed.
- **Server errors:** run `novagpt doctor --json` and `novagpt logs tail --lines 50` locally. These diagnostics are in English and intentionally exclude credentials.
- **Browser features unavailable:** enable ChatGPT Browser explicitly and install Chrome; browser login may require a separate manual step.

For development and architecture, see [ARCHITECTURE.md](ARCHITECTURE.md) and [ENGINEERING_STANDARDS.md](ENGINEERING_STANDARDS.md).
