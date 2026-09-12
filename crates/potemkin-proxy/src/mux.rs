//! LSP multiplexer: fan a single editor session out to multiple backend
//! language-server processes — one per per-directory config *scope*.
//!
//! # Why
//!
//! A single language-server session may be workspace-wide, so it can't hold genuinely
//! different settings for different directories without hijacking the live
//! `workspace/configuration` protocol (which we refuse to do — see
//! [`crate::config_layer`]). Instead, Potemkin runs a **default backend** for the
//! workspace plus an **extra backend for any subtree whose `potemkin.toml`
//! actually overrides the server's settings**, and routes each document to the
//! backend that owns its subtree. Every backend is the same server binary; they
//! differ only in the `initializationOptions` we hand each at startup (the
//! editor's settings with that scope's `potemkin.toml` layered on top).
//!
//! # How
//!
//! Potemkin becomes a small router between one editor and N backends:
//!
//! - **Routing** — requests/notifications carrying a `textDocument.uri` go to the
//!   owning backend; document-less requests (`workspace/*`, resolves) go to the
//!   default backend; document-less notifications broadcast to all backends.
//! - **Id remapping** — every editor→backend request is re-issued under a
//!   Potemkin-assigned id so ids from different backends never collide; responses
//!   are mapped back. Backend→editor (server-initiated) requests are likewise
//!   re-issued toward the editor and mapped back on reply.
//! - **Handshake** — `initialize` is captured and replayed to each backend as it
//!   spawns (lazily) with that scope's settings; the *primary* backend's
//!   `initialize` result is the one returned to the editor (all backends are the
//!   same binary, so capabilities match). Potemkin drives `initialized` to each
//!   backend itself and queues traffic until a backend is ready.
//! - **Plugin pipeline** — backend→editor responses still flow through the
//!   [`Pipeline`] for type pretty-printing, in the editor's id space.
//!
//! Known v1 limitations: document-less *resolve* requests (`inlayHint/resolve`,
//! `completionItem/resolve`, …) and workspace-wide requests are served by the
//! default backend, so they may be wrong for files in an override subtree;
//! `$/cancelRequest` is broadcast rather than routed. Multiple backends over a
//! nested layout each index their view of the tree.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use log::{error, info};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command as TokioCommand};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::config_layer::{deep_merge, uri_to_path, ConfigLayer, CONFIG_FILE};
use crate::{lsp, Pipeline};

/// Which config scope a backend serves.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ScopeKey {
    /// The workspace-wide default backend.
    Default,
    /// A subtree rooted at this directory whose settings differ from the default.
    Dir(PathBuf),
}

enum Status {
    /// Handshake in flight; messages destined for the backend are queued here.
    Initializing(Vec<String>),
    Ready,
}

struct Backend {
    to_backend: UnboundedSender<String>,
    status: Status,
    child: Child,
    is_primary: bool,
    /// The Potemkin-assigned id of this backend's `initialize` request.
    init_id: i64,
}

/// An editor→backend request awaiting a backend response.
struct EditorReq {
    editor_id: Value,
}

/// A backend→editor (server-initiated) request awaiting the editor's reply.
struct ServerReq {
    backend_id: usize,
    orig_id: Value,
}

#[derive(Default)]
struct MuxState {
    backends: HashMap<usize, Backend>,
    scope_to_backend: HashMap<ScopeKey, usize>,
    /// Potemkin-id → editor request, for translating backend responses back.
    editor_reqs: HashMap<i64, EditorReq>,
    /// Potemkin-id → server request, for routing the editor's reply back.
    server_reqs: HashMap<i64, ServerReq>,
    /// Backend response ids to silently drop (e.g. non-primary `shutdown`).
    swallow: HashSet<i64>,
    editor_initialize_id: Option<Value>,
    primary: Option<usize>,
    next_backend: usize,
}

