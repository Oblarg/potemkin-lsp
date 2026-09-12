# Potemkin — Design Notes

## 1. What Potemkin is

A generic language-server *proxy*. It launches a real language server as a child
process and shuttles LSP traffic between the editor and that server. On the
server→editor path it runs each message through a **plugin pipeline** that
rewrites type text (hovers, inlay hints, diagnostics).

This generalizes the whippyunits `lsp-proxy`, which hard-codes one library's
pretty-printer. Potemkin makes the pretty-printer a plugin, and lets **multiple
plugins compose** on the same message.

## 2. Plugin composition model (current scaffold)

The `Plugin` trait (`crates/potemkin-plugin-api`) is deliberately tiny and
transport-agnostic:

- `markers()` — cheap substrings. If none appear in a raw message payload, the
  proxy skips JSON parsing *and* that plugin. This preserves the whippyunits
  "fast string search before deserialize" optimization, generalized across
  plugins.
- `transform(text, ctx)` — rewrite one span of type text. Returns `Cow` so
  "unchanged" is allocation-free.

The proxy (`Pipeline` in `potemkin-proxy/src/lib.rs`) owns all LSP mechanics:
locating the rewritable strings inside hover contents / inlay-hint labels, then
chaining every interested plugin over each string in **application order** (see
below). Because plugins run in sequence over the same text (each sees the
previous plugin's output; `original` stays fixed), several libraries can each
rewrite the fragment they recognize → **composition falls out for free**.

### Application order

The chain order is computed once at startup (`load_plugins` /`ordered_indices` in
`potemkin-proxy/src/main.rs`), sorting all loaded plugins by, in priority order:

1. **User override** — `POTEMKIN_PLUGIN_ORDER`, a comma-separated list of plugin
   names (earliest first), set from the editor's `potemkin.pluginOrder`. Listed
   plugins run first, in the given order; unlisted plugins follow.
2. **Manifest `order`** — an integer hint the plugin author sets (lower runs
   earlier). This expresses *layering*: e.g. `typed-linear-algebra` (order 10)
   declares it must run *after* `nholthaus-units` (order 0) so the matrix
   pretty-printer sees already-rendered element unit types.
3. **Name** — a deterministic tiebreak, replacing the previous behavior where
   order was whatever `read_dir` returned (unstable across filesystems).

Safety property: any parse/transform failure returns the original message
untouched, so a plugin bug can never break the editor session.

### Per-plugin timeout (fault isolation)

A crashing plugin is already handled (an error just passes the text through), but
a plugin that *hangs* — an infinite loop, a blocked syscall, a pathologically
slow transform — would otherwise stall the whole hover/inlay response, since the
backend→editor pump waits on each plugin round trip. Every plugin call is
therefore bounded by a timeout (`SubprocessRpc` in
`potemkin-proxy/src/subprocess.rs`):

- A dedicated reader thread forwards response lines over a channel; the request
  side waits with `recv_timeout` (a blocking pipe read has no timeout of its
  own). This covers all transports — subprocess, `js`, and `wasm` all funnel
  through `SubprocessRpc`.
- On timeout the proxy **kills the child and marks the transport dead**. The call
  returns an error (→ text passes through unchanged), and every later call
  short-circuits, so one bad plugin simply drops out of the chain for the rest of
  the session instead of repeatedly stalling. A protocol desync (mismatched
  response id) disables the plugin the same way.
- The bound also protects startup: the `initialize` handshake uses the same
  timeout, so a plugin that hangs on init fails to load rather than freezing the
  proxy.

Configuration (most specific wins): a plugin's manifest `timeout_ms`, else the
proxy-wide default from `POTEMKIN_PLUGIN_TIMEOUT_MS` (set from the editor's
`potemkin.pluginTimeoutMs`), else the built-in `DEFAULT_PLUGIN_TIMEOUT` (5 s).
The default is deliberately generous: real transforms are sub-millisecond and
even a cold `js`/`wasm` `initialize` finishes well within it, so the timeout only
ever fires on something genuinely stuck.

### Plugin linking — DECIDED: cross-language, subprocess-first

**Goal (per product direction):** Potemkin operates on generic language-server
concepts; Rust/rust-analyzer is the original problem context, not the ceiling.
Plugin authors should be able to write plugins **in the language of their
choice**. That goal rules out making a Rust ABI (static/dylib) or WASM the *only*
mechanism — WASM in particular still excludes many popular languages
(Python/Ruby/JS aren't first-class WASM-component targets yet).

**Canonical interface = a wire protocol, not a Rust trait.** A plugin is any
process that speaks newline-delimited JSON (JSONL) over stdio. See
`crates/potemkin-plugin-api/src/protocol.rs`:

- Plugins are described by a `PluginManifest` (a small JSON file). Crucially the
  manifest carries the plugin's **markers**, so the proxy applies the "skip
  unless a marker appears" fast path *without launching the plugin* — this keeps
  the hot path cheap and is what makes language-agnosticism affordable.
- Handshake: proxy → `initialize` → plugin replies with capabilities.
- Work: proxy → `transform` (batched items) → plugin returns rewritten strings.
- The Rust `Plugin` trait is now an *internal* abstraction. All transports share
  one `RpcTransport` (`call(method, params) -> result`) and a single
  `ProtocolPlugin` (`potemkin-proxy/src/plugin.rs`) builds the `Plugin` (handshake
  + `transform` marshalling) on top of it, so composition/chaining is identical
  regardless of plugin language *or* transport.

**Transport tiers (same logical interface, all implemented):**
1. **Subprocess** — any language with an stdio runtime. Primary, most flexible.
   `Transport::Subprocess`.
2. **JavaScript** (`Transport::Js`) — a `.js` file run with a Node runtime. The
   proxy resolves Node from `POTEMKIN_NODE` (the editor's bundled Node, supplied
   by the VS Code extension) or `node` on `PATH`. No build step, no separate Node
   install inside VS Code/Cursor. Reuses the subprocess machinery.
3. **WebAssembly** (`Transport::Wasm`) — a single cross-platform `.wasm` that
   runs **in the JavaScript runtime** via the extism JS SDK. A tiny bundled Node
   harness (`runtime/wasm-host/host.js`, esbuild'd to `dist/wasm-host.js` in the
   VSIX) loads the module and speaks the same JSONL protocol; the proxy points at
   it via `POTEMKIN_WASM_HOST`. This deliberately avoids baking a native wasm
   runtime (wasmtime) into the proxy binary — see §6c. Authors compile once with
   any extism PDK (Rust/JS/Go/Python/…) and it runs everywhere Node is present,
   which for the extension means the editor's own runtime.

An in-process Rust registry (`registered_plugins()` in `main.rs`) remains for
first-party/bundled plugins that want zero IPC.

**Performance notes / next steps:**
- **Request-id gating (implemented, important):** only responses to tracked
  `textDocument/hover`, `textDocument/inlayHint`, and `inlayHint/resolve`
  requests are ever parsed or sent to a plugin. `process_outgoing` records those
  request ids; `process_incoming` cheaply extracts the response id and forwards
  anything untracked verbatim. Without this, indexing the whippyunits workspace
  itself (whose source is full of `Quantity`/`Unit`/`Scale`/`Dimension`) made
  nearly every diagnostic / semantic-token message trip the markers, triggering
  blocking plugin IPC on the forwarding hot path — which backpressured
  rust-analyzer until `cargo metadata` / workspace enumeration timed out and
  retried in a loop.
- Markers-in-manifest still gate the (now much rarer) parsed messages: a hover
  with no relevant markers costs one substring scan, no IPC.
- Remaining: the pipeline sends one `transform` per string; batch *all* strings
  in a message into a single round-trip per plugin (the protocol's `items` array
  already supports this) to avoid brief stalls on files with many inlay hints.
  Plugin `transform` also does blocking stdio inside the sync pipeline; with
  id-gating this only happens for actual hover/inlay responses, but moving it off
  the tokio worker (`spawn_blocking`) would remove even those micro-stalls.

## 3. THE decision on the table: VS Code plugin architecture

Requirement: users who don't want to `cargo build` should install a VS Code
extension and get pretty-printing. Constraint: balance **install/distribution
hassle** against **runtime performance**. The proxy core is native Rust, but the
marketplace ships JS/TS — so how does the native code reach the user?

### Option A — Extension bundles prebuilt native binaries (per-platform VSIX)

The extension ships precompiled `potemkin` binaries. On activation it detects the
platform, picks the matching binary, and points each detected server's path
setting (e.g. `rust-analyzer.server.path`, `clangd.path`, …) at it, wrapping the
user's existing servers regardless of language.

- ➕ Full native performance (identical to the build-it-yourself path).
- ➕ Works offline immediately after install.
- ➕ VS Code supports platform-specific `.vsix` targets, so each user downloads
  only their platform's binary (no bloat).
- ➖ Requires CI cross-compilation for {darwin-arm64, darwin-x64, linux-x64,
  linux-arm64, win32-x64}.
- ➖ Plugin set is baked into the shipped binary (unless combined with WASM
  plugins). "Which libraries are prettified" = whatever we compiled in.

### Option B — Extension downloads the binary on first activation

Tiny extension; on activation it fetches the correct platform binary from GitHub
releases (like how the rust-analyzer extension itself bootstraps).

- ➕ Native performance; small published extension.
- ➕ Can update the binary independently of the extension.
- ➖ Needs network on first run; painful behind corporate proxies/air-gapped.
- ➖ Release-hosting + checksum/verification infrastructure to maintain.

### Option C — Pure TypeScript/WASM proxy inside the extension (no native binary)

Implement the proxy in the extension host using the `vscode-languageclient`
middleware hooks (`provideHover`, `provideInlayHints`, etc.), or by piping
rust-analyzer through Node. Library pretty-printers ship as **WASM modules** the
extension calls (the whippyunits pretty-print logic already lives in Rust →
compiles to WASM).

- ➕ Trivial distribution: one cross-platform extension, no binaries, no CI matrix.
- ➕ Plugins as WASM = composable at install time without shipping native code.
- ➖ Rewriting happens in JS on the hot path; the marker fast-path mitigates this,
  but it's slower than native for large responses.
- ➖ Two integration styles (spawn-and-pipe vs. LSP middleware) each have edge
  cases; middleware only sees requests the client makes, not raw server traffic.

### DECIDED: Option A — bundle prebuilt binaries in per-platform VSIX

The extension ships the native `potemkin` binary (per-platform VSIX targets so
each user downloads only their platform's binary) and, on activation, wires it in
as rust-analyzer's server path. Native performance, offline, zero-setup. Cost is
a one-time CI cross-compile matrix ({darwin,linux}×{arm64,x64}, win32-x64).

Because plugins are **subprocess** (see §2), the bundled binary does *not* bake in
the library plugin set: users (or companion extensions) drop plugin manifests
into the plugins dir and any-language plugins compose at runtime. This resolves
the earlier coupling worry — the native-binary distribution and the cross-language
plugin model are now independent.

## 4. First plugin: whippyunits (ported)

`whippyunits/potemkin-plugin/` is the whippyunits `lsp-proxy` ported to the
Potemkin plugin model. It's a subprocess plugin (JSONL over stdio) that **reuses
the exact formatting code** from `whippyunits-lsp-proxy` (hover trait-signature
simplification, `Quantity<…>` / bare `Unit<Scale…>` rewriting, inlay-hint
exponent pruning), so output matches the original proxy. This validates the
"library ships its own plugin, in the language it likes" model end-to-end:
verbose `Quantity<Unit<Scale<…>>, …>` renders as `Quantity<m, f64>` through the
full proxy → plugin stack.

A pipeline change was required to support it (and any real type printer):
inlay-hint labels are **concatenated** across rust-analyzer's split parts before
being handed to plugins, then re-emitted as a single located part. Without this,
plugins would only ever see fragments (`Quantity`, `<`, `Unit`, …).

### Structured output: `qty!` seeded text edits (protocol v2, implemented)

The original inlay-hint processor also injected a `qty!(mm)` completion
(`textEdits`) when you accept a plain-`Quantity` hint — inserting `: qty!(mm)`
instead of the verbose type. That's *structured* output beyond string-in/
string-out, so the plugin protocol was extended (**PROTOCOL_VERSION = 2**) rather
than special-cased:

- `TransformItem` may now carry inlay-hint `position` and existing `text_edits`
  (skipped on the wire for hovers), giving a plugin what it needs to build an
  edit range.
- `TransformResult` items are an **untagged** union: a bare string (v1 form, what
  `demo.py` and any simple plugin still emit) *or* a rich
  `{ text, text_edits }` object. A plugin returning `text_edits: Some(..)`
  **replaces** the hint's edits; `None`/absent leaves them untouched.
- Internally, `Plugin::transform` returns `TransformOutput { text, text_edits }`,
  and the pipeline's `apply_chain` threads `position`/existing edits in and
  aggregates edits out (last plugin to emit wins). `rewrite_inlay_hint` applies
  the new label *and* sets `hint["textEdits"]` in one pass. All of this stays on
  the id-gated inlay/hover path, so it never touches unrelated traffic.

The whippyunits plugin reuses `InlayHintProcessor::seeded_text_edits` (extracted
as a standalone entry point) to emit `: qty!(mm)` / `: qty!(mm, i32)` for plain
quantities only; composites (`MixedUnitMatrix<…>`, bare `Unit`) keep the server's
own edits. Both single-round-trip: the label rewrite and the seeded edit come
back from one `transform` call.

## 5. Generic-over-language-server wiring (implemented)

rust-analyzer is not special. VS Code has **no universal, supported hook** to
inject a proxy into an arbitrary third-party language client, so "generic"
means a data-driven approach rather than one magic API:

- **Server registry** (`ServerDef` in `editors/vscode/src/extension.ts`): built-in
  entries for rust-analyzer / clangd / gopls, plus user-defined entries via the
  `potemkin.servers` setting. Each entry declares the config key that overrides
  the server binary (scalar like `clangd.path`, or an object field like
  `go.alternateTools.gopls`), the restart command, and the default server
  command.
- **Launcher shim**: for each wrapped server, the extension writes a tiny script
  (in global storage) that `export POTEMKIN_SERVER=<real server>` (+ `POTEMKIN_*`
  config) and `exec`s the bundled binary, then points the server's path-setting
  at that shim. This sidesteps the fact that most extensions have no
  env-passthrough setting — the shim carries the env itself.
- **`potemkin --version` passthrough**: server binaries are often probed with
  `--version`; the proxy forwards that to the real backend instead of hanging.
- **Auto-detect + restore**: on activation it wraps whichever registered servers
  are installed (`vscode.extensions.getExtension`), saves the pristine original
  setting, and restores it on disable. A stale-Potemkin guard makes re-install /
  upgrade self-healing (never wraps its own binary).

Trade-off accepted: this works *with* each ecosystem's native extension (keeping
all their features), at the cost of needing a small registry entry per server.
Servers whose extension exposes no path override simply can't be wrapped — that's
an editor limitation, surfaced honestly rather than worked around with fragile
PATH shims.

### Consequences / TODO for the VS Code extension

- CI: cross-compile `potemkin` for all targets; attach to per-platform VSIX.
- Graceful fallback: if the binary is missing/incompatible, leave the user's
  server untouched so the editor never breaks.

## 6. Binary distribution — how consumers get the binaries (DECIDED)

Two distinct binaries, two answers.

### 6a. The Potemkin proxy itself → bundled in the VSIX

Per §3 (Option A): the Potemkin extension ships the native `potemkin` binary in
per-platform VSIX targets. Installing the extension *is* installing the proxy —
offline, auto-updating, no download step. Outstanding work is only the CI
cross-compile matrix.

### 6b. Library plugins → companion extensions (primary), via a registration API

Potemkin must **not** bundle every library's plugin (doesn't scale, recouples
the core to libraries). The VS Code-native answer is **one small companion
extension per library**, distributed through the marketplace (one-click install,
auto-update, per-platform binaries, discoverability, signing — all for free).

**Registration API.** Potemkin's extension returns a `PotemkinApi` from
`activate()` (`editors/vscode/src/extension.ts`):

```ts
const potemkin = vscode.extensions.getExtension("potemkin-lsp.potemkin");
const api = await potemkin.activate();       // PotemkinApi
await api.registerPlugin({ name, command, markers });
```

`registerPlugin` writes a `PluginManifest` into an **extension-managed plugins
dir** (`<globalStorage>/plugins`) and schedules a **debounced** restart of
wrapped servers so the proxy reloads. It's idempotent per `name` (safe to call on
every activation) and no-ops if the manifest is unchanged (avoids needless
restarts at startup). `unregisterPlugin(name)` removes it.

