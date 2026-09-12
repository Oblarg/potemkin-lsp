//! Transport-agnostic plugin adapter.
//!
//! Every plugin — whether a subprocess, a JavaScript file, or a WebAssembly
//! module — implements the same tiny RPC contract: a single `call(method,
//! params) -> result`. [`RpcTransport`] captures that, and [`ProtocolPlugin`]
//! builds the higher-level [`Plugin`] (handshake + `transform` marshalling) on
//! top of any transport, so the protocol logic lives in exactly one place.

use std::borrow::Cow;
use std::time::Duration;

use anyhow::{anyhow, Result};
use log::warn;
use serde_json::Value;

use potemkin_plugin_api::protocol::{
    InitializeParams, InitializeResult, PluginManifest, TransformItem, TransformParams,
    TransformResult, Transport, PROTOCOL_VERSION,
};
use potemkin_plugin_api::{Plugin, TransformContext, TransformOutput};

use crate::subprocess::SubprocessRpc;

/// One JSON request/response round trip to a plugin, returning the method's
/// `result` payload. Implementations encapsulate the transport (stdio framing,
/// wasm memory, …); everything protocol-level lives in [`ProtocolPlugin`].
pub trait RpcTransport: Send + Sync {
    fn call(&self, method: &str, params: Value) -> Result<Value>;
}

/// A [`Plugin`] implemented over any [`RpcTransport`].
pub struct ProtocolPlugin {
    name: String,
    markers: Vec<Cow<'static, str>>,
    rpc: Box<dyn RpcTransport>,
}

impl ProtocolPlugin {
    /// Build the transport described by `manifest`, perform the `initialize`
    /// handshake, and return the ready plugin.
    ///
    /// `default_timeout` bounds each plugin round trip (including this
    /// `initialize`); the manifest's `timeout_ms` overrides it per plugin.
    pub fn spawn(
        manifest: &PluginManifest,
        verbosity: u8,
        unicode: bool,
        default_timeout: Duration,
    ) -> Result<Self> {
        let timeout = manifest
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(default_timeout);
        let rpc = build_transport(manifest, timeout)?;
        Self::with_transport(manifest, rpc, verbosity, unicode)
    }

    /// Wrap an already-constructed transport (used by tests and by [`spawn`]).
    pub fn with_transport(
        manifest: &PluginManifest,
        rpc: Box<dyn RpcTransport>,
        verbosity: u8,
        unicode: bool,
    ) -> Result<Self> {
        let init_params = serde_json::to_value(InitializeParams {
            protocol_version: PROTOCOL_VERSION,
            verbosity,
            unicode,
        })?;
        let init: InitializeResult = serde_json::from_value(rpc.call("initialize", init_params)?)?;
        if init.name != manifest.name {
            warn!(
                "plugin manifest name '{}' does not match reported name '{}'",
                manifest.name, init.name
            );
        }

        Ok(Self {
            name: manifest.name.clone(),
            markers: manifest.markers.iter().cloned().map(Cow::Owned).collect(),
            rpc,
        })
    }
}

impl Plugin for ProtocolPlugin {
    fn name(&self) -> &str {
        &self.name
    }

    fn markers(&self) -> &[Cow<'static, str>] {
        &self.markers
    }

    fn transform<'a>(&self, text: &'a str, ctx: &TransformContext<'_>) -> TransformOutput<'a> {
        let result = (|| -> Result<TransformResult> {
            let params = serde_json::to_value(TransformParams {
                kind: ctx.kind,
                verbosity: ctx.verbosity,
                unicode: ctx.unicode,
                items: vec![TransformItem {
                    text: text.to_string(),
                    original: ctx.original.to_string(),
                    position: ctx.position.cloned(),
                    text_edits: ctx.text_edits.cloned(),
                }],
            })?;
            Ok(serde_json::from_value(self.rpc.call("transform", params)?)?)
        })();

        match result {
            Ok(mut res) if !res.items.is_empty() => {
                let (out_text, text_edits) = res.items.swap_remove(0).into_parts();
                let text = if out_text == text {
                    Cow::Borrowed(text)
                } else {
                    Cow::Owned(out_text)
                };
                TransformOutput { text, text_edits }
            }
            Ok(_) => TransformOutput::unchanged(text),
            Err(e) => {
                warn!("plugin '{}' transform failed: {e}", self.name);
                TransformOutput::unchanged(text)
            }
        }
    }
}

/// Construct the transport for a manifest, resolving JS/WASM specifics.
/// `timeout` bounds each round trip through the resulting transport.
fn build_transport(manifest: &PluginManifest, timeout: Duration) -> Result<Box<dyn RpcTransport>> {
    match &manifest.transport {
        Transport::Subprocess { command, args } => {
            Ok(Box::new(SubprocessRpc::spawn(command, args, &[], timeout)?))
        }
        Transport::Js { path, args } => {
            // Run the .js directly with a Node runtime.
            let node = resolve_node();
            let mut full = Vec::with_capacity(args.len() + 1);
            full.push(path.clone());
            full.extend(args.iter().cloned());
            Ok(Box::new(spawn_node(&node, &full, timeout)?))
        }
        Transport::Wasm { path } => {
            // WASM runs in the JS runtime too: a small Node harness loads the
            // module with the extism JS SDK and speaks the same JSONL protocol.
            // The same `.wasm` therefore runs everywhere Node is available
            // (notably the editor's bundled Node), with no wasmtime baked into
            // this binary. The harness path comes from the VS Code extension via
            // `POTEMKIN_WASM_HOST`; standalone users point it at their own copy.
            let node = resolve_node();
            let host = std::env::var("POTEMKIN_WASM_HOST")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "plugin '{}' uses the wasm transport, but POTEMKIN_WASM_HOST \
                         is not set (the VS Code extension sets this; for standalone \
                         use, point it at a Node harness bundling @extism/extism)",
                        manifest.name
                    )
                })?;
            Ok(Box::new(spawn_node(&node, &[host, path.clone()], timeout)?))
        }
    }
}

/// Spawn a Node process as a JSONL plugin. `ELECTRON_RUN_AS_NODE=1` makes an
/// Electron binary (the editor's bundled Node) behave as plain Node; a real
/// `node` binary ignores it.
fn spawn_node(node: &str, args: &[String], timeout: Duration) -> Result<SubprocessRpc> {
    SubprocessRpc::spawn(node, args, &[("ELECTRON_RUN_AS_NODE", "1")], timeout)
}

/// Resolve the Node runtime for JS/WASM plugins: the editor's bundled Node
/// (passed by the extension as `POTEMKIN_NODE`), else `node` on `PATH`.
fn resolve_node() -> String {
    std::env::var("POTEMKIN_NODE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "node".to_string())
}
