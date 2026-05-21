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

        if let Ok(v) = serde_json::from_slice::<Value>(&body) {
            inspect_client_frame(&v, &session).await;
        } else {
            trace!(target: "alzed_bridge::wire", bytes = body.len(), "client->server non-JSON");
        }

        to_server.send(frame(&body)).await?;

        // Fire the AL handshake right after `initialized` flows through.
        if is_method(&body, "initialized") {
            spawn_handshake(session.clone(), to_server.clone());
        }
    }
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
                trace!(target: "alzed_bridge::wire", bytes = body.len(), "server->client non-JSON");
                to_client.send(frame(&body)).await?;
                continue;
            }
        };

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
        if let Err(e) = run_handshake(session, to_server).await {
            error!(error = %e, "AL handshake failed");
        }
    });
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

        // After loadManifest, push a workspace/didChangeConfiguration so the
        // AL server actually starts analyzing. The VS Code extension does this
        // via sendConfigurationChange after a manifest is loaded. Without it
        // the server has the manifest but no config, and stays idle.
        send_did_change_configuration(&project_path, &to_server).await?;
    }

    Ok(())
}

async fn send_did_change_configuration(
    project_path: &std::path::Path,
    to_server: &mpsc::Sender<Vec<u8>>,
) -> Result<()> {
    let package_cache_path = project_path
        .join(".alpackages")
        .to_string_lossy()
        .into_owned();

    let notification = json!({
        "jsonrpc": "2.0",
        "method": "workspace/didChangeConfiguration",
        "params": {
            "settings": {
                "al": {
                    "packageCachePath": package_cache_path,
                    "enableCodeAnalysis": false,
                    "backgroundCodeAnalysis": "Project",
                    "codeAnalyzers": [],
                    "ruleSetPath": "",
                    "incrementalBuild": false,
                    "assemblyProbingPaths": ["./.netpackages"],
                    "dependencyClosure": [],
                    "projectReferences": []
                }
            }
        }
    });

    info!(
        project = %project_path.display(),
        "sending workspace/didChangeConfiguration"
    );
    let body = serde_json::to_vec(&notification)?;
    to_server.send(frame(&body)).await?;
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
