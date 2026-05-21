//! alzed-bridge: protocol translation between vanilla LSP (spoken by Zed) and
//! the proprietary `al/*` LSP variant spoken by Microsoft's AL Language Server
//! (`Microsoft.Dynamics.Nav.EditorServices.Host`).
//!
//! Wire layout:
//!
//! ```text
//!   Zed  <--stdio LSP-->  alzed-bridge  <--stdio LSP+al/*-->  AL Language Server
//! ```
//!
//! v0.2: protocol-aware passthrough.
//! - Parses every LSP frame in both directions
//! - After standard `initialized` notification, reads each workspace folder's
//!   `app.json` and sends `al/loadManifest` to the AL server
//! - Watches for `al/activeProjectLoaded` from the server (readiness signal)
//! - Rewrites empty `{}` hover responses to `null` (Zed expects null when no
//!   hover info; MS server returns empty object)
//! - Bridge-initiated requests use the id prefix `"alzed-bridge:N"`; their
//!   responses are consumed locally and never reach the client.
//!
//! See `docs/al-protocol.md` for protocol details.

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, trace, warn};

const ENV_AL_SERVER: &str = "ALZED_AL_SERVER";

/// Starting numeric ID for bridge-initiated requests. The MS AL server parses
/// JSON-RPC IDs as Int32 (despite the spec allowing strings) and crashes on
/// non-numeric IDs. We sit high in the i32 range, well past anything Zed will
/// produce, so collisions are effectively impossible.
const BRIDGE_ID_BASE: u64 = 900_000_000;

struct Config {
    al_server_path: PathBuf,
    al_server_args: Vec<String>,
}

impl Config {
    fn from_env_and_args() -> Result<Self> {
        let mut args = std::env::args().skip(1);
        let mut al_server_path: Option<PathBuf> = None;
        let mut al_server_args: Vec<String> = Vec::new();

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--al-server" => {
                    al_server_path = args.next().map(PathBuf::from);
                }
                "--" => {
                    al_server_args.extend(args.by_ref());
                    break;
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => bail!("unknown argument: {other}"),
            }
        }

        let al_server_path = al_server_path
            .or_else(|| std::env::var_os(ENV_AL_SERVER).map(PathBuf::from))
            .context(format!(
                "AL Language Server path not set. Pass --al-server <path> or set {ENV_AL_SERVER}."
            ))?;

        Ok(Self {
            al_server_path,
            al_server_args,
        })
    }
}

