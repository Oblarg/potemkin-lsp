import * as fs from "fs";
import * as path from "path";
import * as vscode from "vscode";

/**
 * How to wrap one language server. Potemkin is generic over the language server:
 * it never hard-codes rust-analyzer. Each entry describes the editor-side hook a
 * given server exposes.
 */
interface ServerDef {
  /** Stable id, e.g. "rust-analyzer". */
  id: string;
  /** Human label. */
  label: string;
  /** The VS Code extension that owns this server (used for auto-detection). */
  extensionId?: string;
  /**
   * Dotted config path whose value overrides the server binary,
   * e.g. "rust-analyzer.server.path" or "clangd.path". The first segment is the
   * configuration section; the remainder is the key.
   */
  pathSetting: string;
  /**
   * If the setting is an *object* (e.g. go's "go.alternateTools"), the field
   * within it that holds the server path (e.g. "gopls").
   */
  pathSettingField?: string;
  /** Command that restarts the server after we change its path. */
  restartCommand?: string;
  /** The real server command to wrap when the user hasn't set an explicit path. */
  defaultCommand: string;
  /**
   * LSP `languageId`s this server handles, forwarded to the proxy as
   * `POTEMKIN_LANGUAGES` so it only loads plugins that declare a matching
   * language. This is what stops a Rust-only plugin from being spawned under a
   * C++ or Go server.
   */
  languages: string[];
}

const BUILT_IN: ServerDef[] = [
  {
    id: "rust-analyzer",
    label: "rust-analyzer",
    extensionId: "rust-lang.rust-analyzer",
    pathSetting: "rust-analyzer.server.path",
    restartCommand: "rust-analyzer.restartServer",
    defaultCommand: "rust-analyzer",
    languages: ["rust"],
  },
  {
    id: "clangd",
    label: "clangd",
    extensionId: "llvm-vs-code-extensions.vscode-clangd",
    pathSetting: "clangd.path",
    restartCommand: "clangd.restart",
    defaultCommand: "clangd",
    languages: ["c", "cpp", "objective-c", "objective-cpp", "cuda"],
  },
  {
    id: "gopls",
    label: "gopls",
    extensionId: "golang.go",
    pathSetting: "go.alternateTools",
    pathSettingField: "gopls",
    restartCommand: "go.languageserver.restart",
    defaultCommand: "gopls",
    languages: ["go", "gomod", "gotmpl", "gowork"],
  },
];

const savedKey = (def: ServerDef) => `potemkin.saved.${def.id}`;

/**
 * How the proxy renders the "Raw:" section it appends below a pretty-printed
 * hover. This is a property of the *language server's* hover format (markdown vs
 * plaintext, which code fence), so it is configured by the user per server via
 * `potemkin.raw` and baked into that server's launcher — it does not vary per
 * plugin.
 */
interface RawRender {
  enabled: boolean;
  label: string;
  /** Code-fence language (e.g. "rust"); "" means plaintext (no fence). */
  fence: string;
  separator: boolean;
}

/**
 * Sensible built-in defaults per server. rust-analyzer/gopls emit markdown
 * hovers (so a code fence renders nicely); clangd emits plaintext (no fence).
 */
const RAW_DEFAULTS: Record<string, RawRender> = {
  "rust-analyzer": { enabled: true, label: "Raw:", fence: "rust", separator: true },
  clangd: { enabled: true, label: "Raw:", fence: "", separator: false },
  gopls: { enabled: true, label: "Raw:", fence: "go", separator: true },
};

/** Fallback for servers without a specific default (assume plaintext). */
const RAW_FALLBACK: RawRender = { enabled: true, label: "Raw:", fence: "", separator: false };

/** Resolve the raw-render config for a server: user override over built-in default. */
function rawConfigFor(def: ServerDef): RawRender {
  const base = RAW_DEFAULTS[def.id] ?? RAW_FALLBACK;
  const all =
    vscode.workspace.getConfiguration("potemkin").get<Record<string, Partial<RawRender>>>("raw", {}) ??
    {};
  const override = all[def.id] ?? {};
  return { ...base, ...override };
}

/**
 * The public API surface Potemkin exposes from `activate()`. Companion
 * extensions (one per library) call this to register their plugin binary
 * without ever knowing where Potemkin keeps its manifests. Retrieve it with:
 *
 * ```ts
 * const ext = vscode.extensions.getExtension("potemkin-lsp.potemkin");
 * const api: PotemkinApi = await ext.activate();
 * await api.registerPlugin({ name, markers, command });
 * ```
 */
