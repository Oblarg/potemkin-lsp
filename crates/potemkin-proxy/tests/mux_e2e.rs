//! End-to-end test of the per-directory multiplexer against a mock backend.
//!
//! Drives the real `potemkin` binary (with `POTEMKIN_MULTIPLEX=1`) over stdio,
//! wrapping the `mock_lsp` test server, and verifies the whole mux path:
//!   - `initialize` is fanned out and the primary backend's result is relayed
//!     back to the editor (serverInfo present);
//!   - a file at the workspace root is served by the default backend, which was
//!     initialized with the root `potemkin.toml` (`scopeTag = "root"`);
//!   - a file in an override subtree is routed to a *separate* backend that was
//!     lazily spawned and handshaked with that subtree's config
//!     (`scopeTag = "override"`);
//!   - request ids are remapped back into the editor's id space;
//!   - a backend-initiated `workspace/configuration` request is relayed to the
//!     editor and the editor's reply is routed back to the *asking* backend
//!     (proved by folding the reply with that backend's scope tag);
//!   - `shutdown` gets a single reply.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

fn frame(body: &str) -> String {
    format!("Content-Length: {}\r\n\r\n{}", body.len(), body)
}

fn read_message<R: BufRead>(r: &mut R) -> Option<String> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            content_length = rest.trim().parse().ok();
        }
    }
    let len = content_length?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).ok()?;
    String::from_utf8(buf).ok()
}

fn uri(p: &Path) -> String {
    format!("file://{}", p.display())
}

/// The value this fake editor returns for any `workspace/configuration` pull.
const CLIENT_PROBE: &str = "cfg";

fn send<W: Write>(w: &mut W, v: &Value) {
    w.write_all(frame(&v.to_string()).as_bytes())
        .expect("write to potemkin");
    w.flush().unwrap();
}

/// Wait for the *response* with the given id. While waiting, this acts as a real
/// editor: server-initiated requests (which carry both `method` and `id`) are
/// answered so the backend can make progress; unrelated responses are buffered.
fn wait_for_id<W: Write>(
    rx: &Receiver<Value>,
    buf: &mut HashMap<i64, Value>,
    editor: &mut W,
    id: i64,
) -> Value {
    if let Some(v) = buf.remove(&id) {
        return v;
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        let v = rx
            .recv_timeout(remaining)
            .unwrap_or_else(|_| panic!("timed out waiting for response id {id}"));

        let has_method = v.get("method").is_some();
        match v.get("id").and_then(Value::as_i64) {
            // Server-initiated request: reply so the backend unblocks.
            Some(sid) if has_method => {
                let result = if v["method"] == "workspace/configuration" {
                    // One entry per requested item.
                    let n = v.pointer("/params/items").and_then(Value::as_array).map_or(1, Vec::len);
                    Value::Array(vec![Value::String(CLIENT_PROBE.into()); n])
                } else {
                    Value::Null
                };
                send(editor, &json!({ "jsonrpc": "2.0", "id": sid, "result": result }));
            }
            // Response to one of our requests.
            Some(rid) => {
                if rid == id {
                    return v;
                }
                buf.insert(rid, v);
            }
            // Notification (no id): ignore.
            None => {}
        }
    }
}

