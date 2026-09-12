// Headless test of the bundled extension's generic activation logic. Mocks the
// `vscode` module, loads dist/extension.js, and verifies that enabling Potemkin:
//   - only wraps servers whose owning extension is installed,
//   - points the server's path-setting at a generated launcher shim,
//   - writes a launcher that sets POTEMKIN_SERVER and execs the bundled binary,
//   - restarts the server, and restores everything on disable.
"use strict";

const fs = require("fs");
const os = require("os");
const path = require("path");
const assert = require("assert");
const Module = require("module");

const extRoot = path.join(__dirname, "..");
const bundle = path.join(extRoot, "dist", "extension.js");
const globalStorage = fs.mkdtempSync(path.join(os.tmpdir(), "potemkin-gs-"));

// ---- in-memory config store keyed by "<section>.<key>" -------------------
const store = new Map();
const executed = [];

function makeConfig(section) {
  return {
    get(key, def) {
      const k = `${section}.${key}`;
      return store.has(k) ? store.get(k) : def;
    },
    update(key, value) {
      const k = `${section}.${key}`;
      if (value === undefined) store.delete(k);
      else store.set(k, value);
      return Promise.resolve();
    },
  };
}

// Only rust-analyzer is "installed" in this test.
const installed = new Set(["rust-lang.rust-analyzer"]);

const vscodeMock = {
  ConfigurationTarget: { Global: 1, Workspace: 2, WorkspaceFolder: 3 },
  Uri: { file: (p) => ({ fsPath: p }) },
  workspace: {
    workspaceFolders: [{ uri: { fsPath: "/tmp/proj" } }],
    getConfiguration: (section) => makeConfig(section),
  },
  window: {
    showInformationMessage: () => Promise.resolve(undefined),
    showErrorMessage: (m) => {
      throw new Error("unexpected error message: " + m);
    },
    showQuickPick: () => Promise.resolve(undefined),
  },
  extensions: {
    getExtension: (id) => (installed.has(id) ? { id } : undefined),
  },
  commands: {
    registerCommand: (id, fn) => ({ id, fn, dispose() {} }),
    getCommands: () => Promise.resolve(["rust-analyzer.restartServer"]),
    executeCommand: (id) => {
      executed.push(id);
      return Promise.resolve();
    },
  },
};

const origLoad = Module._load;
Module._load = function (request) {
  if (request === "vscode") return vscodeMock;
  return origLoad.apply(this, arguments);
};

function makeContext() {
  const m = new Map();
  return {
    extensionPath: extRoot,
    globalStorageUri: { fsPath: globalStorage },
    subscriptions: [],
    workspaceState: {
      get(k) {
        return m.get(k);
      },
      update(k, v) {
        if (v === undefined) m.delete(k);
        else m.set(k, v);
        return Promise.resolve();
      },
      keys() {
        return Array.from(m.keys());
      },
    },
  };
}