export interface PotemkinApi {
  /** Bumped on breaking API changes so companions can feature-detect. */
  readonly apiVersion: number;
  /** The directory where Potemkin writes companion-registered manifests. */
  readonly pluginsDir: string;
  /**
   * Register (or refresh) a plugin — native (`command`), JavaScript (`js`), or
   * WebAssembly (`wasm`). Idempotent per `name`: calling again overwrites the
   * previous manifest, so companions can safely re-register on every activation.
   * Triggers a (debounced) restart of wrapped servers so Potemkin reloads
   * plugins.
   */
  registerPlugin(plugin: PluginRegistration): Promise<void>;
  /** Remove a previously-registered plugin by `name`. */
  unregisterPlugin(name: string): Promise<void>;
}

export interface PluginRegistration {
  /** Stable plugin id, e.g. "whippyunits". Used as the manifest filename. */
  name: string;
  /**
   * Absolute path to a native plugin executable (the subprocess transport).
   * Provide exactly one of `command`, `js`, or `wasm`.
   */
  command?: string;
  /**
   * Absolute path to a `.js` plugin (the JS transport). Runs with the editor's
   * bundled Node — no separate Node install needed.
   */
  js?: string;
  /**
   * Absolute path to a `.wasm` plugin (the WASM transport). Runs in the editor's
   * Node via the bundled extism JS SDK harness — one artifact, runs everywhere.
   */
  wasm?: string;
  /** Fast-path substrings; if none match a payload the plugin is skipped. */
  markers?: string[];
  /**
   * LSP `languageId`s this plugin applies to (e.g. `["rust"]`). Omit or leave
   * empty for a language-agnostic plugin. Potemkin only loads the plugin under a
   * server whose language matches, so a Rust plugin never runs under clangd/gopls.
   */
  languages?: string[];
  /**
   * Specific language-server ids this plugin supports (e.g. `["rust-analyzer"]`).
   * Finer-grained than `languages`: use it when the plugin only handles a
   * particular server's output format. Omit/empty means any server for the
   * matching language(s).
   */
  languageServers?: string[];
  /** Extra arguments passed to the command. */
  args?: string[];
  /**
   * Application-order hint for the transform chain. Lower runs earlier; plugins
   * that rewrite the same text run in sequence (each sees the previous output).
   * Use a higher value when this plugin must run *after* another — e.g. a matrix
   * pretty-printer (order 10) that needs a units plugin (order 0) to render the
   * element types first. Defaults to 0. The user can override globally via the
   * `potemkin.pluginOrder` setting.
   */
  order?: number;
  /**
   * Per-plugin timeout override, in milliseconds, for one `initialize`/`transform`
   * round trip. If a call exceeds it, Potemkin kills the plugin and drops it for
   * the session (text passes through untouched), so a hung/slow plugin can't
   * stall hovers or inlay hints. Omit to use the global `potemkin.pluginTimeoutMs`
   * setting (or Potemkin's built-in default).
   */
  timeoutMs?: number;
  /**
   * The registering companion's extension id (e.g. `"whippyunits.whippyunits-potemkin"`,
   * typically `context.extension.id`). Recorded in the manifest so Potemkin can
   * drop the plugin automatically when that extension is later **disabled or
   * uninstalled** (see prune-on-activation). Omit only for hand-maintained
   * manifests you want to persist regardless of any extension.
   */
  owner?: string;
}

let reconcileTimer: ReturnType<typeof setTimeout> | undefined;

export async function activate(context: vscode.ExtensionContext): Promise<PotemkinApi> {
  context.subscriptions.push(
    vscode.commands.registerCommand("potemkin.enable", () => enableAll(context)),
    vscode.commands.registerCommand("potemkin.disable", () => disableAll(context)),
    vscode.commands.registerCommand("potemkin.restart", () => restartAll(context)),
    vscode.commands.registerCommand("potemkin.wrapServer", () => pickAndWrap(context)),
    vscode.commands.registerCommand("potemkin.status", () => showStatus(context)),
    vscode.commands.registerCommand("potemkin.showPlugins", () => showPlugins(context)),
    vscode.commands.registerCommand("potemkin.showBinaryPath", () =>
      vscode.window.showInformationMessage(`Potemkin binary: ${bundledBinaryPath(context)}`),
    ),
  );

  // Ensure the managed plugins dir exists so the launcher can point at it even
  // before any companion registers.
  fs.mkdirSync(managedPluginsDir(context), { recursive: true });

  // Drop manifests whose owning companion extension is gone (disabled or
  // uninstalled) *before* wrapping, so disabling a companion actually stops its
  // pretty-printing on the next reload. Enabled companions re-register right
  // after (idempotently), so this only removes genuinely orphaned plugins.
  pruneOrphanedPlugins(context);

  const enabled = vscode.workspace.getConfiguration("potemkin").get<boolean>("enabled", true);
  if (enabled) {
    await enableAll(context, { silent: true });
  }

  return makeApi(context);
}

