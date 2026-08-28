#!/bin/sh
set -eu

repository="alexanderattar/agent-sync"
install_dir="${AGENT_SYNC_INSTALL_DIR:-$HOME/.local/bin}"
version="${AGENT_SYNC_VERSION:-latest}"
mode="install"
attestation_mode="auto"

usage() {
  printf '%s\n' \
    "Install agent-sync from a GitHub release." \
    "" \
    "Usage: install.sh [--version vX.Y.Z] [--install-dir PATH]" \
    "                  [--require-attestation] [--uninstall]"
}

while [ "$#" -gt 0 ]; do
  case "$1" in
    --version)
      [ "$#" -ge 2 ] || { printf '%s\n' "--version needs a value" >&2; exit 2; }
      version="$2"
      shift 2
      ;;
    --install-dir)
      [ "$#" -ge 2 ] || { printf '%s\n' "--install-dir needs a value" >&2; exit 2; }
      install_dir="$2"
      shift 2
      ;;
    --require-attestation)
      attestation_mode="required"
      shift
      ;;
    --uninstall)
      mode="uninstall"
      shift
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      printf 'Unknown option: %s\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

binary="$install_dir/agent-sync"
sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    printf '%s\n' "sha256sum or shasum is required to verify agent-sync" >&2
    return 1
  fi
}

case "$version" in
  latest|v[0-9]*) ;;
  *) printf 'Invalid release version: %s\n' "$version" >&2; exit 2 ;;
esac

case "$version" in
  *[!0-9A-Za-z.+-]*) printf 'Invalid release version: %s\n' "$version" >&2; exit 2 ;;
esac

if [ -L "$install_dir" ]; then
  printf 'Refusing to use symlinked install directory %s\n' "$install_dir" >&2
  exit 1
fi
if [ -L "$binary" ]; then
  if [ "$mode" = "uninstall" ]; then
    printf 'Refusing to remove symlinked path %s\n' "$binary" >&2
  else
    printf 'Refusing to replace non-regular path %s\n' "$binary" >&2
  fi
  exit 1
fi
if [ -e "$binary" ] && [ ! -f "$binary" ]; then
  printf 'Refusing to use non-regular path %s\n' "$binary" >&2
  exit 1
fi

expected_target_sha256=""
target_existed=0
if [ -f "$binary" ]; then
  expected_target_sha256="$(sha256_file "$binary")"
  target_existed=1
elif [ "$mode" = "uninstall" ]; then
  printf 'agent-sync is not installed at %s\n' "$binary"
  exit 0
fi

case "$(uname -s)" in
  Darwin) os="apple-darwin" ;;
  Linux) os="unknown-linux-gnu" ;;
  *) printf 'Unsupported operating system: %s\n' "$(uname -s)" >&2; exit 1 ;;
esac

case "$(uname -m)" in
  arm64|aarch64) arch="aarch64" ;;
  x86_64|amd64) arch="x86_64" ;;
  *) printf 'Unsupported architecture: %s\n' "$(uname -m)" >&2; exit 1 ;;
esac

target="$arch-$os"
asset="agent-sync-$target.tar.gz"

command -v curl >/dev/null 2>&1 || {
  printf '%s\n' "curl is required to download agent-sync" >&2
  exit 1
}

attestation_enabled=0
if command -v gh >/dev/null 2>&1 \
  && gh attestation verify --help >/dev/null 2>&1 \
  && gh auth status --hostname github.com >/dev/null 2>&1; then
  attestation_enabled=1
fi

if [ "$attestation_mode" = "required" ] && [ "$attestation_enabled" -ne 1 ]; then
  printf '%s\n' \
    "A current, authenticated GitHub CLI is required for attestation verification." \
    "Run 'gh auth login', then retry." >&2
  exit 1
fi

