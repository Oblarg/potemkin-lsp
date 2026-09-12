#!/usr/bin/env bash
# Bundle the WASM host harness (runtime/wasm-host/host.js + the extism JS SDK)
# into a single self-contained file at dist/wasm-host.js so it ships in the VSIX
# without a node_modules tree. The proxy runs this with the editor's Node to
# execute `.wasm` plugins via extism's pure-JS runtime.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ext_root="$(cd "$here/.." && pwd)"
repo_root="$(cd "$ext_root/../.." && pwd)"
host_src="$repo_root/runtime/wasm-host"

# The harness's only dependency is @extism/extism; esbuild resolves it from the
# harness's own node_modules, so make sure it's installed.
if [[ ! -d "$host_src/node_modules/@extism/extism" ]]; then
  echo "installing wasm-host deps..." >&2
  ( cd "$host_src" && npm install )
fi

mkdir -p "$ext_root/dist"
"$ext_root/node_modules/.bin/esbuild" "$host_src/host.js" \
  --bundle --platform=node --format=cjs --target=node18 \
  --outfile="$ext_root/dist/wasm-host.js"

echo "staged wasm host -> $ext_root/dist/wasm-host.js ($(wc -c < "$ext_root/dist/wasm-host.js") bytes)"