// ---------------------------------------------------------------------------
// Public API for companion extensions
// ---------------------------------------------------------------------------

function managedPluginsDir(context: vscode.ExtensionContext): string {
  return path.join(context.globalStorageUri.fsPath, "plugins");
}

/**
 * Remove managed manifests whose recorded `owner` extension is no longer present
 * (disabled or uninstalled — `getExtension` returns `undefined` for both). This
 * makes disabling a companion actually stop its pretty-printing after a reload,
 * without the companion needing to (unreliably) unregister from its own
 * `deactivate`. Manifests with no `owner` (hand-maintained) are left untouched.
 */
function pruneOrphanedPlugins(context: vscode.ExtensionContext): void {
  const dir = managedPluginsDir(context);
  let files: string[] = [];
  try {
    files = fs.readdirSync(dir).filter((f) => f.endsWith(".json"));
  } catch {
    return; // dir may not exist yet
  }

  let removed = false;
  for (const f of files) {
    const file = path.join(dir, f);
    let owner: unknown;
    try {
      owner = JSON.parse(fs.readFileSync(file, "utf8"))?.owner;
    } catch {
      continue; // leave unparseable files alone
    }
    if (typeof owner === "string" && owner && !vscode.extensions.getExtension(owner)) {
      try {
        fs.rmSync(file);
        removed = true;
      } catch {
        /* non-fatal */
      }
    }
  }

  if (removed) scheduleReconcile(context);
}

// ---------------------------------------------------------------------------
// Plugin-applicability gating
//
// A server is only worth wrapping if at least one installed + enabled plugin
// applies to it; otherwise inserting Potemkin would spawn an idle pass-through
// proxy for no benefit. These helpers answer "does this server have a matching
// plugin?" by scanning the same manifest dirs the proxy discovers, applying the
// same language / language-server filter the proxy uses.
// ---------------------------------------------------------------------------

/**
 * The plugin search directories, mirroring the proxy's discovery order (see
 * `plugin_dirs` in the proxy): the user's `potemkin.pluginsDir` (if set), the
 * extension-managed dir (companion manifests), and the default
 * `~/.config/potemkin/plugins`.
 */
function pluginSearchDirs(context: vscode.ExtensionContext): string[] {
  const userDir = vscode.workspace
    .getConfiguration("potemkin")
    .get<string>("pluginsDir", "")
    .trim();
  const home = process.env.HOME || process.env.USERPROFILE || "";
  const dirs = [userDir, managedPluginsDir(context)];
  if (home) dirs.push(path.join(home, ".config", "potemkin", "plugins"));
  return dirs.filter(Boolean);
}

/**
 * Whether a manifest applies to `def`, mirroring the proxy's `languages_match`
 * && `server_matches`: an empty `languages`/`language_servers` list is
 * permissive, otherwise it must intersect the server's languages / include its
 * id (case-insensitively).
 */
function pluginMatchesServer(manifest: unknown, def: ServerDef): boolean {
  const m = (manifest ?? {}) as { languages?: unknown; language_servers?: unknown };
  const langs = Array.isArray(m.languages) ? m.languages.map((s) => String(s).toLowerCase()) : [];
  const servers = Array.isArray(m.language_servers)
    ? m.language_servers.map((s) => String(s).toLowerCase())
    : [];
  const defLangs = (def.languages ?? []).map((s) => s.toLowerCase());
  const languagesMatch =
    langs.length === 0 || defLangs.length === 0 || langs.some((l) => defLangs.includes(l));
  const serverMatch = servers.length === 0 || servers.includes(def.id.toLowerCase());
  return languagesMatch && serverMatch;
}

/**
 * Whether any installed + enabled plugin applies to `def`. Scans the manifest
 * dirs the proxy would, de-duplicating by plugin name (earlier dirs win, as in
 * the proxy) so a shadowed manifest can't produce a false positive. A server
 * with no matching plugin is never wrapped, so it spawns no Potemkin process.
 */
function serverHasPlugins(context: vscode.ExtensionContext, def: ServerDef): boolean {
  const seen = new Set<string>();
  for (const dir of pluginSearchDirs(context)) {
    let files: string[];
    try {
      files = fs.readdirSync(dir).filter((f) => f.endsWith(".json"));
    } catch {
      continue; // a missing dir on the search path is fine
    }
    for (const f of files) {
      let manifest: unknown;
      try {
        manifest = JSON.parse(fs.readFileSync(path.join(dir, f), "utf8"));
      } catch {
        continue; // leave unparseable files alone
      }
      const named = manifest as { name?: unknown };
      const name = typeof named?.name === "string" ? named.name : f;
      if (seen.has(name)) continue;
      seen.add(name);
      if (pluginMatchesServer(manifest, def)) return true;
    }
  }
  return false;
}