resolved_version="$version"
if [ "$attestation_enabled" -eq 1 ] && [ "$version" = "latest" ]; then
  if ! resolved_version="$(gh release view --repo "$repository" --json tagName --jq .tagName)"; then
    printf '%s\n' "Could not resolve the latest release for attestation verification." >&2
    exit 1
  fi
  case "$resolved_version" in
    v[0-9]*) ;;
    *) printf 'GitHub returned an invalid release version: %s\n' "$resolved_version" >&2; exit 1 ;;
  esac
  case "$resolved_version" in
    *[!0-9A-Za-z.+-]*) printf 'GitHub returned an invalid release version: %s\n' "$resolved_version" >&2; exit 1 ;;
  esac
fi

if [ "$resolved_version" = "latest" ]; then
  base_url="https://github.com/$repository/releases/latest/download"
else
  base_url="https://github.com/$repository/releases/download/$resolved_version"
fi

umask 077
mkdir -p "$install_dir"
if [ -L "$install_dir" ] || [ ! -d "$install_dir" ]; then
  printf 'Refusing to use unsafe install directory %s\n' "$install_dir" >&2
  exit 1
fi
if [ -L "$binary" ] || { [ -e "$binary" ] && [ ! -f "$binary" ]; }; then
  printf 'Refusing to use non-regular path %s\n' "$binary" >&2
  exit 1
fi

temp_dir="$(mktemp -d "$install_dir/.agent-sync-install.XXXXXX")"
staged=""
trap 'rm -rf "$temp_dir"; if [ -n "$staged" ]; then rm -f "$staged"; fi' EXIT HUP INT TERM

curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
  "$base_url/$asset" --output "$temp_dir/$asset"
curl --proto '=https' --tlsv1.2 --fail --location --silent --show-error \
  "$base_url/$asset.sha256" --output "$temp_dir/$asset.sha256"

expected="$(cut -d ' ' -f 1 "$temp_dir/$asset.sha256")"
case "$expected" in
  *[!0-9a-fA-F]*|'') printf '%s\n' "Release checksum is malformed" >&2; exit 1 ;;
esac
[ "${#expected}" -eq 64 ] || {
  printf '%s\n' "Release checksum is malformed" >&2
  exit 1
}
actual="$(sha256_file "$temp_dir/$asset")"
[ "$actual" = "$expected" ] || {
  printf '%s\n' "Checksum verification failed" >&2
  exit 1
}

if [ "$attestation_enabled" -eq 1 ]; then
  if ! gh attestation verify "$temp_dir/$asset" \
    --repo "$repository" \
    --signer-workflow "$repository/.github/workflows/release.yml" \
    --source-ref "refs/tags/$resolved_version" \
    --deny-self-hosted-runners >/dev/null; then
    printf '%s\n' "GitHub build provenance verification failed." >&2
    exit 1
  fi
  printf 'Verified GitHub build provenance for %s\n' "$resolved_version"
else
  printf '%s\n' \
    "GitHub attestation verification is unavailable; verified the release checksum only." >&2
fi

mkdir -p "$temp_dir/package"
tar -C "$temp_dir/package" -xzf "$temp_dir/$asset" ./agent-sync
[ -f "$temp_dir/package/agent-sync" ] && [ ! -L "$temp_dir/package/agent-sync" ] || {
  printf '%s\n' "Release archive does not contain a regular agent-sync binary" >&2
  exit 1
}

if [ "$mode" = "uninstall" ]; then
  chmod 755 "$temp_dir/package/agent-sync"
  "$temp_dir/package/agent-sync" __install-remove \
    --target "$binary" \
    --expected-sha256 "$expected_target_sha256"
  printf 'Removed %s\n' "$binary"
  exit 0
fi

staged="$(mktemp "$install_dir/.agent-sync.new.XXXXXX")"
cp "$temp_dir/package/agent-sync" "$staged"
chmod 755 "$staged"
if [ "$target_existed" -eq 1 ]; then
  "$staged" __install-commit \
    --target "$binary" \
    --expected-sha256 "$expected_target_sha256"
else
  "$staged" __install-commit --target "$binary"
fi
staged=""

printf 'Installed agent-sync at %s\n' "$binary"
case ":$PATH:" in
  *":$install_dir:"*) ;;
  *) printf 'Run %s now, or add %s to PATH to use: agent-sync\n' "$binary" "$install_dir" ;;
esac