**Why an API rather than "companion drops a manifest file":** Potemkin owns the
dir location and lifecycle, the companion never hard-codes paths, and Potemkin
can debounce/refresh/restart centrally.

**Plugin search path.** The proxy now treats `POTEMKIN_PLUGINS_DIR` as an
OS-style path list (see `plugin_dirs()` in `main.rs`). The launcher shim points
it at `[userPluginsDir?, managedDir]`, and the binary always appends its own
default (`~/.config/potemkin/plugins`). Earlier dirs win on name conflicts, and
duplicate plugin *names* are de-duplicated (first wins) so a companion-registered
plugin cleanly shadows a hand-installed / `cargo install`ed one.

**Language scoping (important).** Each wrapped server runs its *own* Potemkin
process (one launcher shim per server), and previously every process loaded
*every* plugin — so the Rust-only whippyunits plugin was spawned even under the
clangd/gopls processes. Now:

- A `PluginManifest` (and the `registerPlugin` API) carries `languages:
  string[]` — the LSP `languageId`s it applies to (`["rust"]`). Empty =
  language-agnostic.
- Each `ServerDef` declares its `languages` (rust-analyzer→`rust`,
  clangd→`c/cpp/objective-c/...`, gopls→`go/...`); the launcher forwards them as
  `POTEMKIN_LANGUAGES`.
