//! The **cross-language plugin protocol**.
//!
//! Potemkin's goal is broader than Rust/rust-analyzer: it operates on generic
//! language-server concepts, so plugin authors should be able to write plugins
//! in the language of their choice. To make that possible the canonical plugin
//! interface is a small **wire protocol**, not a Rust trait.
//!
//! ## Transport
//!
//! A subprocess plugin communicates over stdio using **newline-delimited JSON**
//! (JSON Lines): one JSON object per line, request then response, matched by
//! `id`. JSONL is trivial to implement in any language (no `Content-Length`
//! framing needed, because serialized JSON never contains a literal newline).
//!
//! ## Handshake and fast path
//!
//! A plugin is described by a [`PluginManifest`] (a small JSON file the proxy
//! discovers at startup). The manifest carries the plugin's **markers** so the
//! proxy can apply the "skip unless a marker appears" fast path *without ever
//! launching the plugin* — critical for both performance and language-agnosticism.
//!
//! On startup the proxy sends [`InitializeParams`]; the plugin replies with
//! [`InitializeResult`]. Thereafter the proxy sends batched [`TransformParams`]
//! and expects [`TransformResult`]. A `shutdown` request ends the session.

use serde::{Deserialize, Serialize};

use crate::TextKind;

/// Bump when the wire format changes incompatibly.
///
/// v2: `transform` results may be *rich* objects (`{ text, text_edits }`) in
/// addition to bare strings, and `TransformItem` may carry inlay-hint
/// `position`/`text_edits` context. Both extensions are backward compatible: a
/// v1 plugin that only reads `text`/`original` and returns strings still works.
pub const PROTOCOL_VERSION: u32 = 2;

/// A JSON-RPC-ish request envelope sent proxy -> plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
}

/// A response envelope sent plugin -> proxy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `method: "initialize"` params.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    pub protocol_version: u32,
    pub verbosity: u8,
    pub unicode: bool,
}

/// `method: "initialize"` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub name: String,
    /// Markers the plugin actually honors. Should match the manifest; the proxy
    /// may log a warning on mismatch.
    #[serde(default)]
    pub markers: Vec<String>,
}

/// One string to rewrite, plus the pristine original for context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformItem {
    /// The current text (may reflect edits by earlier plugins in the chain).
    pub text: String,
    /// The original text before any plugin ran.
    pub original: String,
    /// Inlay-hint only: the hint's LSP `position`. Lets a plugin build a
    /// `textEdit` range (e.g. to seed a macro completion). Absent for hovers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<serde_json::Value>,
    /// Inlay-hint only: the hint's existing LSP `textEdits`, if any. A plugin may
    /// reuse their range or replace them (see [`TransformResultItem::text_edits`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_edits: Option<serde_json::Value>,
}

/// `method: "transform"` params. Items are batched: one round-trip per message
/// per plugin rather than one per string.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformParams {
    pub kind: TextKind,
    pub verbosity: u8,
    pub unicode: bool,
    pub items: Vec<TransformItem>,
}

/// One transformed item. Either a bare rewritten string (the common case, and
/// what v1 plugins emit) or a rich object carrying structured side-effects.
///
/// Serialized **untagged**: a JSON string deserializes to [`Self::Text`], a JSON
/// object to [`Self::Rich`]. This keeps trivial plugins (any language) able to
/// just return strings while richer plugins can attach `text_edits`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum TransformResultItem {
    /// Just the rewritten text.
    Text(String),
    /// Rewritten text plus optional structured output.
    Rich {
        text: String,
        /// Inlay-hint only: replacement LSP `textEdits`. `None` leaves the hint's
        /// existing edits untouched; `Some(..)` replaces them entirely (e.g. to
        /// seed a `qty!(…)` macro instead of inserting the verbose type).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text_edits: Option<Vec<serde_json::Value>>,
    },
}

impl TransformResultItem {
    /// The rewritten text, regardless of variant.
    pub fn text(&self) -> &str {
        match self {
            TransformResultItem::Text(t) => t,
            TransformResultItem::Rich { text, .. } => text,
        }
    }

    /// Consume into `(text, text_edits)`.
    pub fn into_parts(self) -> (String, Option<Vec<serde_json::Value>>) {
        match self {
            TransformResultItem::Text(t) => (t, None),
            TransformResultItem::Rich { text, text_edits } => (text, text_edits),
        }
    }
}