fn print_help() {
    eprintln!(
        "alzed-bridge {version}

Bridges vanilla LSP (from Zed) <-> al/* LSP variant (from Microsoft AL Language Server).

USAGE:
    alzed-bridge --al-server <PATH-TO-EditorServices.Host> [-- <server args>...]

ENV:
    {ENV_AL_SERVER}    Path to Microsoft.Dynamics.Nav.EditorServices.Host(.exe)
                       Used if --al-server is not provided.
    RUST_LOG           Tracing filter (default: info). Try
                       RUST_LOG=alzed_bridge=debug for handshake details, or
                       RUST_LOG=alzed_bridge=trace to see every LSP frame.
",
        version = env!("CARGO_PKG_VERSION"),
    );
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct WorkspaceFolder {
    uri: String,
    name: String,
}

#[derive(Default)]
struct Session {
    workspace_folders: Vec<WorkspaceFolder>,
    /// Client-issued request IDs → method names. Lets the server-to-client
    /// pump recognize responses it wants to mutate (hover normalization,
    /// initialize-capability injection, codeAction injection).
    client_request_methods: HashMap<String, String>,
    /// Per-request URI for codeAction calls, so the response handler can
    /// decide whether to inject AL actions (only for app.json and *.al).
    client_request_uris: HashMap<String, String>,
    /// Request IDs we (the bridge) generated, mapped to their method name so
    /// we can log responses meaningfully.
    bridge_inflight: HashMap<String, String>,
    next_bridge_id: u64,
}

impl Session {
    fn alloc_id(&mut self, method: &str) -> u64 {
        let id = BRIDGE_ID_BASE + self.next_bridge_id;
        self.next_bridge_id += 1;
        self.bridge_inflight.insert(id.to_string(), method.to_string());
        id
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    init_tracing();
    let config = Config::from_env_and_args()?;

    info!(
        al_server = %config.al_server_path.display(),
        args = ?config.al_server_args,
        "spawning AL Language Server"
    );

    let mut child = Command::new(&config.al_server_path)
        .args(&config.al_server_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to spawn {}", config.al_server_path.display()))?;

    let server_stdin = child.stdin.take().context("server stdin missing")?;
    let server_stdout = child.stdout.take().context("server stdout missing")?;
    let server_stderr = child.stderr.take().context("server stderr missing")?;

    tokio::spawn(forward_stderr(server_stderr));

    let session = Arc::new(Mutex::new(Session::default()));

    // Channels into the two writers — multiple tasks may want to send.
    let (to_server_tx, to_server_rx) = mpsc::channel::<Vec<u8>>(64);
    let (to_client_tx, to_client_rx) = mpsc::channel::<Vec<u8>>(64);

    // Writer tasks
    let writer_server = tokio::spawn(writer_pump(server_stdin, to_server_rx));
    let writer_client = tokio::spawn(writer_pump_stdout(to_client_rx));

    // Reader tasks
    let reader_client = tokio::spawn(client_to_server(
        tokio::io::stdin(),
        to_server_tx.clone(),
        session.clone(),
    ));
    let reader_server = tokio::spawn(server_to_client(
        server_stdout,
        to_client_tx,
        to_server_tx,
        session.clone(),
    ));

    tokio::select! {
        res = reader_client => warn!(?res, "client reader terminated"),
        res = reader_server => warn!(?res, "server reader terminated"),
        res = writer_server => warn!(?res, "server writer terminated"),
        res = writer_client => warn!(?res, "client writer terminated"),
        status = child.wait() => warn!(?status, "AL server exited"),
    }

    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
}

/// Wrap a JSON-RPC payload in LSP framing.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 64);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", payload.len()).as_bytes());
    out.extend_from_slice(payload);
    out
}

async fn writer_pump(mut sink: ChildStdin, mut rx: mpsc::Receiver<Vec<u8>>) -> Result<()> {
    while let Some(framed) = rx.recv().await {
        sink.write_all(&framed).await?;
        sink.flush().await?;
    }
    Ok(())
}

async fn writer_pump_stdout(mut rx: mpsc::Receiver<Vec<u8>>) -> Result<()> {
    let mut out = tokio::io::stdout();
    while let Some(framed) = rx.recv().await {
        out.write_all(&framed).await?;
        out.flush().await?;
    }
    Ok(())
}

/// Read frames from Zed and forward to AL server, with interception:
/// - Capture workspace folders from `initialize`.
/// - After `initialized`, kick off the AL handshake (`al/loadManifest` per folder).
/// - Track client-issued hover request IDs so responses can be normalized.
async fn client_to_server<R: AsyncRead + Unpin>(
    mut reader: R,
    to_server: mpsc::Sender<Vec<u8>>,
    session: Arc<Mutex<Session>>,
) -> Result<()> {
    loop {
        let body = match read_frame(&mut reader).await {
            Ok(b) => b,
            Err(e) => {
                debug!("client stream ended: {e}");
                return Ok(());
            }
        };

        let parsed: Option<Value> = serde_json::from_slice(&body).ok();
        if let Some(v) = &parsed {
            trace_frame(">>>", v);
            inspect_client_frame(v, &session).await;
        } else {
            trace!(target: "alzed_bridge::wire", bytes = body.len(), ">>> non-JSON");
        }

        // Intercept workspace/executeCommand for our custom commands.
        if let Some(v) = &parsed {
            if is_method_value(v, "workspace/executeCommand") {
                if let Some(cmd) = v
                    .get("params")
                    .and_then(|p| p.get("command"))
                    .and_then(|c| c.as_str())
                {
                    if OUR_COMMANDS.contains(&cmd) {
                        handle_our_command(v, &session, &to_server).await;
                        continue;
                    }
                }
            }
        }

        let body_to_send = match parsed.as_ref() {
            Some(v) if is_method_value(v, "workspace/didChangeConfiguration") => {
                rewrite_did_change_configuration(v, &session).await
            }
            _ => body.clone(),
        };

        to_server.send(frame(&body_to_send)).await?;

        // Fire the AL handshake right after `initialized` flows through.
        if is_method(&body, "initialized") {
            spawn_handshake(session.clone(), to_server.clone());
        }
    }
}

fn is_method_value(v: &Value, method: &str) -> bool {
    v.get("method").and_then(|m| m.as_str()) == Some(method)
}

/// Zed wraps LSP settings under the language-server id (`{settings:{al:{...}}}`),
/// but the MS AL server expects a specific structured object — see
/// [`build_al_settings`]. Unwrap the `al` namespace, apply user overrides to
/// the AL config sub-object, and emit the proper shape.
async fn rewrite_did_change_configuration(v: &Value, session: &Arc<Mutex<Session>>) -> Vec<u8> {
    let user_al = v
        .get("params")
        .and_then(|p| p.get("settings"))
        .and_then(|s| s.get("al"))
        .cloned()
        .unwrap_or_else(|| json!({}));

    let folders = session.lock().await.workspace_folders.clone();
    let project_path = folders
        .first()
        .and_then(|f| uri_to_path(&f.uri))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();

    let settings = build_al_settings(&project_path, &user_al);

    let rewritten = json!({
        "jsonrpc": "2.0",
        "method": "workspace/didChangeConfiguration",
        "params": { "settings": settings }
    });

    debug!(target: "alzed_bridge", "rewrote workspace/didChangeConfiguration into MS settings shape");
    serde_json::to_vec(&rewritten).expect("rewrite serialization must succeed")
}

/// Build the settings object the MS AL server expects, structured per the
/// VS Code AL extension's `getWorkspaceSettings()` output:
///
/// ```text
/// {
///   workspacePath: <abs path to project folder>,
///   alResourceConfigurationSettings: {
///     packageCachePaths: [...],
///     codeAnalyzers: [...],
///     enableCodeAnalysis, backgroundCodeAnalysis, ruleSetPath,
///     incrementalBuild, assemblyProbingPaths, ...
///   },
///   setActiveWorkspace: true,
///   dependencyParentWorkspacePath: null,
///   expectedProjectReferenceDefinitions: [],
///   activeWorkspaceClosure: []
/// }
/// ```
///
/// `user_overrides` is the flat `al.*` config the user put in Zed's settings
/// (e.g. `{packageCachePath, codeAnalyzers, enableCodeAnalysis}`). Each known
/// key maps into `alResourceConfigurationSettings`.
fn build_al_settings(project_path: &str, user_overrides: &Value) -> Value {
    let default_package_cache = if project_path.is_empty() {
        ".alpackages".to_string()
    } else {
        format!("{project_path}{}{}", std::path::MAIN_SEPARATOR, ".alpackages")
    };

    let overrides = user_overrides.as_object();

    // packageCachePaths is plural array. Accept singular string from user.
    let package_cache_paths = match overrides.and_then(|o| o.get("packageCachePath")) {
        Some(Value::String(s)) => json!([s]),
        Some(Value::Array(a)) => Value::Array(a.clone()),
        _ => json!([default_package_cache]),
    };

    let pick = |key: &str, default: Value| -> Value {
        overrides
            .and_then(|o| o.get(key))
            .cloned()
            .unwrap_or(default)
    };

    let al_resource = json!({
        "assemblyProbingPaths": pick("assemblyProbingPaths", json!(["./.netpackages"])),
        "codeAnalyzers": pick("codeAnalyzers", json!([])),
        "enableCodeAnalysis": pick("enableCodeAnalysis", json!(false)),
        "backgroundCodeAnalysis": pick("backgroundCodeAnalysis", json!("Project")),
        "packageCachePaths": package_cache_paths,
        "ruleSetPath": pick("ruleSetPath", json!("")),
        "enableCodeActions": pick("enableCodeActions", json!(true)),
        "incrementalBuild": pick("incrementalBuild", json!(false)),
        "enableCodeLensExternalUsage": pick("enableCodeLensExternalUsage", json!(false)),
        "outputAnalyzerStatistics": pick("outputAnalyzerStatistics", json!(false)),
        "enableExternalRulesets": pick("enableExternalRulesets", json!(false)),
        "namespaceTemplate": pick("namespaceTemplate", json!("")),
    });

    json!({
        "workspacePath": project_path,
        "alResourceConfigurationSettings": al_resource,
        "setActiveWorkspace": true,
        "dependencyParentWorkspacePath": Value::Null,
        "expectedProjectReferenceDefinitions": [],
        "activeWorkspaceClosure": [],
    })
}

async fn server_to_client<R: AsyncRead + Unpin>(
    mut reader: R,
    to_client: mpsc::Sender<Vec<u8>>,
    to_server: mpsc::Sender<Vec<u8>>,
    session: Arc<Mutex<Session>>,
) -> Result<()> {
    loop {
        let body = match read_frame(&mut reader).await {
            Ok(b) => b,
            Err(e) => {
                debug!("server stream ended: {e}");
                return Ok(());
            }
        };

        let v: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => {
                trace!(target: "alzed_bridge::wire", bytes = body.len(), "<<< non-JSON");
                to_client.send(frame(&body)).await?;
                continue;
            }
        };

        trace_frame("<<<", &v);

        // Inject AL symbol-management hint diagnostic on app.json so the
        // project surface has a visible indicator that AL tooling is wired
        // up. Quick-fix actions on app.json are routed by Zed to the JSON
        // language server, not us — so the diagnostic's *message* points
        // users to the .al-file code-action route for the actual click
        // target. (We do still inject the actions there via the
        // textDocument/codeAction interceptor.)
        if is_method_value(&v, "textDocument/publishDiagnostics") {
            let v = inject_app_json_diagnostic(&v);
            let bytes = serde_json::to_vec(&v).unwrap_or(body.clone());
            to_client.send(frame(&bytes)).await?;
            let _ = &to_server; // keep variable used
            continue;
        }

        // Handle responses to bridge-initiated requests — consume locally, never
        // forward to Zed (it didn't send the request).
        if let Some(id_str) = json_id_to_string(v.get("id")) {
            let method = session.lock().await.bridge_inflight.remove(&id_str);
            if let Some(method) = method {
                if let Some(err) = v.get("error") {
                    warn!(method = %method, error = %err, "bridge request failed");
                } else {
                    debug!(
                        method = %method,
                        result = %truncate(&v.get("result").map(|r| r.to_string()).unwrap_or_default(), 200),
                        "bridge request ok"
                    );
                }
                continue;
            }
        }

        // Server-side notifications we want to observe.
        if let Some(method) = v.get("method").and_then(|m| m.as_str()) {
            if method == "al/activeProjectLoaded" {
                let folder = v
                    .get("params")
                    .and_then(|p| p.get("activeProjectFolder"))
                    .and_then(|f| f.as_str())
                    .unwrap_or("<unknown>");
                info!(folder, "AL server reports project loaded — features should now respond");
            }
        }

        // Normalize empty `{}` hover responses to `null`.
        let (forward, body_to_send) = maybe_normalize_hover(&v, &body, &session).await?;
        if forward {
            to_client.send(frame(&body_to_send)).await?;
        }

        // Hint to the compiler that to_server is used (we'll need it for future
        // server-driven outbound flows — e.g. responding to server→client
        // requests we want to intercept). Currently unused.
        let _ = &to_server;
    }
}

async fn inspect_client_frame(v: &Value, session: &Arc<Mutex<Session>>) {
    // Track method by request id so we can mutate server responses later.
    if let (Some(method), Some(id)) = (
        v.get("method").and_then(|m| m.as_str()),
        json_id_to_string(v.get("id")),
    ) {
        let mut guard = session.lock().await;
        guard
            .client_request_methods
            .insert(id.clone(), method.to_string());
        if matches!(method, "textDocument/codeAction" | "textDocument/codeLens") {
            if let Some(uri) = v
                .get("params")
                .and_then(|p| p.get("textDocument"))
                .and_then(|t| t.get("uri"))
                .and_then(|u| u.as_str())
            {
                guard.client_request_uris.insert(id, uri.to_string());
            }
        }
    }

    // Capture workspace folders from `initialize` params.
    if v.get("method").and_then(|m| m.as_str()) == Some("initialize") {
        let folders = extract_workspace_folders(v);
        if !folders.is_empty() {
            info!(count = folders.len(), "captured workspace folders from initialize");
            session.lock().await.workspace_folders = folders;
        }
    }
}

fn extract_workspace_folders(initialize: &Value) -> Vec<WorkspaceFolder> {
    let params = match initialize.get("params") {
        Some(p) => p,
        None => return Vec::new(),
    };

    if let Some(folders) = params.get("workspaceFolders").and_then(|f| f.as_array()) {
        return folders
            .iter()
            .filter_map(|f| serde_json::from_value::<WorkspaceFolder>(f.clone()).ok())
            .collect();
    }

    // Fallback: rootUri (single-folder workspaces, deprecated but still seen).
    if let Some(root_uri) = params.get("rootUri").and_then(|u| u.as_str()) {
        let name = uri_basename(root_uri);
        return vec![WorkspaceFolder {
            uri: root_uri.to_string(),
            name,
        }];
    }

    Vec::new()
}

fn uri_basename(uri: &str) -> String {
    uri.rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(uri)
        .to_string()
}

async fn maybe_normalize_hover(
    v: &Value,
    original: &[u8],
    session: &Arc<Mutex<Session>>,
) -> Result<(bool, Vec<u8>)> {
    let id = match json_id_to_string(v.get("id")) {
        Some(s) => s,
        None => return Ok((true, original.to_vec())),
    };

    let (method, uri) = {
        let mut guard = session.lock().await;
        let method = guard.client_request_methods.remove(&id);
        let uri = guard.client_request_uris.remove(&id);
        (method, uri)
    };

    let method = match method {
        Some(m) => m,
        None => return Ok((true, original.to_vec())),
    };

    match method.as_str() {
        "textDocument/hover" => {
            let needs_rewrite =
                matches!(v.get("result"), Some(Value::Object(o)) if o.is_empty());
            if !needs_rewrite {
                return Ok((true, original.to_vec()));
            }
            let mut patched = v.clone();
            if let Some(obj) = patched.as_object_mut() {
                obj.insert("result".to_string(), Value::Null);
            }
            debug!(id, "normalized empty hover {{}} -> null");
            Ok((true, serde_json::to_vec(&patched)?))
        }
        "initialize" => Ok((true, inject_execute_commands(v, original)?)),
        "textDocument/codeAction" => {
            Ok((true, inject_code_actions(v, original, uri.as_deref())?))
        }
        "textDocument/codeLens" => {
            Ok((true, inject_code_lenses(v, original, uri.as_deref())?))
        }
        _ => Ok((true, original.to_vec())),
    }
}

const CMD_DOWNLOAD_SYMBOLS: &str = "alzed.al.downloadSymbols";
const CMD_CHECK_SYMBOLS: &str = "alzed.al.checkSymbols";
const OUR_COMMANDS: [&str; 2] = [CMD_DOWNLOAD_SYMBOLS, CMD_CHECK_SYMBOLS];

/// Inject our custom commands and codeLensProvider into the server's
/// `initialize` response so the client knows it can ask for them via
/// `workspace/executeCommand` and `textDocument/codeLens`.
fn inject_execute_commands(v: &Value, original: &[u8]) -> Result<Vec<u8>> {
    let mut patched = v.clone();
    let caps = patched
        .pointer_mut("/result/capabilities")
        .and_then(|c| c.as_object_mut());
    let caps = match caps {
        Some(c) => c,
        None => return Ok(original.to_vec()),
    };

    // executeCommandProvider.commands += our two commands
    let provider = caps
        .entry("executeCommandProvider")
        .or_insert_with(|| json!({"commands": []}));
    if let Some(arr) = provider
        .as_object_mut()
        .and_then(|o| o.entry("commands").or_insert_with(|| json!([])).as_array_mut())
    {
        for cmd in OUR_COMMANDS {
            if !arr.iter().any(|v| v.as_str() == Some(cmd)) {
                arr.push(Value::String(cmd.to_string()));
            }
        }
    }

    // codeLensProvider — claim support so Zed asks us for lenses on every
    // open file. We inject AL: Download/Check on .al and app.json files.
    if !caps.contains_key("codeLensProvider") {
        caps.insert(
            "codeLensProvider".to_string(),
            json!({ "resolveProvider": false }),
        );
    }

    info!("injected alzed commands + codeLensProvider into server capabilities");
    Ok(serde_json::to_vec(&patched)?)
}

/// On textDocument/publishDiagnostics for app.json, prepend a single
/// Information-severity diagnostic that surfaces the AL symbol-management
/// affordance. The server's existing diagnostics for app.json are preserved.
fn inject_app_json_diagnostic(v: &Value) -> Value {
    let uri = v
        .pointer("/params/uri")
        .and_then(|u| u.as_str())
        .unwrap_or("");
    if !uri.to_ascii_lowercase().ends_with("/app.json") {
        return v.clone();
    }

    let mut patched = v.clone();
    let arr = match patched
        .pointer_mut("/params/diagnostics")
        .and_then(|d| d.as_array_mut())
    {
        Some(a) => a,
        None => return v.clone(),
    };

    // De-dup: if our hint is already in the list (e.g. server republished
    // and we re-injected on top of an earlier injection round-trip), skip.
    let already_present = arr
        .iter()
        .any(|d| d.get("source").and_then(|s| s.as_str()) == Some("alzed"));
    if already_present {
        return patched;
    }

    let hint = json!({
        "range": {
            "start": {"line": 0, "character": 0},
            "end":   {"line": 0, "character": 80},
        },
        "severity": 2,
        "source": "alzed",
        "code": "AL_SYMBOLS",
        "message": "AL: symbol management — open any .al file and press Ctrl+. to download or check symbols.",
    });
    arr.insert(0, hint);
    debug!(target: "alzed_bridge", "injected hint diagnostic on app.json");
    patched
}

/// Prepend "AL: Download symbols" / "AL: Check symbols" code lenses to the
/// server's codeLens response for .al files and app.json.
fn inject_code_lenses(v: &Value, original: &[u8], uri: Option<&str>) -> Result<Vec<u8>> {
    let uri = match uri {
        Some(u) => u,
        None => return Ok(original.to_vec()),
    };
    let lower = uri.to_ascii_lowercase();
    if !(lower.ends_with("/app.json") || lower.ends_with(".al")) {
        return Ok(original.to_vec());
    }

    let mut patched = v.clone();
    let result = patched.pointer_mut("/result");
    let arr_mut = match result {
        Some(Value::Array(a)) => a,
        Some(other) => {
            *other = Value::Array(Vec::new());
            other.as_array_mut().unwrap()
        }
        None => return Ok(original.to_vec()),
    };

    let anchor = json!({
        "start": {"line": 0, "character": 0},
        "end":   {"line": 0, "character": 0},
    });

    let mut injected = vec![
        json!({
            "range": anchor,
            "command": {
                "title": "AL: Download symbols",
                "command": CMD_DOWNLOAD_SYMBOLS,
                "arguments": [{ "uri": uri }],
            }
        }),
        json!({
            "range": anchor,
            "command": {
                "title": "AL: Check symbols",
                "command": CMD_CHECK_SYMBOLS,
                "arguments": [{ "uri": uri }],
            }
        }),
    ];
    injected.append(arr_mut);
    *arr_mut = injected;

    debug!(uri, "injected AL code lenses into codeLens response");
    Ok(serde_json::to_vec(&patched)?)
}

/// Prepend "AL: Download symbols" / "AL: Check symbols" actions to the
/// codeAction response when the request was for app.json or a *.al file.
fn inject_code_actions(v: &Value, original: &[u8], uri: Option<&str>) -> Result<Vec<u8>> {
    let uri = match uri {
        Some(u) => u,
        None => return Ok(original.to_vec()),
    };
    let lower = uri.to_ascii_lowercase();
    if !(lower.ends_with("/app.json") || lower.ends_with(".al")) {
        return Ok(original.to_vec());
    }

    let mut patched = v.clone();
    let result = patched.pointer_mut("/result");
    let arr_mut = match result {
        Some(Value::Array(a)) => a,
        Some(other @ Value::Null) => {
            *other = Value::Array(Vec::new());
            other.as_array_mut().unwrap()
        }
        _ => return Ok(original.to_vec()),
    };

    let mut injected = vec![
        json!({
            "title": "AL: Download symbols",
            "isPreferred": true,
            "command": {
                "title": "AL: Download symbols",
                "command": CMD_DOWNLOAD_SYMBOLS,
                "arguments": [{ "uri": uri }],
            }
        }),
        json!({
            "title": "AL: Check symbols",
            "isPreferred": true,
            "command": {
                "title": "AL: Check symbols",
                "command": CMD_CHECK_SYMBOLS,
                "arguments": [{ "uri": uri }],
            }
        }),
    ];
    injected.append(arr_mut);
    *arr_mut = injected;

    debug!(uri, "injected AL code actions into codeAction response");
    Ok(serde_json::to_vec(&patched)?)
}

fn spawn_handshake(session: Arc<Mutex<Session>>, to_server: mpsc::Sender<Vec<u8>>) {
    tokio::spawn(async move {
        if let Err(e) = run_handshake(session, to_server).await {
            error!(error = %e, "AL handshake failed");
        }
    });
}

/// Handle our injected commands locally. The bridge never forwards them to
/// the AL server as-is — instead each one maps to one or more al/* requests.
/// We respond to the client immediately with null so it doesn't block the UI;
/// the actual al/* result comes back asynchronously and is logged.
async fn handle_our_command(
    req: &Value,
    session: &Arc<Mutex<Session>>,
    _to_server: &mpsc::Sender<Vec<u8>>,
) {
    let cmd = req
        .get("params")
        .and_then(|p| p.get("command"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    let id = req.get("id").cloned();
    let arg_uri = req
        .get("params")
        .and_then(|p| p.get("arguments"))
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|a| a.get("uri"))
        .and_then(|u| u.as_str())
        .map(|s| s.to_string());

    info!(command = cmd, uri = ?arg_uri, "executing alzed command");

    // Resolve the project folder: prefer the workspace containing the
    // invocation URI, fall back to the first workspace folder.
    let folder = {
        let guard = session.lock().await;
        let folders: Vec<PathBuf> = guard
            .workspace_folders
            .iter()
            .filter_map(|f| uri_to_path(&f.uri))
            .collect();
        let from_uri = arg_uri
            .as_deref()
            .and_then(uri_to_path)
            .and_then(|p| folders.iter().find(|f| p.starts_with(f)).cloned());
        from_uri.or_else(|| folders.first().cloned())
    };

    let result = match cmd {
        CMD_DOWNLOAD_SYMBOLS => match folder.as_deref() {
            Some(f) => trigger_download_symbols(f, session, _to_server).await,
            None => Err(anyhow!("no workspace folder resolved for downloadSymbols")),
        },
        CMD_CHECK_SYMBOLS => trigger_check_symbols(session, _to_server).await,
        _ => Ok(()),
    };
    if let Err(e) = result {
        warn!(command = cmd, error = %e, "alzed command failed");
    }

    // Acknowledge the executeCommand back to the client with null. We write
    // directly to stdout; this is the only place outside writer_pump_stdout
    // that touches stdout — the synchronization risk is tiny (single byte
    // sequences) and the alternative (threading another channel) isn't worth
    // the complexity for a single response.
    if let Some(id_val) = id {
        let ack = json!({ "jsonrpc": "2.0", "id": id_val, "result": null });
        if let Ok(bytes) = serde_json::to_vec(&ack) {
            let mut out = tokio::io::stdout();
            let framed = frame(&bytes);
            if let Err(e) = out.write_all(&framed).await {
                warn!(error = %e, "failed to ack executeCommand to client");
            } else {
                let _ = out.flush().await;
            }
        }
    }
}

async fn trigger_download_symbols(
    folder: &std::path::Path,
    session: &Arc<Mutex<Session>>,
    to_server: &mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let launch_path = folder.join(".vscode").join("launch.json");
    let launch_text = tokio::fs::read_to_string(&launch_path)
        .await
        .with_context(|| format!("reading {}", launch_path.display()))?;
    let launch: Value = parse_jsonc(&launch_text)
        .with_context(|| format!("parsing {}", launch_path.display()))?;
    let config = launch
        .get("configurations")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .cloned()
        .ok_or_else(|| anyhow!("launch.json has no configurations[]"))?;

    // Params shape from VS Code AL extension's ServerProxy.sendRequest:
    //   Object.assign({configuration: launchConfig}, getAlParams())
    // i.e. the launch.json config goes under `configuration`, and AL-level
    // params (browser/env/symbol feeds + force) are merged alongside.
    let params = json!({
        "configuration": config,
        "browserInfo": { "browser": null, "incognito": false },
        "environmentInfo": { "env": null },
        "symbolsCountryRegion": null,
        "nugetFeeds": [],
        "useOnlyCustomFeeds": false,
        "force": false,
    });

    let id = session.lock().await.alloc_id("al/downloadSymbols");
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "al/downloadSymbols",
        "params": params,
    });
    info!(
        config_name = config.get("name").and_then(|n| n.as_str()).unwrap_or("?"),
        id,
        "sending al/downloadSymbols"
    );
    to_server.send(frame(&serde_json::to_vec(&request)?)).await?;
    Ok(())
}

async fn trigger_check_symbols(
    session: &Arc<Mutex<Session>>,
    to_server: &mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let id = session.lock().await.alloc_id("al/checkSymbols");
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "al/checkSymbols",
        "params": {}
    });
    info!(id, "sending al/checkSymbols");
    to_server.send(frame(&serde_json::to_vec(&request)?)).await?;
    Ok(())
}

/// Parse JSON-with-comments (the format VS Code uses for launch.json,
/// settings.json, tasks.json). Strips // line comments and /* block */
/// comments while respecting string literals, then runs serde_json on the
/// result. Also tolerates trailing commas.
fn parse_jsonc(input: &str) -> Result<Value> {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escape = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
            }
            '/' if chars.peek() == Some(&'/') => {
                for nc in chars.by_ref() {
                    if nc == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for nc in chars.by_ref() {
                    if prev == '*' && nc == '/' {
                        break;
                    }
                    prev = nc;
                }
            }
            _ => out.push(c),
        }
    }
    // Strip trailing commas before } or ].
    let mut cleaned = String::with_capacity(out.len());
    let mut chars = out.chars().peekable();
    while let Some(c) = chars.next() {
        if c == ',' {
            let mut lookahead = chars.clone();
            let mut next_non_ws = None;
            while let Some(&p) = lookahead.peek() {
                if p.is_whitespace() {
                    lookahead.next();
                } else {
                    next_non_ws = Some(p);
                    break;
                }
            }
            if matches!(next_non_ws, Some('}') | Some(']')) {
                continue;
            }
        }
        cleaned.push(c);
    }
    Ok(serde_json::from_str(&cleaned)?)
}