function makeApi(context: vscode.ExtensionContext): PotemkinApi {
  return {
    // v2: registerPlugin accepts `js`/`wasm` in addition to `command`.
    apiVersion: 2,
    pluginsDir: managedPluginsDir(context),
    async registerPlugin(plugin: PluginRegistration): Promise<void> {
      if (!plugin?.name) {
        throw new Error("potemkin.registerPlugin requires { name }");
      }
      const dir = managedPluginsDir(context);
      fs.mkdirSync(dir, { recursive: true });
      const manifest: Record<string, unknown> = {
        name: plugin.name,
        markers: plugin.markers ?? [],
        languages: plugin.languages ?? [],
        language_servers: plugin.languageServers ?? [],
        order: plugin.order ?? 0,
        transport: pluginTransport(plugin),
      };
      // Per-plugin timeout override (ms). Omitted unless the author set it, so
      // the proxy falls back to the global default.
      if (typeof plugin.timeoutMs === "number" && plugin.timeoutMs > 0) {
        manifest.timeout_ms = Math.floor(plugin.timeoutMs);
      }
      // Record the owning companion so Potemkin can auto-prune this manifest when
      // that extension is disabled/uninstalled. (The proxy ignores this field.)
      if (plugin.owner) manifest.owner = plugin.owner;
      const file = path.join(dir, `${sanitize(plugin.name)}.json`);
      const next = JSON.stringify(manifest, null, 2);
      // Avoid a needless server restart if nothing actually changed.
      const prev = fs.existsSync(file) ? fs.readFileSync(file, "utf8") : "";
      if (prev.trim() === next.trim()) return;
      fs.writeFileSync(file, next);
      scheduleReconcile(context);
    },
    async unregisterPlugin(name: string): Promise<void> {
      const file = path.join(managedPluginsDir(context), `${sanitize(name)}.json`);
      if (fs.existsSync(file)) {
        fs.rmSync(file);
        scheduleReconcile(context);
      }
    },
  };
}

/**
 * Debounce reconciliation so a burst of companion (un)registrations at startup
 * coalesces into a single pass. Reconciling — not just restarting — is required
 * because the applicable plugin set may have changed: a server that just gained
 * its first matching plugin must now be wrapped, and one that lost its last must
 * be unwrapped so it stops spawning a Potemkin process. `enableAll` performs
 * that wrap/unwrap and restarts the affected servers.
 */
function scheduleReconcile(context: vscode.ExtensionContext): void {
  if (reconcileTimer) clearTimeout(reconcileTimer);
  reconcileTimer = setTimeout(() => {
    reconcileTimer = undefined;
    void enableAll(context, { silent: true });
  }, 800);
}

function sanitize(name: string): string {
  return name.replace(/[^A-Za-z0-9._-]/g, "_");
}

/**
 * Build the manifest `transport` object from a registration, requiring exactly
 * one of `command` (native subprocess), `js`, or `wasm`.
 */
function pluginTransport(plugin: PluginRegistration): Record<string, unknown> {
  const chosen = [plugin.command, plugin.js, plugin.wasm].filter(Boolean).length;
  if (chosen !== 1) {
    throw new Error(
      "potemkin.registerPlugin requires exactly one of { command, js, wasm }",
    );
  }
  if (plugin.wasm) {
    return { type: "wasm", path: plugin.wasm };
  }
  if (plugin.js) {
    return { type: "js", path: plugin.js, args: plugin.args ?? [] };
  }
  return { type: "subprocess", command: plugin.command, args: plugin.args ?? [] };
}

export async function deactivate(): Promise<void> {
  // Leave overrides in place across sessions; use "Potemkin: Disable" to restore.
}

// ---------------------------------------------------------------------------
// High-level actions
// ---------------------------------------------------------------------------

interface Opts {
  silent?: boolean;
}

