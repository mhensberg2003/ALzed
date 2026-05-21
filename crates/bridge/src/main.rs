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
    /// Request IDs that the client sent for `textDocument/hover`, so we can
    /// normalize empty `{}` responses to `null` when they come back.
    client_hover_ids: HashMap<String, ()>,
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
    // Track hover request IDs.
    if let (Some(method), Some(id)) = (
        v.get("method").and_then(|m| m.as_str()),
        json_id_to_string(v.get("id")),
    ) {
        if method == "textDocument/hover" {
            session.lock().await.client_hover_ids.insert(id, ());
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

    let is_hover = {
        let mut guard = session.lock().await;
        guard.client_hover_ids.remove(&id).is_some()
    };

    if !is_hover {
        return Ok((true, original.to_vec()));
    }

    // Only rewrite when result is an empty object.
    let needs_rewrite = matches!(v.get("result"), Some(Value::Object(o)) if o.is_empty());

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

fn spawn_handshake(session: Arc<Mutex<Session>>, to_server: mpsc::Sender<Vec<u8>>) {
    tokio::spawn(async move {
        if let Err(e) = run_handshake(session.clone(), to_server.clone()).await {
            error!(error = %e, "AL handshake failed");
        }
        // After handshake, start the trigger-file watcher so users can invoke
        // commands like download-symbols from outside Zed.
        spawn_trigger_watcher(session, to_server);
    });
}

fn spawn_trigger_watcher(session: Arc<Mutex<Session>>, to_server: mpsc::Sender<Vec<u8>>) {
    tokio::spawn(async move {
        let folders = session.lock().await.workspace_folders.clone();
        let paths: Vec<PathBuf> = folders
            .iter()
            .filter_map(|f| uri_to_path(&f.uri))
            .collect();
        if paths.is_empty() {
            return;
        }
        info!(
            count = paths.len(),
            "watching for .alzed-trigger.txt in each workspace folder"
        );
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            for folder in &paths {
                let trigger = folder.join(".alzed-trigger.txt");
                let content = match tokio::fs::read_to_string(&trigger).await {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                // Best-effort cleanup so a command fires exactly once.
                let _ = tokio::fs::remove_file(&trigger).await;
                let cmd = content.trim().to_string();
                info!(folder = %folder.display(), command = %cmd, "trigger file detected");
                if let Err(e) = handle_trigger(&cmd, folder, &session, &to_server).await {
                    warn!(command = %cmd, error = %e, "trigger command failed");
                }
            }
        }
    });
}

async fn handle_trigger(
    cmd: &str,
    folder: &std::path::Path,
    session: &Arc<Mutex<Session>>,
    to_server: &mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    match cmd {
        "download-symbols" => trigger_download_symbols(folder, session, to_server).await,
        "check-symbols" => trigger_check_symbols(session, to_server).await,
        other => {
            warn!(
                command = other,
                "unknown trigger command — supported: download-symbols, check-symbols"
            );
            Ok(())
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

    let id = session.lock().await.alloc_id("al/downloadSymbols");
    let request = json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "al/downloadSymbols",
        "params": config,
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
