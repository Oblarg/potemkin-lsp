#!/usr/bin/env node
// Potemkin WASM host harness.
//
// Runs a WebAssembly plugin in the JavaScript runtime (Node — including the
// editor's bundled Node) using the extism JS SDK, exposing it over the same
// newline-delimited-JSON protocol as the subprocess/JS transports. This is what
// lets a plugin author compile once to `.wasm` and have it run everywhere Node
// is available, with no native wasm runtime baked into the proxy.
//
// Usage: node host.js <plugin.wasm>
// Speaks JSONL on stdio: one request object per line in, one response per line
// out. The proxy spawns this; it is not meant to be run by hand.
//
// The extism plugin exports functions named after the protocol methods
// (`initialize`, `transform`), each taking the params JSON as input bytes and
// returning the result JSON as output bytes.

"use strict";

const readline = require("readline");

const wasmPath = process.argv[2];
if (!wasmPath) {
  process.stderr.write("potemkin wasm-host: missing <plugin.wasm> argument\n");
  process.exit(2);
}

// Lazily create the plugin once; every request reuses it. `createPlugin` is
// async, so `pluginPromise` is awaited on the first call.
let pluginPromise = null;
function getPlugin() {
  if (!pluginPromise) {
    // @extism/extism's default export is `createPlugin`.
    const createPlugin = require("@extism/extism");
    pluginPromise = createPlugin(wasmPath, { useWasi: true });
  }
  return pluginPromise;
}

async function handle(req) {
  const id = req && req.id != null ? req.id : 0;
  try {
    // `shutdown` is host-side bookkeeping; don't require the module to export it.
    if (req.method === "shutdown") {
      return { id, result: {} };
    }
    const plugin = await getPlugin();
    const input = JSON.stringify(req.params || {});
    const out = await plugin.call(req.method, input);
    // `out` is null when the export returns no bytes; treat as empty object.
    const result = out ? out.json() : {};
    return { id, result };
  } catch (e) {
    return { id, error: String((e && e.stack) || e) };
  }
}

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  line = line.trim();
  if (!line) return;
  let req;
  try {
    req = JSON.parse(line);
  } catch (e) {
    process.stdout.write(JSON.stringify({ id: 0, error: "invalid JSON: " + String(e) }) + "\n");
    return;
  }
  handle(req).then((resp) => {
    process.stdout.write(JSON.stringify(resp) + "\n");
    if (req.method === "shutdown") rl.close();
  });
});