/// The multiplexer. Cheap to clone via [`Arc`]; all mutable state is behind a
/// single mutex, and no lock is ever held across an `.await`.
pub struct Mux {
    state: Mutex<MuxState>,
    to_editor: UnboundedSender<String>,
    config_layer: ConfigLayer,
    pipeline: Pipeline,
    server_path: String,
    args: Vec<String>,
    next_id: AtomicI64,
    /// Workspace roots captured from `initialize`.
    roots: Mutex<Vec<PathBuf>>,
    /// The `initialize` params captured from the editor, replayed to each backend.
    init_params: Mutex<Option<Value>>,
}

impl Mux {
    /// Build a multiplexer for `server_path` (spawned with `args`), tagging its
    /// config scopes for `server_id`. Returns the mux plus the receiver the
    /// caller must drain to the editor's stdout.
    pub fn new(
        server_path: String,
        args: Vec<String>,
        server_id: Option<String>,
        pipeline: Pipeline,
    ) -> (Arc<Self>, UnboundedReceiver<String>) {
        let (to_editor, editor_rx) = mpsc::unbounded_channel();
        let mux = Arc::new(Self {
            state: Mutex::new(MuxState::default()),
            to_editor,
            config_layer: ConfigLayer::new(server_id),
            pipeline,
            server_path,
            args,
            next_id: AtomicI64::new(1),
            roots: Mutex::new(Vec::new()),
            init_params: Mutex::new(None),
        });
        (mux, editor_rx)
    }

    fn next_id(&self) -> i64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    // --- Editor -> backend ------------------------------------------------

    /// Handle one framed message from the editor.
    pub fn on_editor_message(self: &Arc<Self>, raw: &str) {
        // Let the pipeline record hover/inlay ids in the editor id space.
        let _ = self.pipeline.process_outgoing(raw);

        let Some(payload) = lsp::payload(raw) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        let method = v.get("method").and_then(Value::as_str);
        let has_id = v.get("id").is_some();
        match (method, has_id) {
            (Some(m), true) => self.editor_request(m, &v),
            (Some(m), false) => self.editor_notification(m, &v, raw),
            (None, true) => self.editor_response(&v),
            (None, false) => {}
        }
    }

