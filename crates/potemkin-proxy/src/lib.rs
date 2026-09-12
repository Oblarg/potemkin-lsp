//! Potemkin proxy core: a plugin pipeline plus the LSP-message transformation
//! logic. The transport (spawning the backend, wiring stdio) lives in `main.rs`;
//! everything here is transport-agnostic and unit-testable.

pub mod config_layer;
pub mod lsp;
pub mod plugin;
pub mod subprocess;

pub mod mux;

use std::borrow::Cow;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::warn;
use serde_json::Value;

use potemkin_plugin_api::{
    plugin_is_interested, Plugin, TextKind, TransformContext, TransformOutput,
};

/// LSP request methods whose *responses* carry type text we rewrite.
const TRACKED_METHODS: &[&str] = &[
    "textDocument/hover",
    "textDocument/inlayHint",
    "inlayHint/resolve",
];

/// Cap on outstanding tracked request ids, to bound memory if some requests
/// never receive a response (e.g. cancelled).
const MAX_PENDING: usize = 4096;

/// Runtime display options, typically sourced from env vars or editor config.
#[derive(Debug, Clone)]
pub struct Config {
    pub verbosity: u8,
    pub unicode: bool,
    /// How to render the "Raw:" section appended below rewritten hovers.
    pub raw: RawConfig,
    /// Default per-plugin timeout for one `initialize`/`transform` round trip.
    /// A plugin can override it via its manifest `timeout_ms`. Guards the editor
    /// stream: a hung/slow plugin is killed and dropped rather than stalling a
    /// hover or inlay-hint response.
    pub plugin_timeout: Duration,
}

/// Built-in default per-plugin timeout when neither the manifest nor
/// `POTEMKIN_PLUGIN_TIMEOUT_MS` specifies one. Generous enough never to trip on
/// real transform work (which is sub-millisecond) or a cold JS/WASM
/// `initialize`, while still bounding a genuinely stuck plugin.
pub const DEFAULT_PLUGIN_TIMEOUT: Duration = Duration::from_millis(5000);

impl Default for Config {
    fn default() -> Self {
        Self {
            verbosity: 0,
            unicode: true,
            raw: RawConfig::default(),
            plugin_timeout: DEFAULT_PLUGIN_TIMEOUT,
        }
    }
}

/// How the proxy renders the raw (pre-transform) type text it appends below a
/// pretty-printed hover.
///
/// This is a property of the wrapped **language server's** hover format — e.g.
/// rust-analyzer emits markdown with ```rust code fences, whereas clangd emits
/// plaintext — so it is configured by the *user per language server* (in the
/// editor) and delivered to this per-server proxy process via `POTEMKIN_RAW_*`.
/// It deliberately does **not** vary per plugin: every plugin under a given
/// server shares the same raw presentation.
#[derive(Debug, Clone)]
pub struct RawConfig {
    /// Whether to append the raw types below the pretty output at all.
    pub enabled: bool,
    /// Header label for the section, e.g. `"Raw:"`. Empty omits the header.
    pub label: String,
    /// If `Some(lang)`, wrap the raw body in a ```lang fenced block (right for
    /// markdown hovers like rust-analyzer's, where `lang` is e.g. `"rust"`).
    /// `None` emits the body as plain text (right for plaintext hovers like
    /// clangd's).
    pub fence: Option<String>,
    /// If true, precede the section with a markdown thematic break (`---`).
    pub separator: bool,
}

impl Default for RawConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            label: "Raw:".to_string(),
            fence: None,
            separator: false,
        }
    }
}

/// The ordered set of plugins plus display config. Cloning is cheap (shared
/// `Arc`s), so it can be handed to each direction of the proxy.
#[derive(Clone)]
pub struct Pipeline {
    plugins: Arc<Vec<Box<dyn Plugin>>>,
    config: Config,
    /// Ids of in-flight hover / inlay-hint requests (editor -> backend). Only
    /// responses to these are candidates for rewriting; every other backend
    /// message is forwarded untouched, so heavy startup traffic (cargo-metadata
    /// progress, diagnostics, semantic tokens) never hits the plugin path.
    pending: Arc<Mutex<HashSet<i64>>>,
}

