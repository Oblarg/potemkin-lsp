//! End-to-end test of the cross-language plugin path: spawn the Python reference
//! plugin via the subprocess adapter and drive it through the `Pipeline` exactly
//! as the proxy would for a real hover response.

use std::time::{Duration, Instant};

use potemkin_plugin_api::protocol::{PluginManifest, Transport};
use potemkin_plugin_api::Plugin;
use potemkin_proxy::plugin::ProtocolPlugin;
use potemkin_proxy::{lsp, Config, Pipeline};

const TEST_TIMEOUT: Duration = Duration::from_secs(5);

fn demo_manifest() -> PluginManifest {
    // The example plugin lives at <workspace>/examples/plugins/demo.py.
    let py = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/plugins/demo.py");
    PluginManifest {
        name: "demo".to_string(),
        markers: vec!["DemoType<".to_string()],
        languages: vec![],        // language-agnostic demo
        language_servers: vec![], // any server
        order: 0,
        timeout_ms: None,
        transport: Transport::Subprocess {
            command: "python3".to_string(),
            args: vec![py.to_string()],
        },
    }
}

fn have_python() -> bool {
    std::process::Command::new("python3")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn rewrites_hover_via_subprocess_plugin() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }

    let plugin =
        ProtocolPlugin::spawn(&demo_manifest(), 0, true, TEST_TIMEOUT).expect("spawn demo plugin");
    let pipeline = Pipeline::new(vec![Box::new(plugin) as Box<dyn Plugin>], Config::default());

    // Register the hover request so its response id is tracked, mirroring the
    // real editor -> backend flow.
    let request =
        lsp::frame(r#"{"jsonrpc":"2.0","id":1,"method":"textDocument/hover","params":{}}"#);
    pipeline.process_outgoing(&request);

    let hover = r#"{"jsonrpc":"2.0","id":1,"result":{"contents":{"kind":"markdown","value":"let x: DemoType<i32> = ...;"}}}"#;
    let framed = lsp::frame(hover);

    let out = pipeline.process_incoming(&framed);
    let payload = lsp::payload(&out).unwrap();
    assert!(
        payload.contains("Demo{i32}"),
        "expected rewritten type, got: {payload}"
    );
    assert!(!payload.contains("DemoType<"));
}

#[test]
fn untouched_when_no_marker_present() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }

    let plugin =
        ProtocolPlugin::spawn(&demo_manifest(), 0, true, TEST_TIMEOUT).expect("spawn demo plugin");
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

#[test]
fn ignores_untracked_response_ids() {
    // A response whose id was never issued as a hover/inlay request must be
    // forwarded verbatim, even if it contains marker text (e.g. a diagnostic
    // about a DemoType during workspace indexing).
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }
    let plugin =
        ProtocolPlugin::spawn(&demo_manifest(), 0, true, TEST_TIMEOUT).expect("spawn demo plugin");
    let pipeline = Pipeline::new(vec![Box::new(plugin) as Box<dyn Plugin>], Config::default());

    let hover = r#"{"jsonrpc":"2.0","id":999,"result":{"contents":{"kind":"markdown","value":"let x: DemoType<i32> = ...;"}}}"#;
    let framed = lsp::frame(hover);
    let out = pipeline.process_incoming(&framed);
    assert_eq!(out, framed, "untracked id should be forwarded unchanged");
}

/// A plugin that answers `initialize` but hangs forever on `transform` must be
/// timed out, killed, and dropped — the hover is passed through unchanged and the
/// call returns well within the sleep, not after it. A second request short-
/// circuits because the transport is now disabled.
#[test]
fn hung_transform_times_out_and_passes_through() {
    if !have_python() {
        eprintln!("skipping: python3 not available");
        return;
    }

    // Inline plugin: respond to initialize, then sleep on any transform.
    let script = r#"
import sys, json, time
while True:
    line = sys.stdin.readline()
    if not line:
        break
    req = json.loads(line)
    if req.get("method") == "initialize":
        sys.stdout.write(json.dumps({"id": req["id"], "result": {"name": "hang", "markers": ["DemoType<"]}}) + "\n")
        sys.stdout.flush()
    else:
        time.sleep(60)
"#;
    let manifest = PluginManifest {
        name: "hang".to_string(),
        markers: vec!["DemoType<".to_string()],
        languages: vec![],
        language_servers: vec![],
        order: 0,
        // A short per-plugin override so the test resolves quickly; initialize
        // still responds well within it.
        timeout_ms: Some(500),
        transport: Transport::Subprocess {
            command: "python3".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
        },
    };

    // Spawn succeeds because `initialize` answers in time.
    let plugin = ProtocolPlugin::spawn(&manifest, 0, true, TEST_TIMEOUT).expect("spawn hang plugin");
    let pipeline = Pipeline::new(vec![Box::new(plugin) as Box<dyn Plugin>], Config::default());

    let request =
        lsp::frame(r#"{"jsonrpc":"2.0","id":1,"method":"textDocument/hover","params":{}}"#);
    pipeline.process_outgoing(&request);
    let hover = r#"{"jsonrpc":"2.0","id":1,"result":{"contents":{"kind":"markdown","value":"let x: DemoType<i32> = ...;"}}}"#;
    let framed = lsp::frame(hover);

    let start = Instant::now();
    let out = pipeline.process_incoming(&framed);
    let elapsed = start.elapsed();

    // Passed through unchanged (plugin never produced a rewrite), and returned
    // far sooner than the plugin's 60s sleep.
    assert_eq!(out, framed, "hung plugin should pass the hover through unchanged");
    assert!(
        elapsed < Duration::from_secs(10),
        "timeout should fire promptly, took {elapsed:?}"
    );

    // The transport is now disabled: a second tracked request also passes
    // through, and quickly (no second 500ms wait — it fails fast).
    let request2 =
        lsp::frame(r#"{"jsonrpc":"2.0","id":2,"method":"textDocument/hover","params":{}}"#);
    pipeline.process_outgoing(&request2);
    let hover2 = r#"{"jsonrpc":"2.0","id":2,"result":{"contents":{"kind":"markdown","value":"let y: DemoType<u8> = ...;"}}}"#;
    let framed2 = lsp::frame(hover2);
    let start2 = Instant::now();
    let out2 = pipeline.process_incoming(&framed2);
    assert_eq!(out2, framed2, "disabled plugin should pass through");
    assert!(
        start2.elapsed() < Duration::from_millis(400),
        "disabled plugin should fail fast, took {:?}",
        start2.elapsed()
    );
}
