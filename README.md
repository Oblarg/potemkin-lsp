# Potemkin

Potemkin sits between your editor and a real language server (e.g.
`rust-analyzer`), intercepts the server's responses, and feeds them through
a pipeline of plugins that rewrite deep generic types into a human-readable
format.

Potemkin can also multiplex language server sessions for different parts of the
same project, using a per-directory config file.  This is useful for language servers
that do not yet support per-directory configuration.

## Layout

```
potemkin/
├── Cargo.toml                       # workspace
├── crates/
│   ├── potemkin-plugin-api/         # shared contract
│   │   ├── src/lib.rs               # internal Plugin trait
│   │   └── src/protocol.rs          # cross-language wire protocol + manifest
│   └── potemkin-proxy/              # the proxy binary + plugin pipeline
│       ├── src/lsp.rs               # LSP Content-Length framing
│       ├── src/lib.rs               # Pipeline: detect + rewrite hover/inlay text
│       ├── src/plugin.rs            # RpcTransport + ProtocolPlugin (transport-agnostic)
│       ├── src/subprocess.rs        # subprocess/JS stdio transport
│       ├── src/config_layer.rs      # potemkin.toml discovery/merge (per-dir config)
│       ├── src/mux.rs               # opt-in multiplexer: one backend per config scope
│       ├── src/main.rs              # spawn backend server(s), load plugins, pump stdio
│       └── tests/                   # end-to-end tests (subprocess / js / wasm)
├── editors/vscode/                  # VS Code extension (bundles binary + wasm host, wraps servers, plugin API)
├── runtime/wasm-host/               # Node harness that runs .wasm plugins via the extism JS SDK
├── examples/plugins/                # demo.py, demo.js, wasm-demo/ reference plugins
└── DESIGN.md                        # architecture notes + decisions
```

## How to use Potemkin

Either:

1. Use the VS Code extension: `editors/vscode/` bundles the prebuilt native
   binary and, on activation, points a supported server's path setting
   (`rust-analyzer.server.path`, `clangd.path`, `go.alternateTools.gopls`, …) at
   it. Wrapping is plugin-gated: Potemkin only inserts itself in front of
   servers that have at least one matching plugin installed.
2. Build the binary and point your editor at it: `cargo build --release`, then point your
   editor's server path at `target/release/potemkin` and tell it which language
   server to wrap via `POTEMKIN_SERVER`.

## How to get plugins

Either:

1. Via the VS Code extension: Plugins install as **companion extensions**: 
   a library publishes a small extension that bundles its plugin binary and registers it with
   Potemkin via the exported `registerPlugin` API. Marketplace install → it just works.
   See `whippyunits/potemkin-plugin/vscode/` for a reference plugin extension.
2. As standalone binaries: Build the plugin binary and drop its `*.json` manifest
   into a plugin search directory: any dir on `POTEMKIN_PLUGINS_DIR` (an OS-style path list)
   or the default `~/.config/potemkin/plugins`.

## Writing a plugin (any language)

A plugin implements `initialize` + `transform` over
newline-delimited JSON.  Plugins may run natively on the host or in the 
editor's JS runtime.

Deliver a plugin by either (a) shipping a companion extension that calls
`registerPlugin` (with `command`, `js`, or `wasm`), or (b) dropping a manifest in
the plugins dir (`POTEMKIN_PLUGINS_DIR` — an OS path list — default
`~/.config/potemkin/plugins`). Potemkin loads and composes plugins at startup
(de-duplicating by name). See `crates/potemkin-plugin-api/src/protocol.rs` for
the full contract.

## Per-directory server config (`potemkin.toml`, opt-in)

Some language servers can't hold genuinely different settings for different
directories: rust-analyzer, for example, folds all client config into one unit
and runs a single session (its on-disk `rust-analyzer.toml` is meant to fix this
but is still incomplete). Potemkin can multiplex language server sessions for different parts of the
same project, using a per-directory config file.

Drop a `potemkin.toml` at a project (or sub-directory) root:

```toml
# Contents mirror the server's own config namespace; dotted keys nest.
[servers.rust-analyzer]
checkOnSave = false
cargo.features = ["ci"]
```

Enable the feature with the `potemkin.multiplex` VS Code setting, or `POTEMKIN_MULTIPLEX=1` when running the binary directly. When enabled, Potemkin runs the default backend for the workspace plus one extra backend for each subtree whose `potemkin.toml` actually overrides the settings, and routes each file to the backend that owns its subtree. Each backend is the same server binary; it just receives the editor's settings with that scope's `potemkin.toml` layered on top (file wins) as its `initializationOptions`.
