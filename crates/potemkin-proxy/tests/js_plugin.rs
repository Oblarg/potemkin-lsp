//! End-to-end test of the `js` transport: run the JavaScript reference plugin
//! with Node and drive it through the `Pipeline` exactly as the proxy would for a
//! real hover response. Mirrors `subprocess_plugin.rs` but exercises the Node
//! runtime resolution path.

use potemkin_plugin_api::protocol::{PluginManifest, Transport};
use potemkin_plugin_api::Plugin;
use potemkin_proxy::plugin::ProtocolPlugin;
use potemkin_proxy::{lsp, Config, Pipeline};

fn demo_js_manifest() -> PluginManifest {
    // The example plugin lives at <workspace>/examples/plugins/demo.js.
    let js = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/plugins/demo.js");
    PluginManifest {
        name: "demo-js".to_string(),
        markers: vec!["JsType<".to_string()],
        languages: vec![],
        language_servers: vec![],
        order: 0,
        timeout_ms: None,
        transport: Transport::Js {
            path: js.to_string(),
            args: vec![],
        },
    }
}

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

#[test]
fn rewrites_hover_via_js_plugin() {
    if !have_node() {
        eprintln!("skipping: node not available");
        return;
    }

    let plugin = ProtocolPlugin::spawn(&demo_js_manifest(), 0, true, std::time::Duration::from_secs(10))
        .expect("spawn js plugin");
    let pipeline = Pipeline::new(vec![Box::new(plugin) as Box<dyn Plugin>], Config::default());

    let request =
        lsp::frame(r#"{"jsonrpc":"2.0","id":1,"method":"textDocument/hover","params":{}}"#);
    pipeline.process_outgoing(&request);

    let hover = r#"{"jsonrpc":"2.0","id":1,"result":{"contents":{"kind":"markdown","value":"let x: JsType<i32> = ...;"}}}"#;
    let framed = lsp::frame(hover);

    let out = pipeline.process_incoming(&framed);
    let payload = lsp::payload(&out).unwrap();
    assert!(
        payload.contains("Js{i32}"),
        "expected rewritten type, got: {payload}"
    );
    assert!(!payload.contains("JsType<"));
}

#[test]
fn untouched_when_no_marker_present_js() {
    if !have_node() {
        eprintln!("skipping: node not available");
        return;
    }

    let plugin = ProtocolPlugin::spawn(&demo_js_manifest(), 0, true, std::time::Duration::from_secs(10))
        .expect("spawn js plugin");
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
