//! Potemkin: a generic, composable-plugin language-server proxy.
//!
//! It spawns a real language server as a child process and shuttles LSP messages
//! between the editor (stdin/stdout) and that server, running server->editor
//! responses through the plugin [`Pipeline`] so registered plugins can
//! pretty-print their library's types.
//!
//! Usage (as a drop-in server binary):
//!   Point your editor's server path at `potemkin` and set
//!   `POTEMKIN_SERVER=/path/to/your-language-server` (or a command on `PATH`).
//!   Potemkin is language-agnostic and has no default backend — the VS Code
//!   extension sets `POTEMKIN_SERVER` automatically for each server it wraps.

use anyhow::{anyhow, Result};
use log::{error, info, warn};
use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command as TokioCommand};

use potemkin_plugin_api::protocol::PluginManifest;
use potemkin_plugin_api::Plugin;
use potemkin_proxy::mux::Mux;
use potemkin_proxy::plugin::ProtocolPlugin;
use potemkin_proxy::{lsp, Config, Pipeline, RawConfig};

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    // Fast path for server-version probes: some editor extensions run the
    // configured server binary with `--version` before starting a session. Since
    // editors point at Potemkin, forward such probes to the real backend so they
    // see the wrapped server's version rather than hanging on the LSP pump.
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    if raw_args.iter().any(|a| a == "--version" || a == "-V") {
        match backend_server().and_then(|s| resolve_server(&s).ok()) {
            Some(path) => {
                let status = std::process::Command::new(path).args(&raw_args).status();
                std::process::exit(status.ok().and_then(|s| s.code()).unwrap_or(0));
            }
            None => {
                println!(
                    "potemkin {} (no backend configured; set POTEMKIN_SERVER)",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
        }
    }

    let config = read_config();
    let plugins = load_plugins(&config);
    let pipeline = Pipeline::new(plugins, config);
    info!(
        "Potemkin starting with plugins: [{}]",
        pipeline.plugin_names().join(", ")
    );

    // Discover the backend language server. Potemkin is language-agnostic and has
    // no default backend, so this must be set.
    let server = backend_server().ok_or_else(|| {
        anyhow!(
            "POTEMKIN_SERVER is not set. Potemkin is a language-agnostic proxy with \
             no default backend; set POTEMKIN_SERVER=/path/to/your-language-server \
             (the VS Code extension sets this automatically for each wrapped server)."
        )
    })?;
    let server_path = resolve_server(&server)?;
    info!("Backend language server: {server_path}");

    let args: Vec<String> = std::env::args().skip(1).collect();

    // The per-directory multiplexer is a last-resort, opt-in feature: it spawns
    // one backend per config scope, which is heavier than a single session (each
    // backend indexes its view of the tree). Off by default; the common path is a
    // fully transparent single-backend proxy.
    if multiplex_enabled() {
        info!("POTEMKIN_MULTIPLEX enabled: routing per-directory potemkin.toml scopes to separate backends");
        run_multiplexed(server_path, args, pipeline).await
    } else {
        run_passthrough(server_path, args, pipeline).await
    }
}

/// Whether the per-directory multiplexer is enabled (`POTEMKIN_MULTIPLEX`).
///
/// Off by default. This is a last-resort feature for servers that cannot yet do
/// per-directory configuration themselves (rust-analyzer's `rust-analyzer.toml`
/// is meant to fill this gap but is still incomplete). When off, Potemkin is a
/// transparent single-backend proxy and `potemkin.toml` files are ignored.
fn multiplex_enabled() -> bool {
    std::env::var("POTEMKIN_MULTIPLEX")
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        })
        .unwrap_or(false)
}

