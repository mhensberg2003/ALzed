//! LSP `$/progress` helpers.
//!
//! We use progress notifications for two distinct flows:
//!
//! 1. **Bridge-initiated commands** (download/check symbols, build, publish).
//!    Each user-triggered command opens a fresh token, sends a `begin`
//!    immediately, and closes with `end` when the AL server's response
//!    arrives. This is what gives Zed the "spinner in the status bar"
//!    behavior during the otherwise-silent 30s symbol downloads.
//!
//! 2. **Server-driven `al/progressNotification`**. The AL server emits
//!    these during project load (owner=OpenWorkspace) and profiler runs
//!    (owner=Profiler). We map each unique owner to a stable token so
//!    multiple notifications from the same owner update the same task
//!    in Zed's UI, rather than spawning new ones each time.
//!
//! Token IDs are plain strings — `alzed.<purpose>.<n>` — so they're easy
//! to grep in client logs when debugging.

use anyhow::Result;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::frame;

/// Send `window/workDoneProgress/create` to the client. The client is
/// expected to respond OK; we hand back the request `id` we used so the
/// caller can either ignore the response or filter it from the
/// client→server pump.
pub async fn create(
    to_client: &mpsc::Sender<Vec<u8>>,
    request_id: u64,
    token: &str,
) -> Result<()> {
    let req = json!({
        "jsonrpc": "2.0",
        "id": request_id,
        "method": "window/workDoneProgress/create",
        "params": { "token": token },
    });
    let bytes = serde_json::to_vec(&req)?;
    to_client
        .send(frame(&bytes))
        .await
        .map_err(|e| anyhow::anyhow!("send create progress: {e}"))?;
    Ok(())
}

pub async fn begin(
    to_client: &mpsc::Sender<Vec<u8>>,
    token: &str,
    title: &str,
    message: Option<&str>,
) -> Result<()> {
    let mut value = json!({
        "kind": "begin",
        "title": title,
        "cancellable": false,
    });
    if let Some(m) = message {
        value["message"] = Value::String(m.to_string());
    }
    send_notification(to_client, token, value).await
}

pub async fn report(
    to_client: &mpsc::Sender<Vec<u8>>,
    token: &str,
    percentage: Option<u32>,
    message: Option<&str>,
) -> Result<()> {
    let mut value = json!({ "kind": "report" });
    if let Some(p) = percentage {
        value["percentage"] = json!(p.min(100));
    }
    if let Some(m) = message {
        value["message"] = Value::String(m.to_string());
    }
    send_notification(to_client, token, value).await
}

pub async fn end(
    to_client: &mpsc::Sender<Vec<u8>>,
    token: &str,
    message: Option<&str>,
) -> Result<()> {
    let mut value = json!({ "kind": "end" });
    if let Some(m) = message {
        value["message"] = Value::String(m.to_string());
    }
    send_notification(to_client, token, value).await
}

async fn send_notification(
    to_client: &mpsc::Sender<Vec<u8>>,
    token: &str,
    value: Value,
) -> Result<()> {
    let notif = json!({
        "jsonrpc": "2.0",
        "method": "$/progress",
        "params": { "token": token, "value": value },
    });
    let bytes = serde_json::to_vec(&notif)?;
    to_client
        .send(frame(&bytes))
        .await
        .map_err(|e| anyhow::anyhow!("send progress notification: {e}"))?;
    Ok(())
}
