//! Per-directory language-server config injection.
//!
//! Potemkin is a transparent LSP proxy, so any config file a server reads itself
//! (e.g. `rust-analyzer.toml`) already works untouched. This module adds an
//! *orthogonal* layer: a generic `potemkin.toml` that Potemkin injects into the
//! LSP config channels, giving **any** server project-local — and per-directory —
//! configuration it doesn't natively provide.
//!
//! It only ever supplies the server's **initial settings** (the `initialize`
//! request's `initializationOptions`); it never touches the server's ongoing
//! `workspace/configuration` pulls, so the live config protocol is never
//! hijacked.
//!
//! ## File format
//!
//! A `potemkin.toml` carries one table per server id (matched against
//! `POTEMKIN_SERVER_ID`). The table contents mirror that server's own config
//! namespace, so they can be injected verbatim as `initializationOptions`:
//!
//! ```toml
//! [servers.rust-analyzer]
//! checkOnSave = false
//! cargo.features = ["ci"]        # dotted keys nest: { cargo: { features: [..] } }
//! ```
//!
//! ## Discovery & precedence
//!
//! For a given directory, Potemkin walks up the tree collecting `potemkin.toml`
//! files and deep-merges them **nearest-wins**. The merged result is then
//! deep-merged over whatever the editor supplied, with the **file winning** for
//! the keys it specifies (editor keys it doesn't mention are preserved).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use log::warn;
use serde_json::Value;

/// The config filename Potemkin discovers in the workspace tree.
pub const CONFIG_FILE: &str = "potemkin.toml";

/// Loads, caches, and resolves per-directory `potemkin.toml` server config for a
/// single server id.
pub struct ConfigLayer {
    server_id: Option<String>,
    /// dir -> merged server-config object for `server_id` (nearest-wins).
    cache: Mutex<HashMap<PathBuf, Value>>,
}

impl ConfigLayer {
    pub fn new(server_id: Option<String>) -> Self {
        Self {
            server_id: server_id.filter(|s| !s.trim().is_empty()),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Whether any injection should happen at all. When there's no server id we
    /// can't map config to a server, so the layer is inert (a plain proxy).
    pub fn active(&self) -> bool {
        self.server_id.is_some()
    }

    /// The merged server-config object that applies to `dir` (walking up the
    /// tree, nearest file winning). `Value::Null` if inactive or nothing applies.
    pub fn for_dir(&self, dir: &Path) -> Value {
        if self.server_id.is_none() {
            return Value::Null;
        }
        let key = dir.to_path_buf();
        if let Some(v) = self.cache.lock().unwrap().get(&key) {
            return v.clone();
        }
        let merged = self.load_merged(dir);
        self.cache.lock().unwrap().insert(key, merged.clone());
        merged
    }

    /// Collect `potemkin.toml` files from `dir` up to the filesystem root and
    /// deep-merge their server tables, nearest winning.
    fn load_merged(&self, dir: &Path) -> Value {
        let Some(server_id) = self.server_id.as_deref() else {
            return Value::Null;
        };

        // Nearest-first as we walk up...
        let mut files: Vec<PathBuf> = Vec::new();
        let mut cur = Some(dir);
        while let Some(d) = cur {
            let f = d.join(CONFIG_FILE);
            if f.is_file() {
                files.push(f);
            }
            cur = d.parent();
        }
        // ...merge farthest-first so nearer files override.
        files.reverse();

        let mut acc = Value::Null;
        for f in files {
            if let Some(cfg) = read_server_config(&f, server_id) {
                deep_merge(&mut acc, cfg);
            }
        }
        acc
    }
}

/// Parse a `potemkin.toml` and extract `servers.<server_id>` as JSON.
fn read_server_config(file: &Path, server_id: &str) -> Option<Value> {
    let text = std::fs::read_to_string(file).ok()?;
    let toml_val: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            warn!("ignoring malformed {}: {e}", file.display());
            return None;
        }
    };
    let json = serde_json::to_value(toml_val).ok()?;
    json.get("servers")
        .and_then(|s| s.get(server_id))
        .filter(|v| !v.is_null())
        .cloned()
}

/// Recursively merge `overlay` into `base`, with `overlay` winning. Objects are
/// merged key-by-key; any non-object overlay replaces the base value entirely
/// (so a scalar/array in the file overrides the editor's value for that key,
/// while sibling keys the file doesn't mention are preserved).
pub fn deep_merge(base: &mut Value, overlay: Value) {
    match overlay {
        Value::Object(o) => {
            if !base.is_object() {
                *base = Value::Object(serde_json::Map::new());
            }
            let b = base.as_object_mut().expect("just ensured object");
            for (k, v) in o {
                match b.get_mut(&k) {
                    Some(existing) => deep_merge(existing, v),
                    None => {
                        b.insert(k, v);
                    }
                }
            }
        }
        other => *base = other,
    }
}

/// Convert a `file://` URI to a filesystem path (best-effort: handles the common
/// `file:///abs/path` form and percent-encoding). Returns `None` for non-file or
/// unparseable URIs.
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // Drop an (empty) authority component: file:///a -> "/a", file://host/a -> "/a".
    let path = match rest.find('/') {
        Some(idx) => &rest[idx..],
        None => rest,
    };
    let decoded = percent_decode(path);
    #[cfg(windows)]
    {
        // file:///C:/x -> "/C:/x" -> "C:/x"
        Some(PathBuf::from(decoded.trim_start_matches('/')))
    }
    #[cfg(not(windows))]
    {
        Some(PathBuf::from(decoded))
    }
}

/// Minimal `%XX` percent-decoding for file URIs.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deep_merge_file_wins_but_preserves_siblings() {
        let mut base = json!({ "checkOnSave": true, "cargo": { "features": ["a"], "keep": 1 } });
        let overlay = json!({ "checkOnSave": false, "cargo": { "features": ["b"] } });
        deep_merge(&mut base, overlay);
        assert_eq!(base["checkOnSave"], json!(false)); // file wins
        assert_eq!(base["cargo"]["features"], json!(["b"])); // array replaced
        assert_eq!(base["cargo"]["keep"], json!(1)); // sibling preserved
    }

    #[test]
    fn uri_to_path_decodes() {
        #[cfg(not(windows))]
        assert_eq!(
            uri_to_path("file:///home/me/my%20proj").unwrap(),
            PathBuf::from("/home/me/my proj")
        );
    }
}
