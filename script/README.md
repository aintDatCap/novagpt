# Releases and installation

## Local development supervisor

Launch with `python script/dev.py` (Windows) or `python3 script/dev.py` (Linux/macOS). The script requires Python 3 and Cargo. It builds and starts NovaGPT once, then **never automatically rebuilds or restarts** when source files change or the server exits. After making edits, use the authenticated MCP tool `debug_restart`, or run `novagpt --root /path/to/novagpt debug-restart --port 8787` with a **debug-build** CLI when the connected ChatGPT tool list is stale. The tool and CLI command exist in POSIX and Windows debug builds, but never in release tool discovery or help. A running supervisor is always required. Failed builds keep the current server online; a failed replacement triggers rollback. Duplicate restart requests return pending. Responses acknowledge **scheduling**, not successful deployment; MCP connections briefly disconnect, and a dynamic ngrok address may change.

POSIX uses an owner-private Unix domain socket; Windows uses an ephemeral **127.0.0.1-only TCP listener** with a random 256-bit capability passed only to the owned MCP child through its environment. Each new supervisor run creates a new capability. No secrets are displayed or written to disk by the supervisor. Windows starts processes in a dedicated group and requests graceful shutdown with `CTRL_BREAK_EVENT`; if that fails, it terminates the owned process tree. Normal and release MCP processes cannot use the debug control. No auto-restart flags exist. To update an already running supervisor, stop and relaunch `script/dev.py`. For a local test without ngrok, use `python script/dev.py --no-ngrok`.

## GitHub Actions

All workflow variants use Rust-version-aware dependency caching; the three native release targets also upload Cargo timing reports. Linux release builds use Clang + LLVM LLD where installed, with a system-linker fallback. See [build performance and per-target settings](../docs/BUILD_PERFORMANCE.md) for commands, trade-offs, and measurement limits.

- **PR testing** (`.github/workflows/test.yml`): every pull request runs `cargo fmt`, Clippy with warnings denied, `cargo check` and the workspace tests on Ubuntu x64 and Windows x64. macOS jobs are excluded from GitHub CI.
- **Manual release** (`.github/workflows/release.yml`): from **Actions → Manual release → Run workflow** on **main**. The tag is derived from `[workspace.package].version` in root `Cargo.toml` (for example, `0.1.1` becomes `v0.1.1`). It runs the release checks, builds native `novagpt` binaries, uploads release archives and matching `.sha256` checksums, then creates a GitHub release with generated notes. An existing tag stops the workflow; bump the Cargo version before the next release.

The current release matrix produces:

| OS | Architecture | Archive |
| --- | --- | --- |
| Linux | x86_64 | `novagpt-linux-x86_64.tar.gz` |
| Linux | aarch64 | `novagpt-linux-aarch64.tar.gz` |
| Windows | x86_64 | `novagpt-windows-x86_64.zip` |

macOS archives are not built or published by this CI release workflow. macOS users can build from source with `cargo build --release -p novagpt-cli` or use any separately maintained distribution; the release installer cannot download a macOS archive from releases produced by this workflow.

Each archive includes the executable and `LICENSE.md`. The workflow needs permission to create releases using its built-in `GITHUB_TOKEN` (job permission `contents: write`). These archives are **not code-signed**, and an install/rollback smoke test on every supported OS is still needed before treating them as production-quality distributions.

## Installation options

To install directly from GitHub without cloning this repository, follow the [quick installation and configuration guide](../docs/INSTALLATION.md). It includes download commands for Linux/macOS and Windows, initial setup, and MCP connection instructions.

### Install using a local clone

From a clone of this repository:

**Linux** (Bash; installs for the current user, no `sudo`):

```bash
bash script/install.sh
bash script/install.sh --version v0.1.1
bash script/install.sh --install-dir "$HOME/.local/bin"
```

Requires `curl` or `wget`, `tar`, and `sha256sum` or `shasum`. By default, the latest published GitHub release is downloaded. The installer checks SHA-256, places `novagpt` in `~/.local/bin`, and adds that directory to `~/.bashrc`, `~/.zshrc` or `~/.profile` (without duplicating its own line).

**Windows x64** (PowerShell; installs for the current user, no administrator rights):

```powershell
powershell -ExecutionPolicy Bypass -File .\script\install.ps1
powershell -ExecutionPolicy Bypass -File .\script\install.ps1 -Version v0.1.1
powershell -ExecutionPolicy Bypass -File .\script\install.ps1 -InstallDir "$env:LOCALAPPDATA\Programs\NovaGPT\bin"
```

Downloads and verifies `novagpt-windows-x86_64.zip`, installs `novagpt.exe` under the user's `%LOCALAPPDATA%\Programs\NovaGPT\bin` by default, and appends that directory to the **user** PATH if absent. A new terminal is needed to pick up persistent PATH changes. Windows ARM64 does not have a published native release package in this matrix. The Windows x64 installer uses an architecture check compatible with Windows PowerShell 5.1, including 32-bit PowerShell on x64 Windows; its native Windows test cases are configured to run in CI.

Installers use `https://github.com/aintDatCap/novagpt/releases` and therefore require an **already published, accessible release** and GitHub download access; they do not build from source. This workflow no longer supplies a macOS release asset, so the Bash installer cannot install its generated releases on macOS. They never modify the machine-wide PATH or another user's installation.
