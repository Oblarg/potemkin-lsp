# Potemkin (VS Code)

Zero-setup type pretty-printing for language servers, powered by the
[Potemkin](../../README.md) composable LSP proxy.

The extension bundles the native `potemkin` binary and, on activation, wraps the
language servers you already have installed — it's **generic over the language
server**, not tied to rust-analyzer.

## How it wraps a server (generic mechanism)

VS Code has no universal hook to inject a proxy into another extension's language
client, so Potemkin uses a small **server registry** plus a **launcher shim**:

- A registry entry says which setting overrides a server's binary
  (`rust-analyzer.server.path`, `clangd.path`, `go.alternateTools.gopls`, …),
  which command restarts it, and the default server command.
- For each wrapped server, Potemkin writes a tiny launcher script that sets
  `POTEMKIN_SERVER=<real server>` (+ config) and `exec`s the bundled binary,
  then points that server's path-setting at the launcher. This works even for
  extensions that have no env-passthrough setting.

Built-in servers: **rust-analyzer, clangd, gopls**. Add more with the
`potemkin.servers` setting — no code changes needed.

## Plugins (how libraries add pretty-printing)

Potemkin ships the proxy; the per-library pretty-printers are **plugins**. The
easiest way to install one is a **companion extension**: each library publishes a
small extension that bundles its plugin binary and registers it with Potemkin on
activation. Install the companion from the marketplace and it just works —
per-platform binary, auto-update, discoverability, all via VS Code.

Companion extensions register through Potemkin's exported API:

```ts
const potemkin = vscode.extensions.getExtension("potemkin-lsp.potemkin");
const api = await potemkin.activate();
await api.registerPlugin({
  name: "mylib",
  command: bundledBinaryPath,        // native binary; or `js:`/`wasm:` (see below)
  markers: ["MyType"],
  languages: ["rust"],               // only load under a Rust server
  languageServers: ["rust-analyzer"], // (optional) only this server
});
```

Provide exactly one of `command` (native binary, any language), `js` (a `.js`
file), or `wasm` (a single cross-platform `.wasm`). JS and WASM plugins run in the
editor's own bundled Node — WASM via the extism JS SDK harness shipped with this
extension — so authors can compile once to `.wasm` and it runs everywhere without
per-platform native builds:

```ts
await api.registerPlugin({ name: "mylib", wasm: bundledWasmPath, markers: ["MyType"] });
```

Plugins are **scoped**: Potemkin runs a separate proxy per wrapped server, and
only loads a plugin whose `languages` match that server's language (and, if set,
whose `languageServers` include that server id). A Rust plugin is therefore never
spawned under clangd/gopls. Omit both for a language-agnostic plugin.

Manifests are written to an extension-managed dir and picked up on the next
server restart. Power users can also `cargo install` a plugin and drop a manifest
in `~/.config/potemkin/plugins` (or point `potemkin.pluginsDir` at their own dir).
See `whippyunits/potemkin-plugin/vscode/` for a reference companion.

## Commands

- **Potemkin: Enable** — wrap all detected language servers.
- **Potemkin: Disable** — restore original settings.
- **Potemkin: Wrap a language server…** — pick one to wrap.
- **Potemkin: Show installed plugins** — list companion-registered plugins.
- **Potemkin: Restart language servers**, **Show status**, **Show bundled binary path**.

## Settings

- `potemkin.enabled` — auto-wrap detected servers on activation (default `true`).
- `potemkin.servers` — extra server definitions (id, pathSetting, defaultCommand, …).
- `potemkin.verbosity`, `potemkin.unicode` — forwarded to plugins.
- `potemkin.pluginsDir` — optional extra plugin-manifest dir, searched before the
  managed dir and Potemkin's default (`~/.config/potemkin/plugins`).

## Building locally

```bash
npm install
npm test                    # compile + headless activation test
./scripts/stage-binary.sh   # copy target/release/potemkin into bin/<platform>
npm run package             # produces potemkin-<version>.vsix
```