impl Pipeline {
    pub fn new(plugins: Vec<Box<dyn Plugin>>, config: Config) -> Self {
        Self {
            plugins: Arc::new(plugins),
            config,
            pending: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    pub fn plugin_names(&self) -> Vec<&str> {
        self.plugins.iter().map(|p| p.name()).collect()
    }

    /// Editor -> backend. Requests are forwarded unchanged, but we record the ids
    /// of hover / inlay-hint requests so we know which responses to rewrite.
    pub fn process_outgoing(&self, message: &str) -> String {
        let Some(payload) = lsp::payload(message) else {
            return message.to_string();
        };

        // Cheap prefilter before parsing: only look closer if this could be
        // one of the request methods we track.
        if TRACKED_METHODS.iter().any(|m| payload.contains(m)) {
            if let Ok(v) = serde_json::from_str::<Value>(payload) {
                let id = v.get("id").and_then(Value::as_i64);
                let method = v.get("method").and_then(Value::as_str);
                if let (Some(id), Some(method)) = (id, method) {
                    if TRACKED_METHODS.contains(&method) {
                        let mut pending = self.pending.lock().unwrap();
                        if pending.len() >= MAX_PENDING {
                            pending.clear();
                        }
                        pending.insert(id);
                    }
                }
            }
        }

        message.to_string()
    }

    /// Backend -> editor. Rewrites hover / inlay-hint type text.
    ///
    /// Only responses to tracked hover / inlay-hint requests are considered; all
    /// other messages (notifications, progress, diagnostics, semantic tokens,
    /// and responses to other requests) are forwarded immediately after a cheap
    /// id check — never parsed in full and never sent to a plugin. This keeps the
    /// forwarding stream from stalling during heavy workspace loading.
    ///
    /// Returns the original message untouched whenever nothing matches or
    /// anything goes wrong, so a plugin bug can never break the editor session.
    pub fn process_incoming(&self, message: &str) -> String {
        let Some(json) = lsp::payload(message) else {
            return message.to_string();
        };

        // Is this a response to a hover / inlay-hint request we issued?
        let Some(id) = extract_response_id(json) else {
            return message.to_string();
        };
        {
            let mut pending = self.pending.lock().unwrap();
            if !pending.remove(&id) {
                return message.to_string();
            }
        }

        // It is. Now the (rarer, bounded) work: skip if no plugin cares, else
        // parse and transform.
        let any_interested = self
            .plugins
            .iter()
            .any(|p| plugin_is_interested(p.as_ref(), json));
        if !any_interested {
            return message.to_string();
        }

        let mut msg: Value = match serde_json::from_str(json) {
            Ok(v) => v,
            Err(e) => {
                warn!("failed to parse LSP message: {e}");
                return message.to_string();
            }
        };

        let Some(result) = msg.get_mut("result") else {
            return message.to_string();
        };

        let changed = self.rewrite_result(result);
        if !changed {
            return message.to_string();
        }

        match serde_json::to_string(&msg) {
            Ok(new_json) => lsp::frame(&new_json),
            Err(e) => {
                warn!("failed to re-serialize LSP message: {e}");
                message.to_string()
            }
        }
    }

    /// Dispatch on the shape of `result` to find rewritable text. Returns whether
    /// anything changed.
    fn rewrite_result(&self, result: &mut Value) -> bool {
        // Hover response: { contents: MarkupContent | MarkedString | [MarkedString] }
        if let Some(contents) = result.get_mut("contents") {
            return self.rewrite_hover_contents(contents);
        }

        // Inlay hints: array of { position, label } (or a single hint on resolve).
        if result.is_array() {
            let mut changed = false;
            if let Some(arr) = result.as_array_mut() {
                for hint in arr {
                    changed |= self.rewrite_inlay_hint(hint);
                }
            }
            return changed;
        }
        if result.is_object() && result.get("label").is_some() {
            return self.rewrite_inlay_hint(result);
        }

        false
    }

    fn rewrite_hover_contents(&self, contents: &mut Value) -> bool {
        match contents {
            // MarkupContent { kind, value } or MarkedString { language, value }
            Value::Object(obj) => {
                if let Some(Value::String(s)) = obj.get_mut("value") {
                    return self.rewrite_hover_string(s);
                }
                false
            }
            // Plain MarkedString.
            Value::String(s) => self.rewrite_hover_string(s),
            // [MarkedString | MarkupContent]
            Value::Array(arr) => {
                let mut changed = false;
                for item in arr {
                    changed |= self.rewrite_hover_contents(item);
                }
                changed
            }
            _ => false,
        }
    }

    /// Run one hover string through the plugin chain and, if it changed, append
    /// the user-configured raw section (see [`RawConfig`]).
    fn rewrite_hover_string(&self, s: &mut String) -> bool {
        let original = s.clone();
        let changed = self.apply_in_place(s, TextKind::Hover);
        if changed {
            if let Some(section) = build_raw_section(&original, s, &self.config.raw) {
                s.push_str(&section);
            }
        }
        changed
    }

    fn rewrite_inlay_hint(&self, hint: &mut Value) -> bool {
        // Snapshot the bits a plugin may need to build seeded `textEdits`, before
        // taking any mutable borrow of `hint`.
        let position = hint.get("position").cloned();
        let existing_edits = hint.get("textEdits").cloned();

        // Compute the new label value and any replacement edits, holding only a
        // short immutable borrow of `hint` (so we can mutate it afterwards).
        let (new_label, new_edits): (Option<Value>, Option<Vec<Value>>) = {
            let Some(label) = hint.get("label") else {
                return false;
            };
            match label {
                // InlayHintLabel as a plain string.
                Value::String(s) => {
                    let mut current = s.clone();
                    let (changed, edits) = self.apply_chain(
                        &mut current,
                        TextKind::InlayHint,
                        position.as_ref(),
                        existing_edits.as_ref(),
                    );
                    (changed.then(|| Value::String(current)), edits)
                }
                // InlayHintLabel as [InlayHintLabelPart { value, location?, .. }].
                //
                // rust-analyzer splits a single type across many parts (each named
                // type carries its own go-to-definition `location`, punctuation is
                // its own part). A plugin must see the *whole* type, so concatenate
                // the parts, transform once, and — if changed — re-emit as a single
                // part that keeps the first location for go-to-definition.
                Value::Array(parts) => {
                    let mut full = String::new();
                    let mut first_location: Option<Value> = None;
                    for part in parts.iter() {
                        if let Some(v) = part.get("value").and_then(Value::as_str) {
                            full.push_str(v);
                        }
                        if first_location.is_none() {
                            if let Some(loc) = part.get("location") {
                                first_location = Some(loc.clone());
                            }
                        }
                    }

                    let mut current = full;
                    let (changed, edits) = self.apply_chain(
                        &mut current,
                        TextKind::InlayHint,
                        position.as_ref(),
                        existing_edits.as_ref(),
                    );
                    let new_label = changed.then(|| {
                        let mut type_part = serde_json::Map::new();
                        type_part.insert("value".to_string(), Value::String(current));
                        if let Some(loc) = first_location {
                            type_part.insert("location".to_string(), loc);
                        }
                        Value::Array(vec![Value::Object(type_part)])
                    });
                    (new_label, edits)
                }
                _ => (None, None),
            }
        };

        let mut mutated = false;
        if let Some(label) = new_label {
            hint["label"] = label;
            mutated = true;
        }
        if let Some(edits) = new_edits {
            hint["textEdits"] = Value::Array(edits);
            mutated = true;
        }
        mutated
    }

    /// Run `text` through every interested plugin in order (mutating in place),
    /// threading inlay-hint `position`/`existing_edits` so a plugin can emit
    /// replacement `textEdits`. Returns `(text_changed, replacement_edits)`,
    /// where the last plugin to return edits wins.
    fn apply_chain(
        &self,
        text: &mut String,
        kind: TextKind,
        position: Option<&Value>,
        existing_edits: Option<&Value>,
    ) -> (bool, Option<Vec<Value>>) {
        let original = text.clone();
        let mut current = original.clone();
        let mut edits: Option<Vec<Value>> = None;

        for plugin in self.plugins.iter() {
            if !plugin_is_interested(plugin.as_ref(), &current) {
                continue;
            }
            let ctx = TransformContext {
                kind,
                original: &original,
                verbosity: self.config.verbosity,
                unicode: self.config.unicode,
                position,
                text_edits: existing_edits,
            };
            let TransformOutput {
                text: out_text,
                text_edits: out_edits,
            } = plugin.transform(&current, &ctx);
            if let Cow::Owned(next) = out_text {
                current = next;
            }
            if let Some(e) = out_edits {
                edits = Some(e);
            }
        }

        let changed = current != original;
        if changed {
            *text = current;
        }
        (changed, edits)
    }

    /// Text-only convenience over [`apply_chain`] for spans with no inlay context
    /// (hovers, diagnostics). Returns whether the text changed.
    fn apply_in_place(&self, text: &mut String, kind: TextKind) -> bool {
        self.apply_chain(text, kind, None, None).0
    }
}

/// Build the raw section to append below a rewritten hover, or `None` if there is
/// nothing worth showing.
///
/// The *body* is language-server-agnostic: the original lines that the plugin
/// chain rewrote away — i.e. non-blank lines present in `original` but not in the
/// `transformed` output. This keeps the section to just the raw type(s), not the
/// whole (possibly doc-laden) hover. If no such lines are found (e.g. the change
/// was intra-line), it falls back to the full trimmed original.
///
/// The *presentation* (`label`, `fence`, `separator`) comes entirely from the
/// user's per-server [`RawConfig`]; this function does not inspect the hover's
/// markdown-ness, since the user declares the right rendering for their server.
fn build_raw_section(original: &str, transformed: &str, raw: &RawConfig) -> Option<String> {
    if !raw.enabled {
        return None;
    }

    let changed: Vec<&str> = original
        .lines()
        .filter(|line| {
            let t = line.trim();
            !t.is_empty() && !transformed.contains(t)
        })
        .collect();

    let body = if changed.is_empty() {
        original.trim().to_string()
    } else {
        changed.join("\n")
    };
    if body.trim().is_empty() {
        return None;
    }

    let mut out = String::from("\n\n");
    if raw.separator {
        out.push_str("---\n\n");
    }
    if !raw.label.is_empty() {
        out.push_str(&raw.label);
        out.push_str("\n\n");
    }
    match &raw.fence {
        Some(lang) => {
            out.push_str("```");
            out.push_str(lang);
            out.push('\n');
            out.push_str(&body);
            out.push_str("\n```");
        }
        None => out.push_str(&body),
    }
    Some(out)
}

/// Cheaply extract the top-level integer `id` from a JSON-RPC payload, without a
/// full parse. Returns `None` for notifications (no id) and string ids (which our
/// tracked requests never use). Uses the first `"id":` occurrence, which is the
/// top-level id in practice (it precedes `result`).
fn extract_response_id(payload: &str) -> Option<i64> {
    let pos = payload.find("\"id\":")?;
    let rest = payload[pos + 5..].trim_start();
    let bytes = rest.as_bytes();
    let mut i = 0;
    if bytes.first() == Some(&b'-') {
        i = 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None; // not an integer id (e.g. a string id)
    }
    rest[..i].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A stand-in plugin: pretty-prints any text containing `Quantity`, and for
    /// inlay hints attaches a seeded `textEdit` built from the hint position.
    struct SeedPlugin {
        markers: Vec<Cow<'static, str>>,
    }

    impl Plugin for SeedPlugin {
        fn name(&self) -> &str {
            "seed"
        }
        fn markers(&self) -> &[Cow<'static, str>] {
            &self.markers
        }
        fn transform<'a>(
            &self,
            text: &'a str,
            ctx: &TransformContext<'_>,
        ) -> TransformOutput<'a> {
            if !text.contains("Quantity") {
                return TransformOutput::unchanged(text);
            }
            let pretty = ": Quantity<m, f64>".to_string();
            let text_edits = if ctx.kind == TextKind::InlayHint {
                ctx.position.map(|p| {
                    vec![json!({
                        "range": { "start": p, "end": p },
                        "newText": ": qty!(m)"
                    })]
                })
            } else {
                None
            };
            TransformOutput {
                text: Cow::Owned(pretty),
                text_edits,
            }
        }
    }

