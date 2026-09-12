//! Minimal mock language server for Potemkin's multiplexer end-to-end test.
//!
//! It speaks LSP `Content-Length` framing over stdio and does just enough to
//! exercise the mux:
//!
//! - on `initialize` it records `initializationOptions.scopeTag` and echoes that
//!   string back from every `textDocument/hover`, proving the *right
//!   per-directory config* reached the *right backend* (routing + injection),
//!   with ids mapped back correctly;
//! - on `textDocument/definition` it acts as a *server-initiated requester*: it
//!   sends a `workspace/configuration` request up to the editor, waits for the
//!   reply, and returns `"<scopeTag>:<client value>"`. This exercises the
//!   backend→editor request relay and, crucially, that the editor's reply is
//!   routed back to the backend that asked (this backend, not a sibling).
//!
//! `shutdown` replies `null`; `exit` ends the process.
//!
//! This is test-support only; it is not the `potemkin` binary and is never
//! bundled by the VS Code packaging scripts (which copy `potemkin` by name).

use std::io::{self, BufRead, BufReader, Write};

use serde_json::{json, Value};

fn main() {
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut out = stdout.lock();

    let mut scope_tag = String::from("none");
    // Ids for our own server→editor requests, kept well clear of editor ids.
    let mut next_req: i64 = 9000;

    while let Some(body) = read_message(&mut reader) {
        let Ok(v) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        let method = v.get("method").and_then(Value::as_str);
        let id = v.get("id").cloned().unwrap_or(Value::Null);

        match method {
            Some("initialize") => {
                if let Some(tag) = v
                    .pointer("/params/initializationOptions/scopeTag")
                    .and_then(Value::as_str)
                {
                    scope_tag = tag.to_string();
                }
                write_message(
                    &mut out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "capabilities": { "hoverProvider": true, "definitionProvider": true },
                            "serverInfo": { "name": "mock-lsp" }
                        }
                    }),
                );
            }
            Some("textDocument/hover") => {
                write_message(
                    &mut out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "contents": { "kind": "plaintext", "value": scope_tag } }
                    }),
                );
            }
            Some("textDocument/definition") => {
                // Ask the editor for a config value, then fold it together with
                // our own scope tag so the test can prove the reply came back to
                // *this* backend.
                let req_id = next_req;
                next_req += 1;
                write_message(
                    &mut out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": req_id,
                        "method": "workspace/configuration",
                        "params": { "items": [{ "section": "probe" }] }
                    }),
                );
                let client = await_config_reply(&mut reader, req_id).unwrap_or_default();
                write_message(
                    &mut out,
                    &json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "value": format!("{scope_tag}:{client}") }
                    }),
                );
            }
            Some("shutdown") => {
                write_message(
                    &mut out,
                    &json!({ "jsonrpc": "2.0", "id": id, "result": null }),
                );
            }
            Some("exit") => break,
            // All other requests/notifications (initialized, didOpen, …) ignored.
            _ => {}
        }
    }
}

/// Block until the editor's reply to our `workspace/configuration` request with
/// `req_id` arrives, returning the first item's string. Non-matching messages
/// during the wait are ignored (the test doesn't interleave other traffic here).
fn await_config_reply<R: BufRead>(reader: &mut R, req_id: i64) -> Option<String> {
    while let Some(body) = read_message(reader) {
        let Ok(v) = serde_json::from_str::<Value>(&body) else {
            continue;
        };
        if v.get("method").is_none() && v.get("id").and_then(Value::as_i64) == Some(req_id) {
            return v
                .pointer("/result/0")
                .and_then(Value::as_str)
                .map(str::to_string);
        }
    }
    None
}

/// Read one LSP message body, or `None` at EOF.
fn read_message<R: BufRead>(r: &mut R) -> Option<String> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            break; // blank line terminates headers
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

fn write_message<W: Write>(w: &mut W, v: &Value) {
    let body = serde_json::to_string(v).unwrap();
    let _ = write!(w, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = w.flush();
}