async fn run_handshake(
    session: Arc<Mutex<Session>>,
    to_server: mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let folders = session.lock().await.workspace_folders.clone();

    if folders.is_empty() {
        warn!("no workspace folders captured — skipping al/loadManifest (server will stay idle)");
        return Ok(());
    }

    for folder in folders {
        let project_path = match uri_to_path(&folder.uri) {
            Some(p) => p,
            None => {
                warn!(uri = %folder.uri, "cannot convert workspace URI to path");
                continue;
            }
        };

        let manifest_path = project_path.join("app.json");
        let manifest_text = match tokio::fs::read_to_string(&manifest_path).await {
            Ok(t) => t,
            Err(e) => {
                warn!(
                    path = %manifest_path.display(),
                    error = %e,
                    "could not read app.json — workspace folder is not an AL project, skipping"
                );
                continue;
            }
        };

        let id = session.lock().await.alloc_id("al/loadManifest");

        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "al/loadManifest",
            "params": {
                "projectFolder": project_path.to_string_lossy(),
                "manifest": manifest_text,
            }
        });

        info!(
            project = %project_path.display(),
            id,
            "sending al/loadManifest"
        );

        let body = serde_json::to_vec(&request)?;
        to_server.send(frame(&body)).await?;

        // 1) workspace/didChangeConfiguration — give the server its AL
        //    workspace settings in the structured shape it expects.
        let project_path_str = project_path.to_string_lossy().into_owned();
        let settings = build_al_settings(&project_path_str, &json!({}));

        let notification = json!({
            "jsonrpc": "2.0",
            "method": "workspace/didChangeConfiguration",
            "params": { "settings": settings.clone() }
        });
        info!(
            project = %project_path.display(),
            "sending workspace/didChangeConfiguration (structured AL settings)"
        );
        to_server
            .send(frame(&serde_json::to_vec(&notification)?))
            .await?;

        // 2) al/setActiveWorkspace — the trigger that makes the AL server
        //    actually start analyzing. The C# handler reads
        //    settings.workspacePath to build a DirectoryInfo and start
        //    project analysis. Without this call, the server stays idle.
        let active_id = session.lock().await.alloc_id("al/setActiveWorkspace");
        let active_request = json!({
            "jsonrpc": "2.0",
            "id": active_id,
            "method": "al/setActiveWorkspace",
            "params": {
                "currentWorkspaceFolderPath": project_path_str,
                "settings": settings,
            }
        });
        info!(
            project = %project_path.display(),
            id = active_id,
            "sending al/setActiveWorkspace"
        );
        to_server
            .send(frame(&serde_json::to_vec(&active_request)?))
            .await?;
    }

    Ok(())
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let parsed = url::Url::parse(uri).ok()?;
    if parsed.scheme() != "file" {
        return None;
    }
    parsed.to_file_path().ok()
}

