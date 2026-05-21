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
//! v0.1: pure passthrough with trace logging. No translation yet. Establishes
//! the byte transport and lets us capture real traffic to spec out subsequent
//! phases (init handshake, completion translation, etc.).

use anyhow::{bail, Context, Result};
use std::path::PathBuf;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::{debug, info, warn};

const ENV_AL_SERVER: &str = "ALZED_AL_SERVER";

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
                    al_server_path = args
                        .next()
                        .map(PathBuf::from)
                        .context("--al-server requires a path argument")
                        .ok();
                }
                "--" => {
                    al_server_args.extend(args.by_ref());
                    break;
                }
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => {
                    bail!("unknown argument: {other}");
                }
            }
        }

        let al_server_path = al_server_path
            .or_else(|| std::env::var_os(ENV_AL_SERVER).map(PathBuf::from))
            .context(format!(
                "AL Language Server path not set. Pass --al-server <path> or set {ENV_AL_SERVER}.",
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
    RUST_LOG           Tracing filter (default: info). Try RUST_LOG=alzed_bridge=trace
                       to see every LSP message.
",
        version = env!("CARGO_PKG_VERSION"),
    );
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

    // Stderr from the AL server -> our stderr, with a prefix.
    tokio::spawn(forward_stderr(server_stderr));

    // Zed (our stdin) -> AL server (server_stdin), with frame inspection.
    let to_server = tokio::spawn(pump_lsp(
        tokio::io::stdin(),
        server_stdin,
        Direction::ClientToServer,
    ));

    // AL server (server_stdout) -> Zed (our stdout), with frame inspection.
    let to_client = tokio::spawn(pump_lsp(
        server_stdout,
        tokio::io::stdout(),
        Direction::ServerToClient,
    ));

    tokio::select! {
        res = to_server => warn!(?res, "client->server pump terminated"),
        res = to_client => warn!(?res, "server->client pump terminated"),
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

#[derive(Clone, Copy, Debug)]
enum Direction {
    ClientToServer,
    ServerToClient,
}

/// Read length-prefixed LSP frames from `reader`, log the method/id, write them
/// back out to `writer` unmodified. This is the seam where translation will
/// eventually plug in.
async fn pump_lsp<R, W>(mut reader: R, mut writer: W, dir: Direction) -> Result<()>
where
    R: AsyncReadExt + Unpin,
    W: AsyncWriteExt + Unpin,
{
    let mut header_buf = Vec::with_capacity(256);
    loop {
        header_buf.clear();
        let content_length = read_headers(&mut reader, &mut header_buf).await?;

        let mut body = vec![0u8; content_length];
        reader
            .read_exact(&mut body)
            .await
            .context("reading LSP body")?;

        trace_frame(dir, &body);

        writer.write_all(&header_buf).await?;
        writer.write_all(&body).await?;
        writer.flush().await?;
    }
}

/// Read LSP headers from reader, return content length and the raw header
/// bytes (caller is responsible for echoing them).
async fn read_headers<R: AsyncReadExt + Unpin>(
    reader: &mut R,
    out: &mut Vec<u8>,
) -> Result<usize> {
    let mut content_length: Option<usize> = None;
    let mut line = Vec::with_capacity(64);

    loop {
        line.clear();
        read_until_crlf(reader, &mut line).await?;
        out.extend_from_slice(&line);

        if line == b"\r\n" {
            return content_length.context("LSP frame missing Content-Length header");
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

async fn read_until_crlf<R: AsyncReadExt + Unpin>(reader: &mut R, out: &mut Vec<u8>) -> Result<()> {
    let mut byte = [0u8; 1];
    loop {
        let n = reader.read(&mut byte).await?;
        if n == 0 {
            bail!("EOF while reading LSP header line");
        }
        out.push(byte[0]);
        let len = out.len();
        if len >= 2 && out[len - 2] == b'\r' && out[len - 1] == b'\n' {
            return Ok(());
        }
    }
}

fn trace_frame(dir: Direction, body: &[u8]) {
    let preview = match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(v) => {
            let method = v.get("method").and_then(|m| m.as_str());
            let id = v
                .get("id")
                .map(|i| i.to_string())
                .unwrap_or_else(|| "-".to_string());
            let kind = if v.get("method").is_some() {
                if v.get("id").is_some() {
                    "request"
                } else {
                    "notification"
                }
            } else if v.get("result").is_some() {
                "response"
            } else if v.get("error").is_some() {
                "error"
            } else {
                "?"
            };
            format!(
                "{kind} id={id} method={method}",
                method = method.unwrap_or("-"),
            )
        }
        Err(_) => format!("<{} bytes, non-JSON>", body.len()),
    };
    debug!(target: "alzed_bridge::wire", direction = ?dir, "{}", preview);
}

async fn forward_stderr<R: AsyncReadExt + Unpin>(mut server_stderr: R) {
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