    fn editor_request(self: &Arc<Self>, method: &str, v: &Value) {
        let id = v.get("id").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                *self.init_params.lock().unwrap() = v.get("params").cloned();
                *self.roots.lock().unwrap() = v
                    .get("params")
                    .map(extract_roots)
                    .unwrap_or_default();
                self.state.lock().unwrap().editor_initialize_id = Some(id);
                // Spawn the default backend; its initialize result is relayed to
                // the editor when it completes its handshake.
                self.ensure_backend(ScopeKey::Default);
            }
            "shutdown" => self.broadcast_shutdown(id),
            _ => {
                let scope = self.scope_for_request(v);
                let backend_id = self.ensure_backend(scope);
                let our = self.next_id();
                self.state
                    .lock()
                    .unwrap()
                    .editor_reqs
                    .insert(our, EditorReq { editor_id: id });
                let mut out = v.clone();
                out["id"] = json!(our);
                self.send_to_backend(backend_id, frame_value(&out));
            }
        }
    }

    fn editor_notification(self: &Arc<Self>, method: &str, v: &Value, raw: &str) {
        match method {
            // We drive `initialized` to each backend ourselves during handshake.
            "initialized" => {}
            "exit" => self.broadcast(raw.to_string()),
            _ => match uri_of(v) {
                Some(uri) => {
                    let backend_id = self.ensure_backend(self.scope_for_uri(uri));
                    self.send_to_backend(backend_id, raw.to_string());
                }
                None => self.broadcast(raw.to_string()),
            },
        }
    }

    /// The editor's reply to a server-initiated request: route it back to the
    /// backend that asked, restoring the backend's original request id.
    fn editor_response(&self, v: &Value) {
        let Some(our) = v.get("id").and_then(Value::as_i64) else {
            return;
        };
        let sr = self.state.lock().unwrap().server_reqs.remove(&our);
        if let Some(sr) = sr {
            let mut out = v.clone();
            out["id"] = sr.orig_id;
            self.send_to_backend(sr.backend_id, frame_value(&out));
        }
    }

    // --- Backend -> editor ------------------------------------------------

    /// Handle one framed message from backend `backend_id`.
    pub fn on_backend_message(self: &Arc<Self>, backend_id: usize, raw: &str) {
        let Some(payload) = lsp::payload(raw) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(payload) else {
            return;
        };
        let method = v.get("method").and_then(Value::as_str);
        let has_id = v.get("id").is_some();

        match (method, has_id) {
            // Server-initiated request: re-issue toward the editor under a fresh
            // id and remember how to route the reply back.
            (Some(_), true) => {
                let our = self.next_id();
                let orig_id = v.get("id").cloned().unwrap_or(Value::Null);
                self.state
                    .lock()
                    .unwrap()
                    .server_reqs
                    .insert(our, ServerReq { backend_id, orig_id });
                let mut out = v.clone();
                out["id"] = json!(our);
                let _ = self.to_editor.send(frame_value(&out));
            }
            // Server notification (diagnostics, progress, logs): forward as-is.
            (Some(_), false) => {
                let _ = self.to_editor.send(raw.to_string());
            }
            // Response to one of our requests.
            (None, true) => self.backend_response(backend_id, &v),
            (None, false) => {}
        }
    }

    fn backend_response(self: &Arc<Self>, backend_id: usize, v: &Value) {
        let Some(our) = v.get("id").and_then(Value::as_i64) else {
            return;
        };

        // Classify under a short lock (no awaits, no channel sends besides sync).
        let action = {
            let mut st = self.state.lock().unwrap();
            let is_init = st
                .backends
                .get(&backend_id)
                .map(|b| matches!(b.status, Status::Initializing(_)) && b.init_id == our)
                .unwrap_or(false);
            if is_init {
                let primary = st
                    .backends
                    .get(&backend_id)
                    .map(|b| b.is_primary)
                    .unwrap_or(false);
                RespAction::Init { primary }
            } else if let Some(er) = st.editor_reqs.remove(&our) {
                RespAction::Editor {
                    editor_id: er.editor_id,
                }
            } else if st.swallow.remove(&our) {
                RespAction::Swallow
            } else {
                RespAction::Unknown
            }
        };

        match action {
            RespAction::Init { primary } => self.complete_handshake(backend_id, v, primary),
            RespAction::Editor { editor_id } => {
                let mut out = v.clone();
                out["id"] = editor_id;
                // Run the (translated) response through the plugin pipeline so
                // hover/inlay type text is pretty-printed in the editor id space.
                let transformed = self.pipeline.process_incoming(&frame_value(&out));
                let _ = self.to_editor.send(transformed);
            }
            RespAction::Swallow | RespAction::Unknown => {}
        }
    }

    /// Finish a backend's handshake: mark it ready, drive `initialized`, flush any
    /// queued traffic, and (for the primary) relay its `initialize` result to the
    /// editor under the editor's original id.
    fn complete_handshake(&self, backend_id: usize, init_result: &Value, primary: bool) {
        let initialized =
            lsp::frame(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#);
        {
            let mut st = self.state.lock().unwrap();
            if let Some(b) = st.backends.get_mut(&backend_id) {
                let _ = b.to_backend.send(initialized);
                if let Status::Initializing(queue) =
                    std::mem::replace(&mut b.status, Status::Ready)
                {
                    for m in queue {
                        let _ = b.to_backend.send(m);
                    }
                }
            }
        }

        if primary {
            let editor_id = self.state.lock().unwrap().editor_initialize_id.clone();
            let mut out = init_result.clone();
            if let Some(eid) = editor_id {
                out["id"] = eid;
            }
            let _ = self.to_editor.send(frame_value(&out));
        }
    }

    // --- Backend lifecycle ------------------------------------------------

    /// Return the backend serving `scope`, spawning (and starting the handshake
    /// for) it if necessary. Only ever called from the single editor-reader task,
    /// so there is no spawn race.
    fn ensure_backend(self: &Arc<Self>, scope: ScopeKey) -> usize {
        if let Some(id) = self.state.lock().unwrap().scope_to_backend.get(&scope) {
            return *id;
        }
        self.spawn_backend(scope)
    }

    fn spawn_backend(self: &Arc<Self>, scope: ScopeKey) -> usize {
        let (backend_id, is_primary) = {
            let mut st = self.state.lock().unwrap();
            let id = st.next_backend;
            st.next_backend += 1;
            let is_primary = st.primary.is_none() && scope == ScopeKey::Default;
            (id, is_primary)
        };

        // Compute this scope's initializationOptions: editor's options with the
        // scope's potemkin.toml layered on top (file wins).
        let scope_dir = match &scope {
            ScopeKey::Dir(d) => Some(d.clone()),
            ScopeKey::Default => self.roots.lock().unwrap().first().cloned(),
        };
        let scope_cfg = scope_dir
            .as_deref()
            .map(|d| self.config_layer.for_dir(d))
            .unwrap_or(Value::Null);

        let mut params = self
            .init_params
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| json!({}));
        if !scope_cfg.is_null() {
            if !params.is_object() {
                params = json!({});
            }
            let opts = params
                .as_object_mut()
                .unwrap()
                .entry("initializationOptions")
                .or_insert(Value::Null);
            deep_merge(opts, scope_cfg);
        }

        let init_id = self.next_id();
        let init_msg = json!({
            "jsonrpc": "2.0",
            "id": init_id,
            "method": "initialize",
            "params": params,
        });

        let mut child = match self.spawn_child() {
            Ok(c) => c,
            Err(e) => {
                error!("failed to spawn backend for {scope:?}: {e}");
                // Register nothing; the (rare) affected requests will simply be
                // dropped. POTEMKIN_SERVER was validated at startup, so this is
                // unlikely in practice.
                return backend_id;
            }
        };

        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        // Per-backend writer task: drain the channel to the child's stdin.
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(msg) = rx.recv().await {
                if stdin.write_all(msg.as_bytes()).await.is_err() || stdin.flush().await.is_err()
                {
                    break;
                }
            }
        });

        // Per-backend reader task: dispatch the child's stdout back through us.
        let mux = self.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut header = String::new();
            loop {
                match lsp::read_message(&mut reader, &mut header).await {
                    Ok(Some(m)) => mux.on_backend_message(backend_id, &m),
                    Ok(None) => break,
                    Err(e) => {
                        error!("backend {backend_id} read error: {e}");
                        break;
                    }
                }
            }
        });

        // Forward backend stderr to ours for diagnostics.
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => eprint!("{line}"),
                    Err(_) => break,
                }
            }
        });

        // Send `initialize` first, then register the backend as initializing.
        let _ = tx.send(lsp::frame(&init_msg.to_string()));
        {
            let mut st = self.state.lock().unwrap();
            st.backends.insert(
                backend_id,
                Backend {
                    to_backend: tx,
                    status: Status::Initializing(Vec::new()),
                    child,
                    is_primary,
                    init_id,
                },
            );
            st.scope_to_backend.insert(scope.clone(), backend_id);
            if is_primary {
                st.primary = Some(backend_id);
            }
        }
        info!("spawned backend {backend_id} for scope {scope:?} (primary={is_primary})");
        backend_id
    }

    fn spawn_child(&self) -> anyhow::Result<Child> {
        let mut cmd = TokioCommand::new(&self.server_path);
        cmd.args(&self.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for key in ["PATH", "DEVELOPER_DIR", "SDKROOT", "RUST_LOG"] {
            if let Ok(val) = std::env::var(key) {
                cmd.env(key, val);
            }
        }
        Ok(cmd.spawn()?)
    }

    /// Fan `shutdown` out to every backend, relaying the primary's reply to the
    /// editor and swallowing the rest.
    fn broadcast_shutdown(&self, editor_id: Value) {
        let backends: Vec<(usize, bool)> = {
            let st = self.state.lock().unwrap();
            st.backends.iter().map(|(k, b)| (*k, b.is_primary)).collect()
        };
        if backends.is_empty() {
            let _ = self
                .to_editor
                .send(frame_value(&json!({ "jsonrpc": "2.0", "id": editor_id, "result": null })));
            return;
        }
        for (backend_id, primary) in backends {
            let our = self.next_id();
            {
                let mut st = self.state.lock().unwrap();
                if primary {
                    st.editor_reqs.insert(
                        our,
                        EditorReq {
                            editor_id: editor_id.clone(),
                        },
                    );
                } else {
                    st.swallow.insert(our);
                }
            }
            let msg = json!({ "jsonrpc": "2.0", "id": our, "method": "shutdown" });
            self.send_to_backend(backend_id, frame_value(&msg));
        }
    }

    /// Send to a backend, queueing if it is still handshaking.
    fn send_to_backend(&self, backend_id: usize, framed: String) {
        let mut st = self.state.lock().unwrap();
        if let Some(b) = st.backends.get_mut(&backend_id) {
            match &mut b.status {
                Status::Ready => {
                    let _ = b.to_backend.send(framed);
                }
                Status::Initializing(queue) => queue.push(framed),
            }
        }
    }

    fn broadcast(&self, framed: String) {
        let mut st = self.state.lock().unwrap();
        for b in st.backends.values_mut() {
            match &mut b.status {
                Status::Ready => {
                    let _ = b.to_backend.send(framed.clone());
                }
                Status::Initializing(queue) => queue.push(framed.clone()),
            }
        }
    }

    /// Kill every backend process. Called on session teardown.
    pub fn shutdown(&self) {
        let mut st = self.state.lock().unwrap();
        for b in st.backends.values_mut() {
            let _ = b.child.start_kill();
        }
    }

    // --- Scope resolution -------------------------------------------------

    fn scope_for_request(&self, v: &Value) -> ScopeKey {
        match uri_of(v) {
            Some(uri) => self.scope_for_uri(uri),
            None => ScopeKey::Default,
        }
    }

    /// Resolve the config scope that owns `uri` against the current roots.
    fn scope_for_uri(&self, uri: &str) -> ScopeKey {
        let roots = self.roots.lock().unwrap().clone();
        resolve_scope(&roots, &self.config_layer, uri)
    }
}