fn json_id_to_string(id: Option<&Value>) -> Option<String> {
    match id? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Log a JSON-RPC frame's shape (method + id + kind) at TRACE level.
fn trace_frame(arrow: &str, v: &Value) {
    let method = v.get("method").and_then(|m| m.as_str());
    let id = v.get("id").map(|i| i.to_string());
    let kind = if v.get("method").is_some() {
        if v.get("id").is_some() { "request" } else { "notif" }
    } else if v.get("result").is_some() {
        "response"
    } else if v.get("error").is_some() {
        "error"
    } else {
        "?"
    };
    let preview = match v.get("params").or_else(|| v.get("result")).or_else(|| v.get("error")) {
        Some(p) => truncate(&p.to_string(), 240),
        None => String::new(),
    };
    trace!(
        target: "alzed_bridge::wire",
        "{arrow} {kind:8} id={id:?} method={method:?} payload={preview}"
    );
}

fn is_method(body: &[u8], method: &str) -> bool {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_string))
        .map(|m| m == method)
        .unwrap_or(false)
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Vec<u8>> {
    let mut content_length: Option<usize> = None;
    let mut line = Vec::with_capacity(64);

    loop {
        line.clear();
        read_until_crlf(reader, &mut line).await?;

        if line == b"\r\n" {
            let len = content_length.context("LSP frame missing Content-Length header")?;
            let mut body = vec![0u8; len];
            reader.read_exact(&mut body).await.context("reading body")?;
            return Ok(body);
        }

        let line_str = std::str::from_utf8(&line).context("LSP header not UTF-8")?;
        if let Some(rest) = line_str.strip_prefix("Content-Length:") {
            let n: usize = rest
                .trim()
                .trim_end_matches("\r\n")
                .parse()
                .context("parsing Content-Length")?;
            content_length = Some(n);
        }
    }
}

async fn read_until_crlf<R: AsyncRead + Unpin>(reader: &mut R, out: &mut Vec<u8>) -> Result<()> {
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            return Err(anyhow!("EOF while reading LSP header line"));
        }
        out.push(byte[0]);
        let len = out.len();
        if len >= 2 && out[len - 2] == b'\r' && out[len - 1] == b'\n' {
            return Ok(());
        }
    }
}

async fn forward_stderr<R: AsyncRead + Unpin>(mut server_stderr: R) {
    let mut buf = [0u8; 4096];
    loop {
        match server_stderr.read(&mut buf).await {
            Ok(0) => return,
            Ok(n) => {
                let line = String::from_utf8_lossy(&buf[..n]);
                warn!(target: "alzed_bridge::al_server_stderr", "{}", line.trim_end());
            }
            Err(e) => {
                warn!(error = %e, "reading AL server stderr");
                return;
            }
        }
    }
}