#[test]
fn multiplexer_routes_per_directory_config() {
    // --- temp workspace: root + override subtree, each with a potemkin.toml ---
    let base = std::env::temp_dir().join(format!(
        "potemkin-e2e-{}-{}",
        std::process::id(),
        Instant::now().elapsed().as_nanos()
    ));
    let root = base.join("proj");
    let sub = root.join("sub");
    let home = base.join("home");
    let plugins = base.join("plugins");
    for d in [&root, &sub, &home, &plugins] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(
        root.join("potemkin.toml"),
        "[servers.mock-ls]\nscopeTag = \"root\"\n",
    )
    .unwrap();
    std::fs::write(
        sub.join("potemkin.toml"),
        "[servers.mock-ls]\nscopeTag = \"override\"\n",
    )
    .unwrap();

    // --- spawn potemkin wrapping the mock server, multiplexer enabled ---------
    let mut child = Command::new(env!("CARGO_BIN_EXE_potemkin"))
        .current_dir(&root)
        .env("POTEMKIN_SERVER", env!("CARGO_BIN_EXE_mock_lsp"))
        .env("POTEMKIN_SERVER_ID", "mock-ls")
        .env("POTEMKIN_MULTIPLEX", "1")
        // Isolate plugin discovery so nothing on the dev box loads.
        .env("POTEMKIN_PLUGINS_DIR", &plugins)
        .env("HOME", &home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn potemkin");

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    let (tx, rx) = mpsc::channel::<Value>();
    std::thread::spawn(move || {
        let mut r = BufReader::new(stdout);
        while let Some(body) = read_message(&mut r) {
            if let Ok(v) = serde_json::from_str::<Value>(&body) {
                if tx.send(v).is_err() {
                    break;
                }
            }
        }
    });

    let mut buf: HashMap<i64, Value> = HashMap::new();

    // initialize -> primary backend's result relayed back.
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "rootUri": uri(&root),
                "workspaceFolders": [{ "uri": uri(&root), "name": "proj" }],
                "initializationOptions": {}
            }
        }),
    );
    let init = wait_for_id(&rx, &mut buf, &mut stdin, 1);
    assert_eq!(
        init["result"]["serverInfo"]["name"], "mock-lsp",
        "primary backend's initialize result should be relayed to the editor"
    );

    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }),
    );

    // Open a file at the root and one in the override subtree.
    for f in [root.join("main.rs"), sub.join("lib.rs")] {
        send(
            &mut stdin,
            &json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": { "textDocument": {
                    "uri": uri(&f), "languageId": "rust", "version": 1, "text": ""
                }}
            }),
        );
    }

    // Hover on the root file -> default backend (root config).
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "textDocument/hover",
            "params": {
                "textDocument": { "uri": uri(&root.join("main.rs")) },
                "position": { "line": 0, "character": 0 }
            }
        }),
    );
    let h_root = wait_for_id(&rx, &mut buf, &mut stdin, 2);
    assert_eq!(
        h_root["result"]["contents"]["value"], "root",
        "root file should be served by the default backend (root potemkin.toml)"
    );

    // Hover on the override file -> its own backend (override config).
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "textDocument/hover",
            "params": {
                "textDocument": { "uri": uri(&sub.join("lib.rs")) },
                "position": { "line": 0, "character": 0 }
            }
        }),
    );
    let h_over = wait_for_id(&rx, &mut buf, &mut stdin, 3);
    assert_eq!(
        h_over["result"]["contents"]["value"], "override",
        "override subtree file should be routed to a backend with the override config"
    );

    // Definition on the root file: the default backend fires a
    // `workspace/configuration` request up to us and folds our reply with its
    // scope tag. Getting "root:cfg" proves the reply was routed back to it.
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": uri(&root.join("main.rs")) },
                "position": { "line": 0, "character": 0 }
            }
        }),
    );
    let d_root = wait_for_id(&rx, &mut buf, &mut stdin, 5);
    assert_eq!(
        d_root["result"]["value"], "root:cfg",
        "default backend's server-initiated request should relay and its reply route back to it"
    );

    // Definition on the override file: the *override* backend fires the request.
    // "override:cfg" proves the editor's reply went back to the asking backend,
    // not its sibling.
    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "textDocument/definition",
            "params": {
                "textDocument": { "uri": uri(&sub.join("lib.rs")) },
                "position": { "line": 0, "character": 0 }
            }
        }),
    );
    let d_over = wait_for_id(&rx, &mut buf, &mut stdin, 6);
    assert_eq!(
        d_over["result"]["value"], "override:cfg",
        "override backend's request reply must route back to it, not the default backend"
    );

    // shutdown -> single reply, correct id.
    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "id": 7, "method": "shutdown" }),
    );
    let sd = wait_for_id(&rx, &mut buf, &mut stdin, 7);
    assert!(sd["result"].is_null(), "shutdown should reply with null");

    send(
        &mut stdin,
        &json!({ "jsonrpc": "2.0", "method": "exit", "params": null }),
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&base);
}