/// The config scope that owns `uri`: the nearest ancestor directory (within a
/// workspace `root`) that holds a `potemkin.toml` whose effective settings differ
/// from the default (root) backend's. Falls back to [`ScopeKey::Default`].
///
/// Pulled out as a free function so the default-vs-override decision — the part
/// that keeps the backend (and indexing) count minimal — is unit-testable.
fn resolve_scope(roots: &[PathBuf], cfg: &ConfigLayer, uri: &str) -> ScopeKey {
    let Some(path) = uri_to_path(uri) else {
        return ScopeKey::Default;
    };
    // Start from the containing directory (a file's parent, or the dir itself).
    let start = if path.is_dir() {
        path
    } else {
        path.parent().map(Path::to_path_buf).unwrap_or(path)
    };

    // Pick the deepest workspace root that contains this path.
    let Some(base) = roots
        .iter()
        .filter(|r| start.starts_with(r))
        .max_by_key(|r| r.components().count())
        .cloned()
    else {
        return ScopeKey::Default;
    };

    // Walk up from `start` to `base` for the nearest `potemkin.toml`.
    let mut found: Option<PathBuf> = None;
    let mut cur: &Path = &start;
    loop {
        if cur.join(CONFIG_FILE).is_file() {
            found = Some(cur.to_path_buf());
            break;
        }
        if cur == base {
            break;
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break,
        }
    }

    match found {
        None => ScopeKey::Default,
        Some(dir) => {
            // Only a distinct backend if the effective settings actually differ
            // from the default (root) backend's.
            if cfg.for_dir(&dir) == cfg.for_dir(&base) {
                ScopeKey::Default
            } else {
                ScopeKey::Dir(dir)
            }
        }
    }
}