    fn seed_pipeline() -> Pipeline {
        let plugin: Box<dyn Plugin> = Box::new(SeedPlugin {
            markers: vec![Cow::Borrowed("Quantity")],
        });
        Pipeline::new(vec![plugin], Config::default())
    }

    #[test]
    fn inlay_resolve_gets_seeded_text_edits() {
        let pipe = seed_pipeline();

        // Track the resolve request id so its response is eligible for rewriting.
        let req = r#"{"jsonrpc":"2.0","id":7,"method":"inlayHint/resolve","params":{}}"#;
        pipe.process_outgoing(&lsp::frame(req));

        // A resolve response: single hint object with a split label + position.
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": {
                "position": { "line": 1, "character": 10 },
                "label": [
                    { "value": ": " },
                    { "value": "Quantity<Unit<Scale<..>>, f64>" }
                ]
            }
        })
        .to_string();

        let out = pipe.process_incoming(&lsp::frame(&resp));
        let payload = lsp::payload(&out).expect("framed payload");
        let v: Value = serde_json::from_str(payload).unwrap();

        // Label was rewritten to a single pretty part.
        assert_eq!(v["result"]["label"][0]["value"], ": Quantity<m, f64>");
        // And a seeded textEdit was attached, using the hint position for range.
        let edits = v["result"]["textEdits"].as_array().expect("textEdits");
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0]["newText"], ": qty!(m)");
        assert_eq!(edits[0]["range"]["start"]["line"], 1);
    }

    #[test]
    fn hover_is_rewritten_without_text_edits() {
        let pipe = seed_pipeline();

        let req = r#"{"jsonrpc":"2.0","id":3,"method":"textDocument/hover","params":{}}"#;
        pipe.process_outgoing(&lsp::frame(req));

        let resp = json!({
            "jsonrpc": "2.0",
            "id": 3,
            "result": { "contents": { "kind": "markdown", "value": "Quantity<Unit<..>>" } }
        })
        .to_string();

        let out = pipe.process_incoming(&lsp::frame(&resp));
        let payload = lsp::payload(&out).unwrap();
        let v: Value = serde_json::from_str(payload).unwrap();

        assert_eq!(v["result"]["contents"]["value"], ": Quantity<m, f64>");
        assert!(v["result"].get("textEdits").is_none());
    }

    fn raw_pipeline(raw: RawConfig) -> Pipeline {
        let plugin: Box<dyn Plugin> = Box::new(SeedPlugin {
            markers: vec![Cow::Borrowed("Quantity")],
        });
        Pipeline::new(
            vec![plugin],
            Config {
                raw,
                ..Config::default()
            },
        )
    }

    fn hover_value(pipe: &Pipeline, value: &str) -> String {
        let req = r#"{"jsonrpc":"2.0","id":1,"method":"textDocument/hover","params":{}}"#;
        pipe.process_outgoing(&lsp::frame(req));
        let resp = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "result": { "contents": { "kind": "markdown", "value": value } }
        })
        .to_string();
        let out = pipe.process_incoming(&lsp::frame(&resp));
        let payload = lsp::payload(&out).unwrap();
        let v: Value = serde_json::from_str(payload).unwrap();
        v["result"]["contents"]["value"].as_str().unwrap().to_string()
    }

    #[test]
    fn raw_disabled_appends_nothing() {
        let pipe = raw_pipeline(RawConfig::default()); // enabled: false
        let out = hover_value(&pipe, "Quantity<Unit<..>>");
        assert_eq!(out, ": Quantity<m, f64>");
        assert!(!out.contains("Raw:"));
    }

    #[test]
    fn raw_markdown_fence_and_separator() {
        let pipe = raw_pipeline(RawConfig {
            enabled: true,
            label: "Raw:".to_string(),
            fence: Some("rust".to_string()),
            separator: true,
        });
        let out = hover_value(&pipe, "Quantity<Unit<..>>");
        // Pretty output first, then a fenced raw section with the original type.
        assert!(out.starts_with(": Quantity<m, f64>"), "pretty first: {out}");
        assert!(out.contains("\n\n---\n\nRaw:\n\n```rust\n"), "fenced header: {out}");
        assert!(out.contains("Quantity<Unit<..>>"), "raw body present: {out}");
        assert!(out.trim_end().ends_with("```"), "fence closed: {out}");
    }

    #[test]
    fn raw_plaintext_no_fence() {
        // clangd-style: plaintext hover, no code fence, no separator.
        let pipe = raw_pipeline(RawConfig {
            enabled: true,
            label: "Raw:".to_string(),
            fence: None,
            separator: false,
        });
        let out = hover_value(&pipe, "Quantity<Unit<..>>");
        assert_eq!(out, ": Quantity<m, f64>\n\nRaw:\n\nQuantity<Unit<..>>");
        assert!(!out.contains("```"), "no fence in plaintext mode: {out}");
    }

    #[test]
    fn raw_shows_only_changed_lines() {
        // Multi-line hover: only the rewritten line should appear under Raw:,
        // not the unchanged surrounding lines.
        let plugin: Box<dyn Plugin> = Box::new(LineSeedPlugin);
        let pipe = Pipeline::new(
            vec![plugin],
            Config {
                raw: RawConfig {
                    enabled: true,
                    label: "Raw:".to_string(),
                    fence: None,
                    separator: false,
                },
                ..Config::default()
            },
        );
        let value = "variable x\n\nType: Quantity<Unit<..>>\nextra: keepme";
        let out = hover_value(&pipe, value);
        assert!(out.contains("Type: Q<m>"), "line rewritten: {out}");
        // Raw body is just the original changed line.
        assert!(out.contains("Raw:\n\nType: Quantity<Unit<..>>"), "raw = changed line: {out}");
        // Unchanged lines are not duplicated into the raw section.
        assert_eq!(out.matches("keepme").count(), 1, "unchanged line not in raw: {out}");
        assert_eq!(out.matches("variable x").count(), 1, "unchanged line not in raw: {out}");
    }

    /// Rewrites only the `Type:` line, leaving the rest of a multi-line hover
    /// intact — used to prove the raw section shows just the changed line.
    struct LineSeedPlugin;
    impl Plugin for LineSeedPlugin {
        fn name(&self) -> &str {
            "line-seed"
        }
        fn markers(&self) -> &[Cow<'static, str>] {
            const M: &[Cow<'static, str>] = &[Cow::Borrowed("Quantity")];
            M
        }
        fn transform<'a>(
            &self,
            text: &'a str,
            _ctx: &TransformContext<'_>,
        ) -> TransformOutput<'a> {
            let replaced = text.replace("Type: Quantity<Unit<..>>", "Type: Q<m>");
            if replaced == text {
                TransformOutput::unchanged(text)
            } else {
                TransformOutput::text(replaced)
            }
        }
    }
}
