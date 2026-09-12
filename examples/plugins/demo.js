#!/usr/bin/env node
// Reference Potemkin plugin, in raw JavaScript, demonstrating the `js` transport.
//
// It speaks newline-delimited JSON over stdio: one request object per line in,
// one response object per line out — exactly like the subprocess protocol. This
// trivial plugin rewrites the type text `JsType<...>` into a friendlier
// `Js{...}` form to show the mechanism.
//
// Potemkin runs this with the editor's bundled Node (or `node` on PATH); plugin
// authors don't need a separate Node install inside VS Code/Cursor.

"use strict";

const NAME = "demo-js";
const MARKERS = ["JsType<"];

function transformOne(text) {
  return text.replace(/JsType<([^>]*)>/g, (_m, inner) => "Js{" + inner + "}");
}

function handle(req) {
  switch (req.method) {
    case "initialize":
      return { id: req.id, result: { name: NAME, markers: MARKERS } };
    case "transform": {
      const items = (req.params && req.params.items) || [];
      return {
        id: req.id,
        result: { items: items.map((it) => transformOne(it.text || "")) },
      };
    }
    case "shutdown":
      return { id: req.id, result: {} };
    default:
      return { id: req.id != null ? req.id : 0, error: "unknown method: " + req.method };
  }
}

const rl = require("readline").createInterface({ input: process.stdin });
rl.on("line", (line) => {
  line = line.trim();
  if (!line) return;
  let req;
  try {
    req = JSON.parse(line);
    const resp = handle(req);
    process.stdout.write(JSON.stringify(resp) + "\n");
  } catch (e) {
    // Never crash the proxy's session.
    process.stdout.write(JSON.stringify({ id: 0, error: String(e) }) + "\n");
  }
  if (req && req.method === "shutdown") rl.close();
});