async function enableAll(context: vscode.ExtensionContext, opts: Opts = {}): Promise<void> {
  const bin = bundledBinaryPath(context);
  if (!fs.existsSync(bin)) {
    vscode.window.showErrorMessage(
      `Potemkin: no bundled binary for this platform (${platformDir()}). Expected at ${bin}.`,
    );
    return;
  }

  // Reconcile wrapping against the *applicable* plugin set: wrap an installed
  // server only if some plugin matches it; otherwise make sure it's left (or
  // put back) unwrapped, so a server with no plugins spawns no Potemkin process.
  const installed = relevantServers(context);
  const installedIds = new Set(installed.map((d) => d.id));
  const wrapped: string[] = [];
  const skipped: string[] = [];
  for (const def of installed) {
    if (serverHasPlugins(context, def)) {
      if (await wrapServer(context, def)) wrapped.push(def.label);
    } else {
      await unwrapIfNeeded(context, def);
      skipped.push(def.label);
    }
  }

  // Restore any server we previously wrapped that is no longer installed (its
  // owning extension was removed), so we never leave a dangling launcher path.
  const byId = new Map([...BUILT_IN, ...userServers()].map((d) => [d.id, d] as const));
  for (const key of context.workspaceState.keys()) {
    const m = /^potemkin\.saved\.(.+)$/.exec(key);
    if (!m || installedIds.has(m[1])) continue;
    const def = byId.get(m[1]);
    if (def) await unwrapIfNeeded(context, def);
  }

  if (!opts.silent) {
    if (wrapped.length) {
      vscode.window.showInformationMessage(`Potemkin: now wrapping ${wrapped.join(", ")}.`);
    } else if (skipped.length) {
      vscode.window.showInformationMessage(
        `Potemkin: ${skipped.join(
          ", ",
        )} installed but no matching plugins, so left unwrapped. Install a library's Potemkin companion to enable pretty-printing.`,
      );
    } else {
      vscode.window.showInformationMessage(
        "Potemkin: no supported language servers detected. Use \u201CPotemkin: Wrap a language server\u201D to choose one.",
      );
    }
  }
}

/**
 * Restore `def` only if we currently wrap it (or hold saved state for it), so a
 * reconcile never restarts a server we weren't already touching.
 */
async function unwrapIfNeeded(context: vscode.ExtensionContext, def: ServerDef): Promise<boolean> {
  const wrappedNow = extractPath(def, readSetting(def)) === launcherPath(context, def);
  const hasSaved = context.workspaceState.get(savedKey(def)) !== undefined;
  if (!wrappedNow && !hasSaved) return false;
  await unwrapServer(context, def);
  return true;
}

async function disableAll(context: vscode.ExtensionContext): Promise<void> {
  // Unwrap anything we have saved state for, plus any currently-relevant server.
  const byId = new Map<string, ServerDef>();
  for (const def of [...BUILT_IN, ...userServers()]) {
    byId.set(def.id, def);
  }
  const ids = new Set<string>();
  for (const def of relevantServers(context)) ids.add(def.id);
  for (const key of context.workspaceState.keys()) {
    const m = /^potemkin\.saved\.(.+)$/.exec(key);
    if (m) ids.add(m[1]);
  }

  for (const id of ids) {
    const def = byId.get(id);
    if (def) await unwrapServer(context, def);
  }
  vscode.window.showInformationMessage("Potemkin: restored original language-server settings.");
}

async function restartAll(context: vscode.ExtensionContext): Promise<void> {
  for (const def of relevantServers(context)) {
    await runRestart(def);
  }
}

async function pickAndWrap(context: vscode.ExtensionContext): Promise<void> {
  const all = [...BUILT_IN, ...userServers()];
  const pick = await vscode.window.showQuickPick(
    all.map((d) => ({ label: d.label, description: d.id, def: d })),
    { placeHolder: "Which language server should Potemkin wrap?" },
  );
  if (!pick) return;
  // Keep the invariant that a server is only wrapped when a plugin applies —
  // otherwise the wrap would just add an idle pass-through proxy (and the next
  // reconcile would undo it anyway).
  if (!serverHasPlugins(context, pick.def)) {
    vscode.window.showInformationMessage(
      `Potemkin: ${pick.def.label} has no matching plugins installed, so wrapping it would add an idle proxy. Install a companion plugin for its language first.`,
    );
    return;
  }
  if (await wrapServer(context, pick.def)) {
    vscode.window.showInformationMessage(`Potemkin: now wrapping ${pick.def.label}.`);
  }
}

async function showStatus(context: vscode.ExtensionContext): Promise<void> {
  const lines: string[] = [];
  for (const def of [...BUILT_IN, ...userServers()]) {
    const current = extractPath(def, readSetting(def));
    const ours = current === launcherPath(context, def);
    const installed = !def.extensionId || !!vscode.extensions.getExtension(def.extensionId);
    lines.push(`${ours ? "\u2713" : "\u25CB"} ${def.label}${installed ? "" : " (extension not installed)"}`);
  }
  vscode.window.showInformationMessage("Potemkin status:\n" + lines.join("\n"), { modal: true });
}

async function showPlugins(context: vscode.ExtensionContext): Promise<void> {
  const dir = managedPluginsDir(context);
  let names: string[] = [];
  try {
    names = fs
      .readdirSync(dir)
      .filter((f) => f.endsWith(".json"))
      .map((f) => {
        try {
          const m = JSON.parse(fs.readFileSync(path.join(dir, f), "utf8"));
          return typeof m?.name === "string" ? m.name : f.replace(/\.json$/, "");
        } catch {
          return f.replace(/\.json$/, "");
        }
      });
  } catch {
    /* dir may not exist yet */
  }
  const body = names.length
    ? names.map((n) => `\u2713 ${n}`).join("\n")
    : "No plugins registered yet. Install a library's Potemkin companion extension to add one.";
  vscode.window.showInformationMessage(
    `Potemkin plugins (managed):\n${body}\n\nDir: ${dir}`,
    { modal: true },
  );
}

