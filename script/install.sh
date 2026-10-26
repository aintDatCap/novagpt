#!/usr/bin/env bash
# Install NovaGPT release binaries on Linux and macOS (per-user, no sudo).
set -euo pipefail

REPOSITORY="aintDatCap/novagpt"
VERSION="${NOVAGPT_VERSION:-latest}"
INSTALL_DIR="${NOVAGPT_INSTALL_DIR:-$HOME/.local/bin}"

usage() {
  cat <<'USAGE'
Usage: bash script/install.sh [--version vX.Y.Z] [--install-dir /absolute/path]

Downloads a NovaGPT GitHub release for this OS/CPU, checks SHA-256, installs
novagpt to ~/.local/bin (by default), and adds that directory to your shell PATH.
Use --version to install a specific release instead of the latest published one.
USAGE
}

while (( $# )); do
  case "$1" in
    --version)
      (( $# >= 2 )) || { echo "Missing value for --version" >&2; exit 2; }
      VERSION="$2"
      shift 2
      ;;
    --install-dir)
      (( $# >= 2 )) || { echo "Missing value for --install-dir" >&2; exit 2; }
      INSTALL_DIR="$2"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ "$VERSION" != "latest" && ! "$VERSION" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "Version must be 'latest' or a tag such as v0.1.0." >&2
  exit 2
fi
case "$INSTALL_DIR" in
  /*) ;;
  *) echo "Install directory must be an absolute path." >&2; exit 2 ;;
esac

case "$(uname -s)" in
  Linux) platform_os=linux ;;
  Darwin) platform_os=macos ;;
  *) echo "Unsupported OS (use install.ps1 on Windows)." >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64) platform_arch=x86_64 ;;
  aarch64|arm64) platform_arch=aarch64 ;;
  *) echo "Unsupported CPU architecture: $(uname -m)" >&2; exit 1 ;;
esac

asset="novagpt-${platform_os}-${platform_arch}.tar.gz"
if [[ "$VERSION" == "latest" ]]; then
  base_url="https://github.com/$REPOSITORY/releases/latest/download"
else
  base_url="https://github.com/$REPOSITORY/releases/download/$VERSION"
fi

tmp_dir="$(mktemp -d)"
staged=""
cleanup() {
  rm -rf "$tmp_dir"
  if [[ -n "$staged" ]]; then rm -f "$staged"; fi
}
trap cleanup EXIT

download() {
  local url="$1" destination="$2"
  if command -v curl >/dev/null 2>&1; then
    curl --fail --location --silent --show-error --retry 3 --proto '=https' --proto-redir '=https' "$url" -o "$destination"
  elif command -v wget >/dev/null 2>&1; then
    wget --quiet --output-document="$destination" "$url"
  else
    echo "curl or wget is required to download NovaGPT." >&2
    exit 1
  fi
}

echo "Downloading $asset from $VERSION..."
download "$base_url/$asset" "$tmp_dir/$asset"
download "$base_url/$asset.sha256" "$tmp_dir/$asset.sha256"

expected="$(tr -d '\r\n' < "$tmp_dir/$asset.sha256")"
if [[ ! "$expected" =~ ^[0-9a-fA-F]{64}$ ]]; then
  echo "Invalid SHA-256 file in release." >&2
  exit 1
fi
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "$tmp_dir/$asset" | cut -d ' ' -f 1)"
elif command -v shasum >/dev/null 2>&1; then
  actual="$(shasum -a 256 "$tmp_dir/$asset" | cut -d ' ' -f 1)"
else
  echo "sha256sum or shasum is required to verify the download." >&2
  exit 1
fi
if [[ "$actual" != "$expected" ]]; then
  echo "SHA-256 verification failed; refusing to install." >&2
  exit 1
fi

tar -xzf "$tmp_dir/$asset" -C "$tmp_dir" novagpt
if [[ ! -f "$tmp_dir/novagpt" || -L "$tmp_dir/novagpt" ]]; then
  echo "Release archive does not contain a normal novagpt executable." >&2
  exit 1
fi

mkdir -p "$INSTALL_DIR"
staged="$(mktemp "$INSTALL_DIR/.novagpt.XXXXXXXX")"
install -m 755 "$tmp_dir/novagpt" "$staged"
mv -f "$staged" "$INSTALL_DIR/novagpt"
staged=""

# A child process cannot update the parent shell PATH: update its startup file.
case "${SHELL:-}" in
  */zsh)
    profile="${ZDOTDIR:-$HOME}/.zshrc"
    ;;
  */bash)
    profile="$HOME/.bashrc"
    ;;
  *)
    profile="$HOME/.profile"
    ;;
esac
printf -v quoted_dir '%q' "$INSTALL_DIR"
path_line="export PATH=${quoted_dir}:\$PATH"
if [[ ! -f "$profile" ]] || ! grep -Fqx -- "$path_line" "$profile"; then
  printf '\n# NovaGPT executable\n%s\n' "$path_line" >> "$profile"
  echo "Added $INSTALL_DIR to PATH in $profile."
fi

echo "Installed: $INSTALL_DIR/novagpt"
echo "Open a new terminal, or run: source \"$profile\""
