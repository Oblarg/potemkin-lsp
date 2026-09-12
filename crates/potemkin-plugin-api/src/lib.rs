//! # Potemkin Plugin API
//!
//! Potemkin is a generic language-server proxy: it sits between an editor and a
//! real language server (e.g. `rust-analyzer`), and rewrites the *type text* that
//! flows back to the editor so that library-specific types can be pretty-printed.
//!
//! A **plugin** teaches the proxy how one library's types should be displayed.
//! Multiple plugins compose: every rewritable string is run through each
//! interested plugin in turn, so several libraries can pretty-print their types
//! in the same hover/inlay-hint at once.
//!
//! This crate defines only the *interface* a plugin must implement. It is
//! deliberately free of any transport/LSP details so that the same trait can be
//! satisfied by any language with stdio.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod protocol;

/// Where a piece of text came from in the LSP stream.
///
/// Plugins can use this to apply different rules to, say, a terse inlay hint
/// versus a full hover tooltip.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextKind {
    /// A hover tooltip body (usually markdown, often containing a ```rust block).
    Hover,
    /// An inlay-hint label (short, e.g. a type annotation next to a `let`).
    InlayHint,
    /// A diagnostic message body.
    Diagnostic,
    /// Any other rewritable string the proxy chooses to route through plugins.
    Other,
}

/// Context handed to a plugin for a single rewrite.
#[derive(Debug, Clone)]
pub struct TransformContext<'a> {
    /// What kind of text is being transformed.
    pub kind: TextKind,
    /// The original, un-transformed text for this span (before *any* plugin ran).
    ///
    /// `text` passed to [`Plugin::transform`] may already reflect edits from
    /// earlier plugins in the chain; `original` never changes within a chain.
    pub original: &'a str,
    /// Verbosity requested by the user (0 = terse, higher = more detail).
    pub verbosity: u8,
    /// Whether unicode symbols are allowed in output.
    pub unicode: bool,
    /// Inlay-hint only: the hint's LSP `position`, so a plugin can build a
    /// `textEdit` range (e.g. seed a macro completion). `None` for hovers.
    pub position: Option<&'a Value>,
    /// Inlay-hint only: the hint's existing LSP `textEdits`, if any.
    pub text_edits: Option<&'a Value>,
}

/// The result of a single [`Plugin::transform`]: the (possibly rewritten) text,
/// plus optional structured side-effects for the LSP item the text came from.
#[derive(Debug, Clone)]
pub struct TransformOutput<'a> {
    /// The rewritten text. `Cow::Borrowed(text)` means "no change".
    pub text: Cow<'a, str>,
    /// Inlay-hint only: replacement `textEdits` (raw LSP values). `None` leaves
    /// the hint's existing edits untouched; `Some(..)` replaces them.
    pub text_edits: Option<Vec<Value>>,
}

impl<'a> TransformOutput<'a> {
    /// No change: borrow the input and attach no edits.
    pub fn unchanged(text: &'a str) -> Self {
        Self {
            text: Cow::Borrowed(text),
            text_edits: None,
        }
    }

    /// Just a rewritten string, no structured edits.
    pub fn text(text: impl Into<Cow<'a, str>>) -> Self {
        Self {
            text: text.into(),
            text_edits: None,
        }
    }
}

/// A composable type pretty-printer.
///
/// The contract is intentionally small: a plugin advertises cheap substring
/// *markers*, and implements a single `transform` over a string. The proxy owns
/// all LSP mechanics (framing, locating hover/inlay-hint text, chaining plugins).
pub trait Plugin: Send + Sync {
    /// Stable identifier, e.g. `"whippyunits"`. Used for logging and config.
    fn name(&self) -> &str;

    /// Cheap substring markers that indicate this plugin *might* be interested in
    /// a message. If none of a plugin's markers appear in a raw message payload,
    /// the proxy skips both JSON parsing and this plugin entirely.
    ///
    /// `Cow<'static, str>` lets in-process plugins use `&'static` literals while
    /// subprocess/WASM plugins supply markers owned from their manifest.
    ///
    /// Return an empty slice to always be considered (discouraged: it forces the
    /// proxy to parse every message).
    fn markers(&self) -> &[Cow<'static, str>];

    /// Rewrite one span of text, optionally attaching structured side-effects
    /// (e.g. replacement inlay-hint `textEdits`). Return
    /// [`TransformOutput::unchanged`] to indicate "no change".
    ///
    /// Implementations must be pure and reasonably fast: this runs on the hot
    /// path of every relevant LSP response.
    fn transform<'a>(&self, text: &'a str, ctx: &TransformContext<'_>) -> TransformOutput<'a>;
}

/// Convenience: does any marker for this plugin appear in `payload`?
pub fn plugin_is_interested(plugin: &dyn Plugin, payload: &str) -> bool {
    let markers = plugin.markers();
    markers.is_empty() || markers.iter().any(|m| payload.contains(m.as_ref()))
}