/// `method: "transform"` result. `items[i]` is the rewrite of the request's
/// `items[i]`; a plugin returns the input unchanged for spans it doesn't touch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransformResult {
    pub items: Vec<TransformResultItem>,
}

/// How the proxy launches/loads a plugin. Declared in a [`PluginManifest`].
///
/// All transports share the same logical contract (`initialize` + `transform`
/// over the [`InitializeParams`]/[`TransformParams`] shapes). They differ only in
/// *how* the proxy reaches the plugin code:
///
/// - [`Transport::Subprocess`] and [`Transport::Js`] speak JSONL over stdio: one
///   request object per line in, one response object per line out.
/// - [`Transport::Wasm`] calls exported functions in-process, passing the params
///   JSON in and receiving the result JSON out (no envelope needed).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Transport {
    /// Any language with an stdio-capable runtime: the proxy spawns `command`
    /// and speaks JSONL over stdio.
    Subprocess {
        command: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// A JavaScript file run with a Node runtime. The proxy resolves Node from
    /// `POTEMKIN_NODE` (the editor's bundled Node, supplied by the VS Code
    /// extension) or `node` on `PATH`, then runs it as a JSONL subprocess. No
    /// build step and no separate Node install needed inside VS Code/Cursor.
    Js {
        path: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// A single cross-platform WebAssembly plugin, run in a Node runtime via the
    /// [extism JS SDK](https://github.com/extism/js-sdk) — the same Node the JS
    /// transport uses. The proxy spawns a small harness
    /// (`POTEMKIN_WASM_HOST`, shipped by the VS Code extension) that loads the
    /// module and speaks the same JSONL protocol; no native wasm runtime is baked
    /// into the proxy. The module exports `initialize` and `transform`, taking the
    /// params JSON as bytes and returning the result JSON as bytes. Authors write
    /// plugins in any language with an extism PDK (Rust, JS, Go, Python, …).
    Wasm { path: String },
}

/// A plugin's manifest: a small JSON file the proxy discovers at startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Stable identifier, e.g. `"whippyunits"`.
    pub name: String,
    /// Cheap substrings gating the fast path. If none appear in a message, the
    /// plugin is never invoked. Lives here so the proxy needn't launch the
    /// plugin to learn them.
    #[serde(default)]
    pub markers: Vec<String>,
    /// LSP `languageId`s this plugin applies to (e.g. `["rust"]`). Empty means
    /// **language-agnostic** — the plugin applies to any wrapped server. The
    /// proxy compares this against the language of the server it is wrapping (see
    /// `POTEMKIN_LANGUAGES`) and skips loading the plugin entirely on a mismatch,
    /// so a Rust-only plugin is never even spawned under a C++ or Go server.
    #[serde(default)]
    pub languages: Vec<String>,
    /// Specific language-*server* ids this plugin supports (e.g.
    /// `["rust-analyzer"]`). Finer-grained than [`languages`]: use it when a
    /// plugin only handles the output format of a particular server (many type
    /// pretty-printers are written against one server's exact hover/inlay text).
    /// Empty means **any server** for the matching language(s). The proxy matches
    /// this against `POTEMKIN_SERVER_ID` and skips the plugin before spawning on a
    /// mismatch.
    #[serde(default)]
    pub language_servers: Vec<String>,
    /// Application-order hint for the transform chain. When several plugins
    /// rewrite the same text they run in sequence, each seeing the previous
    /// plugin's output; **lower `order` runs earlier**. Authors set this to
    /// express *layering* — e.g. a matrix pretty-printer that needs the element
    /// unit types rendered first declares a higher `order` than the units plugin.
    /// Defaults to `0`. Ties break by plugin name (deterministic). A user can
    /// override ordering entirely via `POTEMKIN_PLUGIN_ORDER` / the editor's
    /// `potemkin.pluginOrder`, which takes precedence over this field.
    #[serde(default)]
    pub order: i32,
    /// Per-plugin timeout override, in milliseconds, for a single
    /// `initialize`/`transform` round trip. When a call exceeds it, the proxy
    /// kills the plugin process and disables it for the rest of the session (its
    /// text is thereafter passed through untouched), so a hung or pathologically
    /// slow plugin can never stall a hover/inlay response. `None` uses the
    /// proxy-wide default (`POTEMKIN_PLUGIN_TIMEOUT_MS`, else a built-in
    /// default). Applies to the subprocess, js, and wasm transports alike.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    /// How to run the plugin.
    pub transport: Transport,
}
