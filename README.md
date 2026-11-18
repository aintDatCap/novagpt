# NovaGPT

**Work with your local projects directly from ChatGPT.**

NovaGPT is an independent, open-source MCP server written in Rust. It connects ChatGPT to the files, Git repositories and development tools on your computer, while keeping control of which workspaces are available to the server.

You run NovaGPT on your own machine. ChatGPT connects to it through a protected MCP endpoint, and you can use ordinary conversations to explore source code, review changes and work on your projects.

## What can NovaGPT do?

- **Explore and edit projects:** read files, search, inspect directories and make changes in explicitly approved workspaces.
- **Work with Git:** inspect status and diffs, manage selected Git operations and review changes.
- **Run commands:** execute development commands in your computer's normal shell.
- **Connect multiple conversations:** different ChatGPT conversations can access the same project without copying it.
- **Optional tools:** use a dedicated Chrome browser through NovaGPT-managed ChromeDriver, subagents or virtual machines when configured.
- **Chat from the terminal:** ask ChatGPT from the built-in Chat page through the same dedicated browser and reopen earlier conversations. New subagent chats automatically go into the **NovaGPT - subagents** ChatGPT project (created through your visible browser if needed); an optional project URL can override it.

> **Security:** NovaGPT's shell runs with your user account's permissions; it is not sandboxed. Give access only to ChatGPT connections you trust. Keep your connector URL and token secret.

## Get started

### 1. Install NovaGPT

Download the latest release from [GitHub Releases](https://github.com/aintDatCap/novagpt/releases), or use an installer:

**Linux**

```bash
curl -fsSL https://raw.githubusercontent.com/aintDatCap/novagpt/main/script/install.sh | bash
```

macOS is not included in the GitHub Actions release matrix; build it locally with `cargo build --release -p novagpt-cli` instead of using the release installer.

**Windows (PowerShell)**

```powershell
irm https://raw.githubusercontent.com/aintDatCap/novagpt/main/script/install.ps1 | iex
```

These one-liners execute the latest remote installer immediately without saving the script locally. Review the [Bash installer](script/install.sh) or [PowerShell installer](script/install.ps1) before running them, or download and inspect a script first if you prefer. Open a new terminal after installation if the command is not yet on your PATH.

### 2. Open your project

```bash
cd /path/to/your/project
novagpt --setup
```

Follow the setup guide, check your workspace and save the settings. On subsequent launches, just run `novagpt` to open the terminal interface.

The **Overview** screen shows server and tunnel status, **Activity** shows events, **Settings** lets you manage the connection and optional features, and **Chat** asks ChatGPT from the terminal through the dedicated browser. Interface language is detected automatically, with English as the fallback.

### 3. Connect ChatGPT

1. Create a free [ngrok account](https://dashboard.ngrok.com/signup) and install [ngrok](https://ngrok.com/download). If it is missing when you try to start an ngrok connection, NovaGPT's terminal interface can offer to install it with a supported system package manager, after your confirmation.
2. Add your ngrok authentication token **locally** (copy it from your ngrok dashboard):

   ```bash
   ngrok config add-authtoken YOUR_NGROK_AUTHTOKEN
   ```

3. In NovaGPT **Settings**, enable **Automatic ngrok**, save, then choose **Start MCP + ngrok** from Overview. If you prefer, enable **Start MCP automatically** to launch your configured server whenever you open `novagpt`.
4. Once the tunnel is ready, select **Show / copy ChatGPT URL**. Press **C** to copy the full HTTPS URL. It looks like this (the token below is only a placeholder):

   ```text
   https://your-domain.ngrok-free.app/mcp?novagpt_token=YOUR_PRIVATE_TOKEN
   ```

5. In **ChatGPT on the web**, open **Plugins → + → Add custom MCP server**. Enter a name such as `NovaGPT`, paste the full URL under **Connection**, choose **No authentication** (the URL already carries your secret), review the security warning and select **Create as a plugin**. Then select the NovaGPT plugin in a conversation.

ChatGPT's availability and permission requirements vary by plan and workspace. See [OpenAI's custom MCP server guide](https://help.openai.com/en/articles/12584461-connecting-custom-mcp-servers-in-chatgpt).

**Never share or publish the full URL**: it grants authenticated access to NovaGPT, including host-shell commands. For a URL that stays the same across restarts, configure an ngrok domain assigned to your account in NovaGPT Settings.

## Everyday use

Run `novagpt` from your project folder (or use `novagpt --root /absolute/project/path`). Start and stop MCP from the TUI, view connection status and manage additional authorized workspaces from Settings. The automatic MCP startup option is **off by default**; setup itself never starts a server automatically.

You can also start a local-only server using `novagpt http --no-ngrok`, or start HTTP and ngrok together with `novagpt http --ngrok`.

## More information

- [Detailed installation, ngrok and troubleshooting](docs/INSTALLATION.md)
- [Terminal interface and keyboard shortcuts](docs/TUI_SETUP.md)
- [Security, architecture and development](docs/ARCHITECTURE.md)
- [Contributing](CONTRIBUTING.md)

NovaGPT is under active development. Some optional capabilities need additional configuration and have not yet been verified in every environment.

**License:** see [LICENSE.md](LICENSE.md).