// ---------------------------------------------------------------------------
// Wrap / unwrap a single server
// ---------------------------------------------------------------------------

async function wrapServer(context: vscode.ExtensionContext, def: ServerDef): Promise<boolean> {
  const bin = bundledBinaryPath(context);
  if (!fs.existsSync(bin)) return false;
  ensureExecutable(bin);

  const target = configTarget();
  const launcher = launcherPath(context, def);
  const currentValue = readSetting(def);
  const currentPath = extractPath(def, currentValue);
  const alreadyOurs = currentPath === launcher;

  // Record the pristine original setting value once, so disable restores it.
  // Guard against a stale Potemkin path (e.g. left by an older version that set
  // the server path directly): never treat that as the user's real server.
  if (context.workspaceState.get(savedKey(def)) === undefined && !alreadyOurs) {
    const original = looksLikePotemkin(currentPath, launcher) ? null : (currentValue ?? null);
    await context.workspaceState.update(savedKey(def), { value: original });
  }

  // Determine the real server command to wrap.
  const saved = context.workspaceState.get<{ value: unknown }>(savedKey(def));
  const savedPath = extractPath(def, saved?.value);
  const realCmd =
    savedPath && !looksLikePotemkin(savedPath, launcher) ? savedPath : def.defaultCommand;

  writeLauncher(context, def, realCmd);
  await writeSettingPath(def, launcher, target);
  await runRestart(def);
  return true;
}

async function unwrapServer(context: vscode.ExtensionContext, def: ServerDef): Promise<void> {
  const target = configTarget();
  const saved = context.workspaceState.get<{ value: unknown }>(savedKey(def));

  if (saved !== undefined) {
    // Restore the whole original value (scalar, object, or absent).
    await writeSettingRaw(def, saved.value ?? undefined, target);
    await context.workspaceState.update(savedKey(def), undefined);
  } else if (extractPath(def, readSetting(def)) === launcherPath(context, def)) {
    // No saved state but currently ours: just clear our launcher.
    await writeSettingPath(def, undefined, target);
  }
  await runRestart(def);
}

async function runRestart(def: ServerDef): Promise<void> {
  if (!def.restartCommand) return;
  const commands = await vscode.commands.getCommands(true);
  if (commands.includes(def.restartCommand)) {
    await vscode.commands.executeCommand(def.restartCommand);
  }
  // If the restart command isn't available yet, the new path is picked up on the
  // next server start / window reload; we avoid nagging with a reload prompt.
}

// ---------------------------------------------------------------------------
// Setting access (handles both scalar and object-field settings)
// ---------------------------------------------------------------------------

function splitSetting(dotted: string): { section: string; key: string } {
  const idx = dotted.indexOf(".");
  return { section: dotted.slice(0, idx), key: dotted.slice(idx + 1) };
}

function readSetting(def: ServerDef): unknown {
  const { section, key } = splitSetting(def.pathSetting);
  return vscode.workspace.getConfiguration(section).get(key);
}

/** Extract the server-path string from a raw setting value. */
function extractPath(def: ServerDef, value: unknown): string | undefined {
  if (def.pathSettingField) {
    if (value && typeof value === "object") {
      const v = (value as Record<string, unknown>)[def.pathSettingField];
      return typeof v === "string" ? v : undefined;
    }
    return undefined;
  }
  return typeof value === "string" ? value : undefined;
}

/** Set just the server-path (merging into the object for object-field settings). */
async function writeSettingPath(
  def: ServerDef,
  value: string | undefined,
  target: vscode.ConfigurationTarget,
): Promise<void> {
  const { section, key } = splitSetting(def.pathSetting);
  const cfg = vscode.workspace.getConfiguration(section);
  if (def.pathSettingField) {
    const existing = { ...((cfg.get(key) as Record<string, unknown>) ?? {}) };
    if (value === undefined) delete existing[def.pathSettingField];
    else existing[def.pathSettingField] = value;
    await cfg.update(key, Object.keys(existing).length ? existing : undefined, target);
  } else {
    await cfg.update(key, value, target);
  }
}

/** Restore an entire raw setting value (used on disable). */
async function writeSettingRaw(
  def: ServerDef,
  value: unknown,
  target: vscode.ConfigurationTarget,
): Promise<void> {
  const { section, key } = splitSetting(def.pathSetting);
  await vscode.workspace.getConfiguration(section).update(key, value ?? undefined, target);
}

