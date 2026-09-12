//! Reference Potemkin plugin compiled to WebAssembly via the extism PDK.
//!
//! It exports functions named after the protocol methods (`initialize`,
//! `transform`), each taking the params JSON as input and returning the result
//! JSON as output. The host (Potemkin's Node wasm-host harness, or any extism
//! host) marshals bytes in and out. This trivial plugin rewrites `WasmType<...>`
//! into a friendlier `Wasm{...}` form to demonstrate the mechanism.
//!
//! Build: `cargo build --release --target wasm32-unknown-unknown` (see
//! ../build-wasm.sh, which also copies the artifact to wasm-demo.wasm).

use extism_pdk::*;
use serde_json::{json, Value};

const NAME: &str = "wasm-demo";
const MARKERS: [&str; 1] = ["WasmType<"];

/// `initialize` -> `{ "name", "markers" }`.
#[plugin_fn]
pub fn initialize(_input: String) -> FnResult<String> {
    Ok(json!({ "name": NAME, "markers": MARKERS }).to_string())
}

/// `transform` -> `{ "items": [ ...rewritten strings... ] }`.
#[plugin_fn]
pub fn transform(input: String) -> FnResult<String> {
    let params: Value = serde_json::from_str(&input)?;
    let items = params
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let out: Vec<Value> = items
        .iter()
        .map(|it| {
            let text = it.get("text").and_then(Value::as_str).unwrap_or("");
            Value::String(rewrite(text))
        })
        .collect();

    Ok(json!({ "items": out }).to_string())
}

/// Replace every `WasmType<INNER>` with `Wasm{INNER}` (no regex, to keep the
/// module tiny).
fn rewrite(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = rest.find("WasmType<") {
        out.push_str(&rest[..idx]);
        let after = &rest[idx + "WasmType<".len()..];
        match after.find('>') {
            Some(end) => {
                out.push_str("Wasm{");
                out.push_str(&after[..end]);
                out.push('}');
                rest = &after[end + 1..];
            }
            None => {
                // Unterminated: copy the remainder verbatim and stop.
                out.push_str(&rest[idx..]);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}