- `discover_subprocess_plugins` skips a plugin whose declared languages don't
  intersect `POTEMKIN_LANGUAGES` **before spawning it** (`languages_match` in
  `main.rs`) — so a mismatched plugin costs zero processes, not just a skipped
  transform. When `POTEMKIN_LANGUAGES` is unset (binary run directly, no
  launcher) filtering is disabled and all plugins load, preserving the
  build-it-yourself path.
- **Server scoping (finer grain).** A manifest may also declare
  `language_servers: string[]` (e.g. `["rust-analyzer"]`) for plugins tuned to
  one server's exact output format. The launcher forwards the wrapped server's id
  as `POTEMKIN_SERVER_ID`, and `server_matches` (in `main.rs`) skips the plugin
  before spawning on a mismatch. Empty = any server for the matching language(s);
  unknown id (no launcher) is permissive. whippyunits uses this to scope itself
  to `rust-analyzer` specifically, since its formatter parses rust-analyzer's
  hover/inlay text.
- Ordering note: name de-duplication happens *before* the language check, so the
  highest-priority manifest for a name defines that plugin's language scope
  entirely (a lower-priority agnostic manifest can't re-introduce it under a
  non-matching server).

**Fallbacks (kept):**
- Power users: `cargo install <lib>-potemkin-plugin` (or npm/pip for other
  languages) + a manifest in the default dir — still discovered.
