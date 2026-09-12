//! End-to-end test of the `wasm` transport: run the example WebAssembly plugin
//! through the Node harness (extism JS SDK) and drive it through the `Pipeline`
//! exactly as the proxy would for a real hover response.
//!
//! Skips gracefully unless the prerequisites are present:
//!   - `node` on PATH (or `POTEMKIN_NODE`),
//!   - the staged artifact `examples/plugins/wasm-demo.wasm` (run
//!     `examples/plugins/build-wasm.sh`),
//!   - the harness deps installed (`npm install` in `runtime/wasm-host`).

use std::path::Path;

use potemkin_plugin_api::protocol::{PluginManifest, Transport};
use potemkin_plugin_api::Plugin;
use potemkin_proxy::plugin::ProtocolPlugin;
use potemkin_proxy::{lsp, Config, Pipeline};

const WASM: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/plugins/wasm-demo.wasm");
const HOST: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../runtime/wasm-host/host.js");
const HOST_DEPS: &str =
    concat!(env!("CARGO_MANIFEST_DIR"), "/../../runtime/wasm-host/node_modules");

fn node_cmd() -> String {
    std::env::var("POTEMKIN_NODE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "node".to_string())
}

fn have_node() -> bool {
    std::process::Command::new(node_cmd())
        .arg("--version")
        .env("ELECTRON_RUN_AS_NODE", "1")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Returns true if the wasm prerequisites are present (else the test skips).
fn prerequisites_ready() -> bool {
    if !have_node() {
        eprintln!("skipping: node not available");
        return false;
    }
    if !Path::new(WASM).exists() {
        eprintln!("skipping: {WASM} missing (run examples/plugins/build-wasm.sh)");
        return false;
    }
    if !Path::new(HOST_DEPS).exists() {
        eprintln!("skipping: harness deps missing (npm install in runtime/wasm-host)");
        return false;
    }
    // The proxy reads the harness path from POTEMKIN_WASM_HOST.
    std::env::set_var("POTEMKIN_WASM_HOST", HOST);
    true
}

fn demo_wasm_manifest() -> PluginManifest {
    PluginManifest {
        name: "wasm-demo".to_string(),
        markers: vec!["WasmType<".to_string()],
        languages: vec![],
        language_servers: vec![],
        order: 0,
        timeout_ms: None,
        transport: Transport::Wasm {
            path: WASM.to_string(),
        },
    }
}

#[test]
fn rewrites_hover_via_wasm_plugin() {
    if !prerequisites_ready() {
        return;
    }

    let plugin =
        ProtocolPlugin::spawn(&demo_wasm_manifest(), 0, true, std::time::Duration::from_secs(10))
            .expect("spawn wasm plugin");
    let pipeline = Pipeline::new(vec![Box::new(plugin) as Box<dyn Plugin>], Config::default());

    let request =
        lsp::frame(r#"{"jsonrpc":"2.0","id":1,"method":"textDocument/hover","params":{}}"#);
    pipeline.process_outgoing(&request);

    let hover = r#"{"jsonrpc":"2.0","id":1,"result":{"contents":{"kind":"markdown","value":"let x: WasmType<i32> = ...;"}}}"#;
    let framed = lsp::frame(hover);

    let out = pipeline.process_incoming(&framed);
    let payload = lsp::payload(&out).unwrap();
    assert!(
        payload.contains("Wasm{i32}"),
        "expected rewritten type, got: {payload}"
    );
    assert!(!payload.contains("WasmType<"));
}

#[test]
fn untouched_when_no_marker_present_wasm() {
    if !prerequisites_ready() {
        return;
    }

    let plugin =
        ProtocolPlugin::spawn(&demo_wasm_manifest(), 0, true, std::time::Duration::from_secs(10))
            .expect("spawn wasm plugin");
    let pipeline = Pipeline::new(vec![Box::new(plugin) as Box<dyn Plugin>], Config::default());

    let request =
        lsp::frame(r#"{"jsonrpc":"2.0","id":1,"method":"textDocument/hover","params":{}}"#);
    pipeline.process_outgoing(&request);

    let hover = r#"{"jsonrpc":"2.0","id":1,"result":{"contents":{"kind":"markdown","value":"let x: String = ...;"}}}"#;
    let framed = lsp::frame(hover);

    // No marker in payload -> fast path returns the original message verbatim.
    let out = pipeline.process_incoming(&framed);
    assert_eq!(out, framed);
}
