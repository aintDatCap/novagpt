# Releases and installation

## GitHub Actions

- **PR testing** (`.github/workflows/test.yml`): every pull request runs `cargo fmt`, Clippy with warnings denied, `cargo check` and the workspace tests on Ubuntu x64, Windows x64 and macOS ARM64.
- **Manual release** (`.github/workflows/release.yml`): from **Actions → Manual release → Run workflow** on **main**. The tag is derived from `[workspace.package].version` in root `Cargo.toml` (for example, `0.1.0` becomes `v0.1.0`). It runs the release checks, builds native `novagpt` binaries, uploads release archives and matching `.sha256` checksums, then creates a GitHub release with generated notes. An existing tag stops the workflow; bump the Cargo version before the next release.

The current release matrix produces:

| OS | Architecture | Archive |
| --- | --- | --- |
| Linux | x86_64 | `novagpt-linux-x86_64.tar.gz` |
| Linux | aarch64 | `novagpt-linux-aarch64.tar.gz` |
| macOS | x86_64 | `novagpt-macos-x86_64.tar.gz` |
| macOS | Apple Silicon (aarch64) | `novagpt-macos-aarch64.tar.gz` |
| Windows | x86_64 | `novagpt-windows-x86_64.zip` |

Each archive includes the executable and `LICENSE.md`. The workflow needs permission to create releases using its built-in `GITHUB_TOKEN` (job permission `contents: write`). These archives are **not code-signed**, and an install/rollback smoke test on every supported OS is still needed before treating them as production-quality distributions.

## Install from a published GitHub release

From a clone of this repository:

**Linux or macOS** (Bash; installs for the current user, no `sudo`):

```bash
bash script/install.sh
bash script/install.sh --version v0.1.0
bash script/install.sh --install-dir "$HOME/.local/bin"
```

Requires `curl` or `wget`, `tar`, and `sha256sum` or `shasum`. By default, the latest published GitHub release is downloaded. The installer checks SHA-256, places `novagpt` in `~/.local/bin`, and adds that directory to `~/.bashrc`, `~/.zshrc` or `~/.profile` (without duplicating its own line).

**Windows x64** (PowerShell; installs for the current user, no administrator rights):

```powershell
powershell -ExecutionPolicy Bypass -File .\script\install.ps1
powershell -ExecutionPolicy Bypass -File .\script\install.ps1 -Version v0.1.0
powershell -ExecutionPolicy Bypass -File .\script\install.ps1 -InstallDir "$env:LOCALAPPDATA\Programs\NovaGPT\bin"
```

Downloads and verifies `novagpt-windows-x86_64.zip`, installs `novagpt.exe` under the user's `%LOCALAPPDATA%\Programs\NovaGPT\bin` by default, and appends that directory to the **user** PATH if absent. A new terminal is needed to pick up persistent PATH changes. Windows ARM64 does not have a published native release package in this matrix.

Installers use `https://github.com/aintDatCap/novagpt/releases` and therefore require an **already published, accessible release** and GitHub download access; they do not build from source. They never touch CodexPro, machine-wide PATH, or another user's installation.
