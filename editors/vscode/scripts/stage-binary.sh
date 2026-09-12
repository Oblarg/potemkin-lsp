#!/usr/bin/env bash
# Stage the potemkin proxy binary into the extension's bin/<vscode-target>/ so it
# can be bundled into a (platform-specific) VSIX.
#
# Two modes:
#   * Local dev (no env): auto-detect the host target, build, and stage it.
#   * CI cross-build: set VSCODE_TARGET (e.g. darwin-arm64) and RUST_TARGET
#     (e.g. aarch64-apple-darwin) to build that Rust target and stage it under
#     the matching VS Code target dir.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ext_root="$(cd "$here/.." && pwd)"
repo_root="$(cd "$ext_root/../.." && pwd)"
bin_base="potemkin"

vscode_target="${VSCODE_TARGET:-}"
rust_target="${RUST_TARGET:-}"

if [[ -z "$vscode_target" ]]; then
  # Auto-detect host -> VS Code's ${process.platform}-${process.arch} naming.
  case "$(uname -s)" in
    Darwin) platform="darwin" ;;
    Linux) platform="linux" ;;
    MINGW* | MSYS* | CYGWIN*) platform="win32" ;;
    *) echo "unsupported platform: $(uname -s)" >&2; exit 1 ;;
  esac
  case "$(uname -m)" in
    arm64 | aarch64) arch="arm64" ;;
    x86_64 | amd64) arch="x64" ;;
    *) echo "unsupported arch: $(uname -m)" >&2; exit 1 ;;
  esac
  vscode_target="${platform}-${arch}"
fi

exe=""
[[ "$vscode_target" == win32-* ]] && exe=".exe"

if [[ -n "$rust_target" ]]; then
  (cd "$repo_root" && cargo build --release -p potemkin-proxy --target "$rust_target")
  build_dir="$repo_root/target/$rust_target/release"
else
  (cd "$repo_root" && cargo build --release -p potemkin-proxy)
  build_dir="$repo_root/target/release"
fi

src="$build_dir/${bin_base}${exe}"
dest_dir="$ext_root/bin/${vscode_target}"
mkdir -p "$dest_dir"
cp "$src" "$dest_dir/${bin_base}${exe}"
chmod +x "$dest_dir/${bin_base}${exe}" 2>/dev/null || true
echo "staged $src -> $dest_dir/${bin_base}${exe}"