enum RespAction {
    Init { primary: bool },
    Editor { editor_id: Value },
    Swallow,
    Unknown,
}

/// Serialize a JSON value and wrap it in LSP `Content-Length` framing.
fn frame_value(v: &Value) -> String {
    lsp::frame(&serde_json::to_string(v).unwrap_or_default())
}

/// The `textDocument.uri` a message refers to, if any.
fn uri_of(v: &Value) -> Option<&str> {
    v.get("params")?
        .get("textDocument")?
        .get("uri")?
        .as_str()
}

/// Extract workspace root directories from `initialize` params, trying
/// `workspaceFolders`, then `rootUri`, then `rootPath`.
fn extract_roots(params: &Value) -> Vec<PathBuf> {
    if let Some(folders) = params.get("workspaceFolders").and_then(Value::as_array) {
        let roots: Vec<PathBuf> = folders
            .iter()
            .filter_map(|f| f.get("uri").and_then(Value::as_str))
            .filter_map(uri_to_path)
            .collect();
        if !roots.is_empty() {
            return roots;
        }
    }
    if let Some(uri) = params.get("rootUri").and_then(Value::as_str) {
        if let Some(p) = uri_to_path(uri) {
            return vec![p];
        }
    }
    if let Some(p) = params.get("rootPath").and_then(Value::as_str) {
        return vec![PathBuf::from(p)];
    }
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_of_reads_text_document() {
        let v = json!({
            "method": "textDocument/hover",
            "params": { "textDocument": { "uri": "file:///a/b.rs" } }
        });
        assert_eq!(uri_of(&v), Some("file:///a/b.rs"));
        assert_eq!(uri_of(&json!({ "method": "shutdown" })), None);
    }

    #[test]
    fn extract_roots_prefers_workspace_folders() {
        let v = json!({
            "workspaceFolders": [{ "uri": "file:///ws/a" }, { "uri": "file:///ws/b" }],
            "rootUri": "file:///ws"
        });
        assert_eq!(
            extract_roots(&v),
            vec![PathBuf::from("/ws/a"), PathBuf::from("/ws/b")]
        );
    }

    #[test]
    fn extract_roots_falls_back_to_root_uri() {
        let v = json!({ "rootUri": "file:///ws" });
        assert_eq!(extract_roots(&v), vec![PathBuf::from("/ws")]);
    }

    use std::sync::atomic::AtomicU32;
    static TMP_SEQ: AtomicU32 = AtomicU32::new(0);

    fn temp_root() -> PathBuf {
        let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("potemkin-mux-{}-{}", std::process::id(), n));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn write_toml(dir: &Path, contents: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(CONFIG_FILE), contents).unwrap();
    }

    fn uri(p: &Path) -> String {
        format!("file://{}", p.display())
    }

    #[test]
    fn resolve_scope_only_splits_on_effective_difference() {
        let root = temp_root();
        // Root sets a baseline for the `demo` server.
        write_toml(&root, "[servers.demo]\nvalue = 1\n");

        // An override subtree that genuinely changes the `demo` settings.
        let over = root.join("override");
        write_toml(&over, "[servers.demo]\nvalue = 2\n");

        // A subtree whose potemkin.toml resolves to the *same* effective config.
        let same = root.join("same");
        write_toml(&same, "[servers.demo]\nvalue = 1\n");

        // A subtree that only configures a *different* server.
        let other = root.join("other");
        write_toml(&other, "[servers.otherls]\nvalue = 9\n");

        let cfg = ConfigLayer::new(Some("demo".into()));
        let roots = vec![root.clone()];

        // Override subtree → its own backend.
        assert_eq!(
            resolve_scope(&roots, &cfg, &uri(&over.join("f.rs"))),
            ScopeKey::Dir(over.clone())
        );
        // Identical effective config → default backend (no extra process).
        assert_eq!(
            resolve_scope(&roots, &cfg, &uri(&same.join("f.rs"))),
            ScopeKey::Default
        );
        // Only-other-server override → default backend for `demo`.
        assert_eq!(
            resolve_scope(&roots, &cfg, &uri(&other.join("f.rs"))),
            ScopeKey::Default
        );
        // File directly under the root → default backend.
        assert_eq!(
            resolve_scope(&roots, &cfg, &uri(&root.join("f.rs"))),
            ScopeKey::Default
        );
        // File outside any workspace root → default backend.
        assert_eq!(
            resolve_scope(&roots, &cfg, &uri(Path::new("/nonexistent/x.rs"))),
            ScopeKey::Default
        );

        let _ = std::fs::remove_dir_all(&root);
    }
}