// ---------------------------------------------------------------------------
// Launcher shim generation
// ---------------------------------------------------------------------------

function launcherPath(context: vscode.ExtensionContext, def: ServerDef): string {
  const name = process.platform === "win32" ? `potemkin-${def.id}.cmd` : `potemkin-${def.id}.sh`;
  return path.join(context.globalStorageUri.fsPath, "launchers", name);
}

/**
 * Write a tiny launcher that sets POTEMKIN_* and execs the bundled binary. This
 * lets us wrap *any* server whose extension exposes a path override, without
 * depending on that extension having an env-passthrough setting.
 */
function writeLauncher(context: vscode.ExtensionContext, def: ServerDef, realCmd: string): void {
  const p = launcherPath(context, def);
  fs.mkdirSync(path.dirname(p), { recursive: true });

  const bin = bundledBinaryPath(context);
  const cfg = vscode.workspace.getConfiguration("potemkin");
  const userPluginsDir = cfg.get<string>("pluginsDir", "").trim();
  const verbosity = String(cfg.get<number>("verbosity", 0));
  const unicode = String(cfg.get<boolean>("unicode", true));
  // Per-directory config multiplexer: off by default (last-resort feature that
  // runs one backend per potemkin.toml scope). Only emit the env var when on.
  const multiplex = cfg.get<boolean>("multiplex", false);

  // Always expose the extension-managed dir (where companion extensions drop
  // their manifests). The user's optional dir is prepended so it can shadow
  // managed plugins. The potemkin binary also always appends its own default
  // (~/.config/potemkin/plugins) for `cargo install`-style fallbacks.
  const pluginDirs = [userPluginsDir, managedPluginsDir(context)]
    .filter(Boolean)
    .join(path.delimiter);
  const languages = (def.languages ?? []).join(",");

  // Optional user-controlled plugin application order (earliest first). Overrides
  // each plugin's manifest `order`; unlisted plugins keep their manifest/name
  // ordering after the listed ones. Only emitted when the user sets it.
  const pluginOrder = (cfg.get<string[]>("pluginOrder", []) ?? [])
    .map((s) => String(s).trim())
    .filter(Boolean)
    .join(",");

  // Global default per-plugin timeout (ms) for one initialize/transform round
  // trip; a hung/slow plugin is killed and dropped rather than stalling hovers.
  // A plugin's manifest `timeout_ms` overrides this. Only emitted when the user
  // sets a positive value; otherwise the proxy uses its built-in default.
  const pluginTimeoutMsRaw = cfg.get<number>("pluginTimeoutMs", 0);
  const pluginTimeout =
    typeof pluginTimeoutMsRaw === "number" && pluginTimeoutMsRaw > 0
      ? String(Math.floor(pluginTimeoutMsRaw))
      : "";

  // JS and WASM plugins run in a Node runtime. Hand the proxy the editor's own
  // bundled Node (this extension host's executable, run with
  // ELECTRON_RUN_AS_NODE=1 by the proxy) so plugin authors need no separate Node
  // install, and point it at the bundled WASM harness so `.wasm` plugins run in
  // the VS Code runtime via the extism JS SDK.
  const node = process.execPath;
  const wasmHost = wasmHostPath(context);
  const haveWasmHost = fs.existsSync(wasmHost);

  // Per-server "Raw:" section rendering (user config over built-in defaults).
  const raw = rawConfigFor(def);

  if (process.platform === "win32") {
    const lines = [
      "@echo off",
      `set "POTEMKIN_SERVER=${realCmd}"`,
      `set "POTEMKIN_SERVER_ID=${def.id}"`,
      `set "POTEMKIN_VERBOSITY=${verbosity}"`,
      `set "POTEMKIN_UNICODE=${unicode}"`,
      `set "POTEMKIN_PLUGINS_DIR=${pluginDirs}"`,
      `set "POTEMKIN_LANGUAGES=${languages}"`,
      ...(pluginOrder ? [`set "POTEMKIN_PLUGIN_ORDER=${pluginOrder}"`] : []),
      ...(pluginTimeout ? [`set "POTEMKIN_PLUGIN_TIMEOUT_MS=${pluginTimeout}"`] : []),
      `set "POTEMKIN_NODE=${node}"`,
      `set "POTEMKIN_RAW=${raw.enabled ? "1" : "0"}"`,
      ...(raw.enabled
        ? [
            `set "POTEMKIN_RAW_LABEL=${raw.label}"`,
            `set "POTEMKIN_RAW_FENCE=${raw.fence}"`,
            `set "POTEMKIN_RAW_SEPARATOR=${raw.separator ? "1" : "0"}"`,
          ]
        : []),
      ...(haveWasmHost ? [`set "POTEMKIN_WASM_HOST=${wasmHost}"`] : []),
      ...(multiplex ? [`set "POTEMKIN_MULTIPLEX=1"`] : []),
      `"${bin}" %*`,
      "",
    ];
    fs.writeFileSync(p, lines.join("\r\n"));
  } else {
    const q = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;
    const lines = [
      "#!/bin/sh",
      `export POTEMKIN_SERVER=${q(realCmd)}`,
      `export POTEMKIN_SERVER_ID=${q(def.id)}`,
      `export POTEMKIN_VERBOSITY=${q(verbosity)}`,
      `export POTEMKIN_UNICODE=${q(unicode)}`,
      `export POTEMKIN_PLUGINS_DIR=${q(pluginDirs)}`,
      `export POTEMKIN_LANGUAGES=${q(languages)}`,
      ...(pluginOrder ? [`export POTEMKIN_PLUGIN_ORDER=${q(pluginOrder)}`] : []),
      ...(pluginTimeout ? [`export POTEMKIN_PLUGIN_TIMEOUT_MS=${q(pluginTimeout)}`] : []),
      `export POTEMKIN_NODE=${q(node)}`,
      `export POTEMKIN_RAW=${q(raw.enabled ? "1" : "0")}`,
      ...(raw.enabled
        ? [
            `export POTEMKIN_RAW_LABEL=${q(raw.label)}`,
            `export POTEMKIN_RAW_FENCE=${q(raw.fence)}`,
            `export POTEMKIN_RAW_SEPARATOR=${q(raw.separator ? "1" : "0")}`,
          ]
        : []),
      ...(haveWasmHost ? [`export POTEMKIN_WASM_HOST=${q(wasmHost)}`] : []),
      ...(multiplex ? [`export POTEMKIN_MULTIPLEX=1`] : []),
      `exec ${q(bin)} "$@"`,
      "",
    ];
    fs.writeFileSync(p, lines.join("\n"));
    fs.chmodSync(p, 0o755);
  }
}