/// Transparent single-backend proxy: spawn the server once and shuttle LSP
/// messages between the editor and that backend, running server→editor responses
/// through the plugin [`Pipeline`]. This is the default (non-multiplexed) path.
async fn run_passthrough(server_path: String, args: Vec<String>, pipeline: Pipeline) -> Result<()> {
    let mut backend = spawn_backend(&server_path, &args)?;

    let mut b_stdin = backend.stdin.take().expect("backend stdin");
    let b_stdout = backend.stdout.take().expect("backend stdout");
    let b_stderr = backend.stderr.take().expect("backend stderr");

    // editor -> backend
    let pl = pipeline.clone();
    let editor_to_backend = tokio::spawn(async move {
        let mut reader = BufReader::new(tokio::io::stdin());
        let mut header = String::new();
        loop {
            match lsp::read_message(&mut reader, &mut header).await {
                Ok(Some(message)) => {
                    let out = pl.process_outgoing(&message);
                    if b_stdin.write_all(out.as_bytes()).await.is_err()
                        || b_stdin.flush().await.is_err()
                    {
                        error!("failed writing to backend");
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error!("failed reading from editor: {e}");
                    break;
                }
            }
        }
    });

    // backend -> editor (responses run through the plugin pipeline)
    let pl = pipeline.clone();
    let backend_to_editor = tokio::spawn(async move {
        let mut reader = BufReader::new(b_stdout);
        let mut stdout = tokio::io::stdout();
        let mut header = String::new();
        loop {
            match lsp::read_message(&mut reader, &mut header).await {
                Ok(Some(message)) => {
                    let out = pl.process_incoming(&message);
                    if stdout.write_all(out.as_bytes()).await.is_err()
                        || stdout.flush().await.is_err()
                    {
                        error!("failed writing to editor");
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    error!("failed reading from backend: {e}");
                    break;
                }
            }
        }
    });

    let stderr_forwarder = tokio::spawn(async move {
        let mut reader = BufReader::new(b_stderr);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => break,
                Ok(_) => eprint!("{line}"),
                Err(e) => {
                    error!("failed reading backend stderr: {e}");
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = editor_to_backend => info!("editor->backend task ended"),
        _ = backend_to_editor => info!("backend->editor task ended"),
        _ = stderr_forwarder => info!("stderr forwarder ended"),
    }

    if let Err(e) = backend.kill().await {
        warn!("failed to kill backend server: {e}");
    }
    info!("Potemkin shutting down");
    Ok(())
}

/// Per-directory multiplexed proxy: route LSP traffic between the editor and one
/// backend per `potemkin.toml` config scope. See [`Mux`].
async fn run_multiplexed(
    server_path: String,
    args: Vec<String>,
    pipeline: Pipeline,
) -> Result<()> {
    let (mux, mut editor_rx) = Mux::new(server_path, args, process_server_id(), pipeline);

    // editor writer: drain the mux's outgoing queue to stdout.
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(msg) = editor_rx.recv().await {
            if stdout.write_all(msg.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                error!("failed writing to editor");
                break;
            }
        }
    });

    // editor reader: dispatch editor stdin into the mux.
    let reader_mux = mux.clone();
    let reader = tokio::spawn(async move {
        let mut reader = BufReader::new(tokio::io::stdin());
        let mut header = String::new();
        loop {
            match lsp::read_message(&mut reader, &mut header).await {
                Ok(Some(message)) => reader_mux.on_editor_message(&message),
                Ok(None) => break,
                Err(e) => {
                    error!("failed reading from editor: {e}");
                    break;
                }
            }
        }
    });

    tokio::select! {
        _ = reader => info!("editor reader ended"),
        _ = writer => info!("editor writer ended"),
    }

    mux.shutdown();
    info!("Potemkin shutting down");
    Ok(())
}

/// Spawn the backend server with piped stdio and the environment it commonly
/// needs. Used by both the passthrough path and (indirectly) the multiplexer.
fn spawn_backend(path: &str, args: &[String]) -> Result<Child> {
    let mut cmd = TokioCommand::new(path);
    cmd.args(args)
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

/// Assemble the plugin set and order the transform chain deterministically.
///
/// Plugins are gathered (in-process first, then manifest-discovered) each paired
/// with an `order` hint, then sorted by [`ordered_indices`]: user override
/// (`POTEMKIN_PLUGIN_ORDER`) first, then the manifest `order`, then name. This
/// replaces the previous behavior where chain order was whatever `read_dir`
/// happened to return.
fn load_plugins(config: &Config) -> Vec<Box<dyn Plugin>> {
    // In-process plugins default to order 0 (they carry no manifest).
    let mut entries: Vec<(i32, Box<dyn Plugin>)> =
        registered_plugins().into_iter().map(|p| (0, p)).collect();
    entries.extend(discover_subprocess_plugins(config));

    let user_order = user_plugin_order();
    let keyed: Vec<(String, i32)> = entries
        .iter()
        .map(|(order, p)| (p.name().to_string(), *order))
        .collect();
    let order = ordered_indices(&keyed, &user_order);

    // Reorder `entries` by the computed permutation without cloning the plugins.
    let mut slots: Vec<Option<Box<dyn Plugin>>> = entries.into_iter().map(|(_, p)| Some(p)).collect();
    order
        .into_iter()
        .filter_map(|i| slots[i].take())
        .collect()
}

/// The user's explicit application order, from `POTEMKIN_PLUGIN_ORDER` (a
/// comma-separated list of plugin names, earliest first). The VS Code extension
/// sets this from the `potemkin.pluginOrder` setting. Names not listed keep
/// their manifest/name ordering *after* all listed ones. Unknown names are
/// harmless (they simply never match a loaded plugin).
fn user_plugin_order() -> Vec<String> {
    std::env::var("POTEMKIN_PLUGIN_ORDER")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Compute the chain order as a permutation of indices into `entries`
/// (`(name, order)` pairs). Sort key, ascending:
///   1. position in `user_order` (unlisted plugins sort last, as `usize::MAX`);
///   2. the manifest `order` hint (lower runs earlier);
///   3. the plugin name (deterministic tiebreak, replacing unstable `read_dir`).
fn ordered_indices(entries: &[(String, i32)], user_order: &[String]) -> Vec<usize> {
    let key = |e: &(String, i32)| -> (usize, i32, String) {
        let rank = user_order
            .iter()
            .position(|n| n == &e.0)
            .unwrap_or(usize::MAX);
        (rank, e.1, e.0.clone())
    };
    let mut idx: Vec<usize> = (0..entries.len()).collect();
    idx.sort_by(|&a, &b| key(&entries[a]).cmp(&key(&entries[b])));
    idx
}

/// In-process plugins compiled into this binary. First-party/bundled libraries
/// can register their (fastest) pretty-printers here.
fn registered_plugins() -> Vec<Box<dyn Plugin>> {
    Vec::new()
}

/// The ordered list of directories to scan for plugin manifests.
///
/// `POTEMKIN_PLUGINS_DIR` is treated as an OS-style path list (colon-separated
/// on Unix, semicolon on Windows), letting the VS Code extension point at both
/// its own managed directory and any user directory. The default user directory
/// (`~/.config/potemkin/plugins`) is always appended so `cargo install`-style
/// installs keep working. Earlier directories win on name conflicts.
fn plugin_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Ok(v) = std::env::var("POTEMKIN_PLUGINS_DIR") {
        dirs.extend(std::env::split_paths(&v));
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    dirs.push(PathBuf::from(format!("{home}/.config/potemkin/plugins")));

    let mut seen = HashSet::new();
    dirs.retain(|d| !d.as_os_str().is_empty() && seen.insert(d.clone()));
    dirs
}

/// The LSP `languageId`s the server this process wraps serves, taken from
/// `POTEMKIN_LANGUAGES` (comma-separated, e.g. `"rust"` or `"c,cpp"`). Empty
/// means "unknown" (e.g. the binary run directly, not via the extension launcher),
/// in which case language filtering is disabled and all plugins load.
fn process_languages() -> HashSet<String> {
    std::env::var("POTEMKIN_LANGUAGES")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Whether a plugin declaring `plugin_langs` should run in a process serving
/// `process_langs`. A plugin with no declared languages is language-agnostic and
/// always matches; if the process language is unknown we are permissive and load
/// everything. Otherwise the sets must intersect.
fn languages_match(plugin_langs: &[String], process_langs: &HashSet<String>) -> bool {
    if plugin_langs.is_empty() || process_langs.is_empty() {
        return true;
    }
    plugin_langs
        .iter()
        .any(|l| process_langs.contains(&l.to_ascii_lowercase()))
}

/// The id of the language server this process wraps, from `POTEMKIN_SERVER_ID`
/// (e.g. `"rust-analyzer"`). Empty means "unknown" (binary run directly).
fn process_server_id() -> Option<String> {
    std::env::var("POTEMKIN_SERVER_ID")
        .ok()
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

/// Whether a plugin declaring `plugin_servers` should run under `server_id`. A
/// plugin with no declared servers matches any server; if the server id is
/// unknown we are permissive. Otherwise the id must be listed.
fn server_matches(plugin_servers: &[String], server_id: &Option<String>) -> bool {
    match server_id {
        _ if plugin_servers.is_empty() => true,
        None => true,
        Some(id) => plugin_servers.iter().any(|s| s.to_ascii_lowercase() == *id),
    }
}

/// Discover subprocess plugins from `*.json` [`PluginManifest`] files across the
/// plugin search path (see [`plugin_dirs`]). The first manifest seen for a given
/// plugin name wins; later duplicates (e.g. a manual install shadowed by a
/// companion extension) are skipped. Plugins whose declared `languages` don't
/// match this process's wrapped-server language are skipped *before* spawning.
fn discover_subprocess_plugins(config: &Config) -> Vec<(i32, Box<dyn Plugin>)> {
    let mut plugins: Vec<(i32, Box<dyn Plugin>)> = Vec::new();
    let mut seen_names: HashSet<String> = HashSet::new();
    let process_langs = process_languages();
    let server_id = process_server_id();

    for dir in plugin_dirs() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue, // a missing dir on the search path is fine
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let manifest: PluginManifest = match std::fs::read_to_string(&path)
                .map_err(anyhow::Error::from)
                .and_then(|s| serde_json::from_str(&s).map_err(anyhow::Error::from))
            {
                Ok(m) => m,
                Err(e) => {
                    warn!("skipping plugin manifest {}: {e}", path.display());
                    continue;
                }
            };
            if !seen_names.insert(manifest.name.clone()) {
                info!(
                    "skipping duplicate plugin '{}' from {}",
                    manifest.name,
                    path.display()
                );
                continue;
            }
            if !languages_match(&manifest.languages, &process_langs) {
                info!(
                    "skipping plugin '{}' (languages {:?}) — not applicable to this server ({:?})",
                    manifest.name, manifest.languages, process_langs
                );
                continue;
            }
            if !server_matches(&manifest.language_servers, &server_id) {
                info!(
                    "skipping plugin '{}' (language_servers {:?}) — this server is {:?}",
                    manifest.name, manifest.language_servers, server_id
                );
                continue;
            }
            match ProtocolPlugin::spawn(
                &manifest,
                config.verbosity,
                config.unicode,
                config.plugin_timeout,
            ) {
                Ok(p) => {
                    info!("loaded plugin '{}' (order {})", manifest.name, manifest.order);
                    plugins.push((manifest.order, Box::new(p)));
                }
                Err(e) => warn!("failed to load plugin '{}': {e}", manifest.name),
            }
        }
    }
    plugins
}

#[cfg(test)]
mod tests {
    use super::*;

    fn langs(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn agnostic_plugin_always_matches() {
        assert!(languages_match(&[], &langs(&["rust"])));
        assert!(languages_match(&[], &HashSet::new()));
    }

    #[test]
    fn unknown_process_language_is_permissive() {
        assert!(languages_match(&["rust".into()], &HashSet::new()));
    }

    #[test]
    fn matches_only_on_intersection() {
        assert!(languages_match(&["rust".into()], &langs(&["rust"])));
        assert!(languages_match(
            &["c".into(), "cpp".into()],
            &langs(&["cpp"])
        ));
        assert!(!languages_match(&["rust".into()], &langs(&["cpp", "c"])));
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(languages_match(&["Rust".into()], &langs(&["rust"])));
    }

    #[test]
    fn any_server_when_unscoped_or_unknown() {
        assert!(server_matches(&[], &Some("rust-analyzer".into())));
        assert!(server_matches(&["rust-analyzer".into()], &None));
    }

    #[test]
    fn server_matches_only_listed() {
        assert!(server_matches(
            &["rust-analyzer".into()],
            &Some("rust-analyzer".into())
        ));
        assert!(!server_matches(
            &["rust-analyzer".into()],
            &Some("rust-rover".into())
        ));
    }

    #[test]
    fn server_matching_is_case_insensitive() {
        assert!(server_matches(
            &["Rust-Analyzer".into()],
            &Some("rust-analyzer".into())
        ));
    }

    // ---- plugin application ordering ----

    fn entries(items: &[(&str, i32)]) -> Vec<(String, i32)> {
        items.iter().map(|(n, o)| (n.to_string(), *o)).collect()
    }

    fn ordered_names(items: &[(&str, i32)], user: &[&str]) -> Vec<String> {
        let e = entries(items);
        let u: Vec<String> = user.iter().map(|s| s.to_string()).collect();
        ordered_indices(&e, &u)
            .into_iter()
            .map(|i| e[i].0.clone())
            .collect()
    }

    #[test]
    fn order_defaults_to_name_when_no_hints() {
        // No manifest order, no user override: deterministic alphabetical, which
        // fixes the previous unstable read_dir behavior.
        let out = ordered_names(&[("nholthaus-units", 0), ("clangd-x", 0), ("aardvark", 0)], &[]);
        assert_eq!(out, vec!["aardvark", "clangd-x", "nholthaus-units"]);
    }

    #[test]
    fn manifest_order_beats_name() {
        // Lower order runs earlier regardless of name.
        let out = ordered_names(
            &[("typed-linear-algebra", 10), ("nholthaus-units", 0)],
            &[],
        );
        assert_eq!(out, vec!["nholthaus-units", "typed-linear-algebra"]);
    }

    #[test]
    fn order_ties_break_by_name() {
        let out = ordered_names(&[("b", 5), ("a", 5), ("c", 5)], &[]);
        assert_eq!(out, vec!["a", "b", "c"]);
    }

    #[test]
    fn user_override_wins_over_manifest_order() {
        // User forces TLA first even though its manifest order is higher.
        let out = ordered_names(
            &[("nholthaus-units", 0), ("typed-linear-algebra", 10)],
            &["typed-linear-algebra", "nholthaus-units"],
        );
        assert_eq!(out, vec!["typed-linear-algebra", "nholthaus-units"]);
    }

    #[test]
    fn user_override_partial_lists_rest_after() {
        // Only one name listed: it goes first; the rest follow by (order, name).
        let out = ordered_names(
            &[("z", 0), ("a", 0), ("nholthaus-units", 5)],
            &["nholthaus-units"],
        );
        assert_eq!(out, vec!["nholthaus-units", "a", "z"]);
    }

    #[test]
    fn unknown_user_names_are_ignored() {
        let out = ordered_names(&[("a", 0), ("b", 0)], &["ghost", "b"]);
        assert_eq!(out, vec!["b", "a"]);
    }
}

fn read_config() -> Config {
    let verbosity = std::env::var("POTEMKIN_VERBOSITY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let unicode = std::env::var("POTEMKIN_UNICODE")
        .map(|v| v != "false" && v != "0")
        .unwrap_or(true);
    let plugin_timeout = std::env::var("POTEMKIN_PLUGIN_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(std::time::Duration::from_millis)
        .unwrap_or(potemkin_proxy::DEFAULT_PLUGIN_TIMEOUT);
    Config {
        verbosity,
        unicode,
        raw: read_raw_config(),
        plugin_timeout,
    }
}

/// Read the per-server raw-section rendering from `POTEMKIN_RAW_*`.
///
/// The VS Code extension sets these per wrapped server (from the user's
/// `potemkin.raw` config), so the presentation matches that server's hover
/// format. Run standalone, raw is off unless `POTEMKIN_RAW` is set.
fn read_raw_config() -> RawConfig {
    let enabled = env_truthy("POTEMKIN_RAW");
    let label = std::env::var("POTEMKIN_RAW_LABEL").unwrap_or_else(|_| "Raw:".to_string());
    let fence = std::env::var("POTEMKIN_RAW_FENCE")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let separator = env_truthy("POTEMKIN_RAW_SEPARATOR");
    RawConfig {
        enabled,
        label,
        fence,
        separator,
    }
}

/// Whether an env var is set to a truthy value (`1`/`true`/`yes`/`on`).
fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            v == "1" || v == "true" || v == "yes" || v == "on"
        })
        .unwrap_or(false)
}

/// The backend language server command from `POTEMKIN_SERVER`, or `None` if it
/// is unset/blank. Potemkin is language-agnostic and deliberately has no default
/// backend, so callers must decide how to handle the missing case.
fn backend_server() -> Option<String> {
    std::env::var("POTEMKIN_SERVER")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

/// Resolve a server command to a runnable path, trying `--version` to confirm.
fn resolve_server(server: &str) -> Result<String> {
    // If it's an absolute/relative path that exists, use it directly.
    if server.contains('/') && std::path::Path::new(server).exists() {
        return Ok(server.to_string());
    }

    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    let candidates = [
        server.to_string(),
        format!("{home}/.cargo/bin/{server}"),
        format!("{home}/.rustup/toolchains/stable-aarch64-apple-darwin/bin/{server}"),
        format!("{home}/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin/{server}"),
    ];

    for candidate in candidates {
        if std::process::Command::new(&candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            return Ok(candidate);
        }
    }

    Err(anyhow::anyhow!(
        "backend language server '{server}' not found on PATH; set POTEMKIN_SERVER"
    ))
}