- Hand-maintained manifests via the `potemkin.pluginsDir` setting.

**Reference implementation.** `whippyunits/potemkin-plugin/vscode/` is the
flagship companion: it `extensionDependencies` on `potemkin-lsp.potemkin` (so
installing it pulls in Potemkin), bundles its per-platform plugin binary under
`bin/<platform>/`, and on activation calls `api.registerPlugin({ name:
"whippyunits", command: <bundled bin>, markers: [...] })`.

**Why decentralized distribution is safe here:** plugins talk to Potemkin over
the versioned **JSONL wire protocol**, not a Rust ABI. The plugin binary and the
Potemkin binary are built and updated independently — no lockstep, no ABI
matching — which is exactly what makes per-library companion extensions viable.

### 6c. WASM transport collapses the platform matrix (IMPLEMENTED)

The sleeper argument for `Transport::Wasm` (see §2) is *distribution*, not just
sandboxing: a `.wasm` plugin is **one cross-platform artifact**, so a companion
extension bundles a single file (no per-library native CI matrix) and any future
"download a plugin" flow becomes trivial. It's an *additional* transport (some
languages can't target WASM yet), not a replacement for subprocess.

**Where WASM runs — DECIDED: the JS runtime, not native wasmtime.** Rather than
embed a native wasm runtime in the proxy (which would bake wasmtime into every
cross-compiled binary and inflate it), Potemkin runs `.wasm` plugins in a Node
runtime via the [extism JS SDK](https://github.com/extism/js-sdk), which executes
the *same* `.wasm` plugins with no native addon (it uses the JS runtime's own
`WebAssembly` + `node:wasi`). Mechanics mirror the JS transport:

- `runtime/wasm-host/host.js` is a ~60-line harness: it `createPlugin(path, {
  useWasi: true })`, then for each JSONL request calls the export named by the
  method and returns its JSON. esbuild bundles it (with the extism runtime
  inlined) into a single `dist/wasm-host.js` — ~116 KB, no `node_modules` — that
  ships in the VSIX.
- The extension sets `POTEMKIN_NODE` (the editor's bundled Node) and
  `POTEMKIN_WASM_HOST` (the bundled harness) in the launcher. The proxy spawns
  `<node> <host.js> <plugin.wasm>` as a normal JSONL subprocess.

Net effect: the proxy binary stays lean and trivially cross-compilable (no
wasmtime), and a WASM plugin authored once runs everywhere Node is available —
for the extension, **the editor's own runtime**. The trade-off (chosen
deliberately) is that running a `.wasm` plugin requires a Node runtime to be
present; inside VS Code/Cursor that's always true, and standalone CLI users who
want wasm point `POTEMKIN_WASM_HOST` at their own harness copy (or use the
subprocess/JS transports).

### Not chosen (for now)
- **Potemkin-managed download/registry UX** ("Browse plugins" fetching binaries
  from releases): centralizes UX but makes us responsible for third-party binary
  delivery, verification, and network/air-gap edge cases. Reasonable later
  add-on, not the foundation.

## 7. Per-directory server config — DECIDED: opt-in multiplexer

**Problem.** Provide *per-directory* language-server configuration for servers
that can't do it themselves. rust-analyzer is the motivating case: it folds all
client config into "the client settings" as one unit and runs a single session,
so VS Code's per-folder `settings.json` / `scopeUri` machinery doesn't translate
into per-crate behavior. Its on-disk `rust-analyzer.toml` is meant to close this
gap but is still incomplete/"rather broken" as of writing.

**What we won't do.** Potemkin is a *transparent* proxy, so (a) config files a
server reads itself (`rust-analyzer.toml`, `.clangd`) already work untouched —
duplicating them would double-apply; and (b) we deliberately **do not rewrite the
live `workspace/configuration` protocol** — hijacking a server's ongoing config
pulls would make the proxy an opaque man-in-the-middle. So this is an
*orthogonal* layer, injected only into **initial settings**.

**Config file (`config_layer.rs`).** A generic `potemkin.toml`, discovered by
walking up the tree (nearest-wins), with one table per server id:

```toml
[servers.rust-analyzer]      # matched against POTEMKIN_SERVER_ID
checkOnSave = false
cargo.features = ["ci"]       # dotted keys nest -> { cargo: { features: [..] } }
```

The table contents mirror the server's own config namespace, so they inject
verbatim as `initializationOptions`. Merge is a deep merge with the **file
winning** for keys it names; sibling keys the editor sent pass through.

**Why a multiplexer (`mux.rs`), gated behind `POTEMKIN_MULTIPLEX`.** A single
session is workspace-wide, so genuinely different per-directory settings require
**different processes**. Potemkin runs a **default backend** for the workspace
plus **one extra backend per subtree whose `potemkin.toml` effective settings
actually differ** (the equality check keeps the process/indexing count minimal),
and routes each document to the backend owning its subtree. All backends are the
same binary, so:

- **Routing** — `textDocument.uri`-bearing traffic → owning backend; document-less
  requests (`workspace/*`, resolves) → default; document-less notifications
  broadcast.
- **Id remapping** — every editor↔backend request is re-issued under a
  Potemkin-assigned id (two maps: editor-reqs, server-reqs) so concurrent
  backends never collide; responses map back into the editor's id space, where
  the plugin `Pipeline` still runs.
- **Handshake** — `initialize` is captured and replayed to each backend as it
  lazily spawns (with that scope's settings); the *primary* backend's result is
  the one returned to the editor (identical binary ⇒ identical capabilities).
  Potemkin drives `initialized` per backend and queues traffic until each is
  ready.

**Gating.** Off by default: the common path stays a fully transparent
single-backend proxy (`run_passthrough`), and `potemkin.toml` is ignored. The VS
Code `potemkin.multiplex` setting flips `POTEMKIN_MULTIPLEX` in the launcher.
Running N rust-analyzers over a nested layout means N indexers, which is the cost
of doing what a single session structurally can't — hence last-resort.

**Known v1 limits.** Document-less *resolve* requests and workspace-wide requests
go to the default backend (may be wrong for override subtrees); `$/cancelRequest`
is broadcast rather than routed; backends over a nested tree each index their
view. Scope selection (`resolve_scope`) is unit-tested, and an end-to-end test
(`tests/mux_e2e.rs`) drives the real binary against a mock backend
(`src/bin/mock_lsp.rs`): it verifies that a root file and an override-subtree
file are routed to distinct backends handshaked with their own `potemkin.toml`
config, that the primary `initialize` result is relayed, and that ids map back
into the editor's space. It also covers the **server→editor** direction: each
backend fires a `workspace/configuration` request on `definition`, and the test
(acting as the editor) answers it; getting back `"<scope>:<reply>"` proves the
relay re-issues the request toward the editor and routes the reply back to the
*asking* backend rather than a sibling.
