#!/usr/bin/env bash
# Build the example WASM plugin and stage the artifact next to the other example
# plugins (examples/plugins/wasm-demo.wasm) at a stable path.
#
# Requires the wasm32-unknown-unknown target:
#   rustup target add wasm32-unknown-unknown
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
crate="$here/wasm-demo"

( cd "$crate" && cargo build --release --target wasm32-unknown-unknown )

src="$crate/target/wasm32-unknown-unknown/release/wasm_demo.wasm"
dst="$here/wasm-demo.wasm"
cp "$src" "$dst"
echo "staged $dst ($(wc -c < "$dst") bytes)"