(async () => {
  const ext = require(bundle);
  const context = makeContext();

  // A server is only wrapped when at least one *matching* plugin is installed
  // (otherwise it would spawn an idle pass-through proxy). Seed one for the sole
  // installed server (rust-analyzer) before activating, so wrapping kicks in.
  const managedDir = path.join(globalStorage, "plugins");
  fs.mkdirSync(managedDir, { recursive: true });
  fs.writeFileSync(
    path.join(managedDir, "seed.json"),
    JSON.stringify({
      name: "seed",
      markers: ["X"],
      languages: ["rust"],
      language_servers: ["rust-analyzer"],
      transport: { type: "subprocess", command: "/opt/seed" },
    }),
  );

  const api = await ext.activate(context);

  const launcher = path.join(
    globalStorage,
    "launchers",
    process.platform === "win32" ? "potemkin-rust-analyzer.cmd" : "potemkin-rust-analyzer.sh",
  );

  // rust-analyzer is installed -> wrapped; its path setting points at the launcher.
  assert.strictEqual(
    store.get("rust-analyzer.server.path"),
    launcher,
    `expected rust-analyzer.server.path == ${launcher}, got ${store.get("rust-analyzer.server.path")}`,
  );

  // clangd / gopls are NOT installed -> untouched.
  assert.strictEqual(store.get("clangd.path"), undefined, "clangd should not be wrapped");
  assert.strictEqual(store.get("go.alternateTools"), undefined, "gopls should not be wrapped");

  // Launcher shim exists, sets POTEMKIN_SERVER, and execs the bundled binary.
  assert.ok(fs.existsSync(launcher), "launcher shim should be written");
  const shim = fs.readFileSync(launcher, "utf8");
  assert.ok(/POTEMKIN_SERVER=.*rust-analyzer/.test(shim), "launcher should set POTEMKIN_SERVER");
  const expectedBin = path.join(
    extRoot,
    "bin",
    `${process.platform}-${process.arch}`,
    process.platform === "win32" ? "potemkin.exe" : "potemkin",
  );
  assert.ok(shim.includes(expectedBin), "launcher should exec the bundled binary");

  // Launcher exposes the managed plugins dir so companion-registered plugins load.
  assert.ok(
    shim.includes(managedDir),
    "launcher should point POTEMKIN_PLUGINS_DIR at the managed dir",
  );

  // Launcher tells the proxy which language + server it serves, for filtering.
  assert.ok(
    /POTEMKIN_LANGUAGES=.*rust/.test(shim),
    "launcher should set POTEMKIN_LANGUAGES for the wrapped server",
  );
  assert.ok(
    /POTEMKIN_SERVER_ID=.*rust-analyzer/.test(shim),
    "launcher should set POTEMKIN_SERVER_ID for the wrapped server",
  );

  // Launcher hands the proxy a Node runtime + the WASM harness so JS/WASM
  // plugins run in the editor's runtime.
  assert.ok(
    new RegExp(`POTEMKIN_NODE=.*${process.execPath.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}`).test(
      shim,
    ),
    "launcher should set POTEMKIN_NODE to the editor's Node",
  );
  const wasmHost = path.join(extRoot, "dist", "wasm-host.js");
  assert.ok(fs.existsSync(wasmHost), "compile should produce dist/wasm-host.js");
  assert.ok(
    shim.includes(wasmHost),
    "launcher should set POTEMKIN_WASM_HOST at the bundled harness",
  );

  // The per-directory multiplexer is off by default: the launcher must not set
  // POTEMKIN_MULTIPLEX unless the user opts in.
  assert.ok(
    !shim.includes("POTEMKIN_MULTIPLEX"),
    "launcher should not enable the multiplexer by default",
  );

  // Per-server "Raw:" rendering: rust-analyzer's built-in default is enabled,
  // markdown (rust fence), with a separator. The launcher bakes these in so the
  // proxy renders the section to match this server's hover format.
  assert.ok(/POTEMKIN_RAW=.*1/.test(shim), "rust-analyzer raw should be enabled by default");
  assert.ok(/POTEMKIN_RAW_FENCE=.*rust/.test(shim), "rust-analyzer raw should use a rust fence");
  assert.ok(/POTEMKIN_RAW_SEPARATOR=.*1/.test(shim), "rust-analyzer raw should include a separator");

  assert.ok(
    executed.includes("rust-analyzer.restartServer"),
    "expected rust-analyzer.restartServer to be invoked",
  );

  // ---- companion-extension API: registerPlugin writes a manifest ---------
  assert.ok(api && typeof api.registerPlugin === "function", "activate should return the API");
  assert.strictEqual(api.pluginsDir, managedDir, "api.pluginsDir should be the managed dir");
  await api.registerPlugin({
    name: "whippyunits",
    markers: ["Quantity"],
    languages: ["rust"],
    languageServers: ["rust-analyzer"],
    command: "/opt/whippyunits/whippyunits-potemkin-plugin",
  });
  const manifestFile = path.join(managedDir, "whippyunits.json");
  assert.ok(fs.existsSync(manifestFile), "registerPlugin should write a manifest");
  const manifest = JSON.parse(fs.readFileSync(manifestFile, "utf8"));
  assert.strictEqual(manifest.name, "whippyunits");
  assert.strictEqual(manifest.transport.type, "subprocess");
  assert.strictEqual(manifest.transport.command, "/opt/whippyunits/whippyunits-potemkin-plugin");
  assert.deepStrictEqual(manifest.markers, ["Quantity"]);
  assert.deepStrictEqual(manifest.languages, ["rust"], "manifest should record languages");
  assert.deepStrictEqual(
    manifest.language_servers,
    ["rust-analyzer"],
    "manifest should record language_servers",
  );

  // unregisterPlugin removes it.
  await api.unregisterPlugin("whippyunits");
  assert.ok(!fs.existsSync(manifestFile), "unregisterPlugin should remove the manifest");

  // ---- registerPlugin supports the js and wasm transports ----------------
  await api.registerPlugin({ name: "jsplug", js: "/opt/jsplug/plugin.js", markers: ["JsType<"] });
  const jsManifest = JSON.parse(fs.readFileSync(path.join(managedDir, "jsplug.json"), "utf8"));
  assert.strictEqual(jsManifest.transport.type, "js", "js registration -> js transport");
  assert.strictEqual(jsManifest.transport.path, "/opt/jsplug/plugin.js");

  await api.registerPlugin({ name: "wasmplug", wasm: "/opt/wasmplug/plugin.wasm" });
  const wasmManifest = JSON.parse(fs.readFileSync(path.join(managedDir, "wasmplug.json"), "utf8"));
  assert.strictEqual(wasmManifest.transport.type, "wasm", "wasm registration -> wasm transport");
  assert.strictEqual(wasmManifest.transport.path, "/opt/wasmplug/plugin.wasm");

  // Ambiguous/empty transport is rejected.
  let threw = false;
  try {
    await api.registerPlugin({ name: "bad", command: "/x", wasm: "/y" });
  } catch {
    threw = true;
  }
  assert.ok(threw, "registerPlugin should reject more than one of command/js/wasm");

  await api.unregisterPlugin("jsplug");
  await api.unregisterPlugin("wasmplug");

  // Disable restores the original (absent) setting.
  const disable = context.subscriptions.find((s) => s.id === "potemkin.disable");
  assert.ok(disable, "disable command should be registered");
  await disable.fn();
  assert.strictEqual(
    store.get("rust-analyzer.server.path"),
    undefined,
    "disable should restore the original (unset) server.path",
  );

  // ---- gating: an installed server with no matching plugin is NOT wrapped ---
  // Pretend clangd's extension is now installed, but with no C++ plugin present.
  installed.add("llvm-vs-code-extensions.vscode-clangd");
  await ext.activate(makeContext());
  assert.strictEqual(
    store.get("clangd.path"),
    undefined,
    "clangd must not be wrapped (no matching plugin) — so it spawns no Potemkin process",
  );

  // Add a matching C++ plugin, re-activate: clangd should now be wrapped.
  fs.writeFileSync(
    path.join(managedDir, "cpp.json"),
    JSON.stringify({
      name: "cpp",
      languages: ["cpp"],
      language_servers: ["clangd"],
      transport: { type: "subprocess", command: "/opt/cpp" },
    }),
  );
  const clangdLauncher = path.join(
    globalStorage,
    "launchers",
    process.platform === "win32" ? "potemkin-clangd.cmd" : "potemkin-clangd.sh",
  );
  await ext.activate(makeContext());
  assert.strictEqual(
    store.get("clangd.path"),
    clangdLauncher,
    "clangd should be wrapped once a matching plugin is installed",
  );

  // Remove the C++ plugin again, re-activate: clangd should be unwrapped.
  fs.rmSync(path.join(managedDir, "cpp.json"));
  await ext.activate(makeContext());
  assert.strictEqual(
    store.get("clangd.path"),
    undefined,
    "clangd should be unwrapped again once its last matching plugin is gone",
  );
  installed.delete("llvm-vs-code-extensions.vscode-clangd");

  // ---- prune: manifests owned by a disabled/uninstalled extension are removed ---
  const orphan = path.join(managedDir, "orphan.json");
  fs.writeFileSync(
    orphan,
    JSON.stringify({ name: "orphan", owner: "ghost.not-installed", transport: { type: "subprocess", command: "/x" } }),
  );
  const kept = path.join(managedDir, "kept.json");
  fs.writeFileSync(
    kept,
    JSON.stringify({ name: "kept", owner: "rust-lang.rust-analyzer", transport: { type: "subprocess", command: "/y" } }),
  );
  const manual = path.join(managedDir, "manual.json");
  fs.writeFileSync(
    manual,
    JSON.stringify({ name: "manual", transport: { type: "subprocess", command: "/z" } }),
  );
  // Re-activate: activate() prunes orphaned manifests before wrapping.
  await ext.activate(makeContext());
  assert.ok(!fs.existsSync(orphan), "prune should remove a manifest owned by a missing extension");
  assert.ok(fs.existsSync(kept), "prune should keep a manifest owned by an installed extension");
  assert.ok(fs.existsSync(manual), "prune should keep hand-maintained manifests (no owner)");

  fs.rmSync(globalStorage, { recursive: true, force: true });
  console.log(
    "generic activation test OK: wraps only servers with matching plugins, via launcher; unwraps when plugins go; restores; prunes orphans",
  );
  // Exit promptly so the debounced reconcile timer scheduled by register/unregister
  // can't fire after we've torn down the temp storage.
  process.exit(0);
})().catch((e) => {
  console.error(e);
  process.exit(1);
});