/**
 * Path to the bundled Node harness that runs WASM plugins via the extism JS SDK.
 * Produced by `scripts/stage-wasm-host.sh` (esbuild → a single self-contained
 * file) and shipped in the VSIX.
 */
function wasmHostPath(context: vscode.ExtensionContext): string {
  return path.join(context.extensionPath, "dist", "wasm-host.js");
}

// ---------------------------------------------------------------------------
// Registry + helpers
// ---------------------------------------------------------------------------

function userServers(): ServerDef[] {
  const raw = vscode.workspace.getConfiguration("potemkin").get<unknown[]>("servers", []);
  const out: ServerDef[] = [];
  for (const entry of raw ?? []) {
    if (entry && typeof entry === "object") {
      const e = entry as Partial<ServerDef>;
      if (e.id && e.pathSetting && e.defaultCommand) {
        out.push({
          id: e.id,
          label: e.label ?? e.id,
          extensionId: e.extensionId,
          pathSetting: e.pathSetting,
          pathSettingField: e.pathSettingField,
          restartCommand: e.restartCommand,
          defaultCommand: e.defaultCommand,
          languages: Array.isArray(e.languages) ? e.languages : [],
        });
      }
    }
  }
  return out;
}

/** Built-in + user servers, filtered to those whose owning extension is present. */
function relevantServers(context: vscode.ExtensionContext): ServerDef[] {
  const seen = new Set<string>();
  const all = [...BUILT_IN, ...userServers()];
  return all.filter((def) => {
    if (seen.has(def.id)) return false;
    seen.add(def.id);
    return !def.extensionId || !!vscode.extensions.getExtension(def.extensionId);
  });
}

function configTarget(): vscode.ConfigurationTarget {
  return vscode.workspace.workspaceFolders?.length
    ? vscode.ConfigurationTarget.Workspace
    : vscode.ConfigurationTarget.Global;
}

function platformDir(): string {
  return `${process.platform}-${process.arch}`;
}

function bundledBinaryPath(context: vscode.ExtensionContext): string {
  const name = process.platform === "win32" ? "potemkin.exe" : "potemkin";
  return path.join(context.extensionPath, "bin", platformDir(), name);
}

/** Heuristic: does this path point at a Potemkin binary or one of our launchers? */
function looksLikePotemkin(p: string | undefined, launcher: string): boolean {
  if (!p) return false;
  if (p === launcher) return true;
  const base = path.basename(p);
  return base === "potemkin" || base === "potemkin.exe" || base.startsWith("potemkin-");
}

function ensureExecutable(binary: string): void {
  if (process.platform === "win32") return;
  try {
    fs.chmodSync(binary, 0o755);
  } catch {
    /* non-fatal */
  }
}
