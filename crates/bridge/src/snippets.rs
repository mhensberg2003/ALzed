//! Embedded AL snippets, ported from the VS Code AL extension's
//! `snippets/*.json` files (mid-2026 build). They're merged at repo build
//! time into a single `al_snippets.json` and shipped inside this binary.
//!
//! At runtime we expose them as LSP `CompletionItem`s and prepend them to
//! the AL server's `textDocument/completion` responses, so Zed surfaces the
//! same `tprocedure` / `ttable` / `tpage` shortcuts that VS Code users get,
//! without any user config.

use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::OnceLock;

const SNIPPETS_RAW: &str = include_str!("../snippets/al_snippets.json");

/// LSP CompletionItemKind value for snippets (15).
const KIND_SNIPPET: u8 = 15;
/// LSP InsertTextFormat value for snippets (2 == snippet, 1 == plain text).
const INSERT_TEXT_FORMAT_SNIPPET: u8 = 2;

#[derive(Debug, Deserialize)]
struct RawSnippet {
    prefix: String,
    body: BodyShape,
    #[serde(default)]
    description: Option<String>,
}

/// VS Code snippet bodies may be either an array of strings (one per line)
/// or a single string. Accept both.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum BodyShape {
    Lines(Vec<String>),
    OneLine(String),
}

impl BodyShape {
    fn joined(&self) -> String {
        match self {
            BodyShape::Lines(v) => v.join("\n"),
            BodyShape::OneLine(s) => s.clone(),
        }
    }
}

/// Pre-built LSP `CompletionItem` JSON values. Cheap to clone; we lazily
/// build them once on first access.
static COMPLETION_ITEMS: OnceLock<Vec<Value>> = OnceLock::new();

pub fn completion_items() -> &'static [Value] {
    COMPLETION_ITEMS
        .get_or_init(|| match build_items() {
            Ok(items) => items,
            Err(e) => {
                tracing::error!(error = %e, "failed to parse embedded AL snippets");
                Vec::new()
            }
        })
        .as_slice()
}

fn build_items() -> Result<Vec<Value>> {
    let parsed: std::collections::BTreeMap<String, RawSnippet> =
        serde_json::from_str(SNIPPETS_RAW)?;
    let mut out = Vec::with_capacity(parsed.len());
    for (name, snip) in parsed {
        let body = snip.body.joined();
        let detail = snip.description.unwrap_or_else(|| name.clone());

        // sortText with leading "0_" pushes our snippets above server
        // suggestions of equal score, mirroring VS Code's behavior where
        // ttable shows up first when the user types "tt".
        out.push(json!({
            "label": snip.prefix,
            "kind": KIND_SNIPPET,
            "detail": detail,
            "insertText": body,
            "insertTextFormat": INSERT_TEXT_FORMAT_SNIPPET,
            "filterText": snip.prefix,
            "sortText": format!("0_{}", snip.prefix),
            "documentation": {
                "kind": "markdown",
                "value": format!("```al\n{}\n```", body),
            },
        }));
    }
    Ok(out)
}

/// Merge our snippet completions into an LSP `textDocument/completion`
/// response. The result may be:
///   * `null` (server has no suggestions)
///   * an array of CompletionItem
///   * a CompletionList object `{ isIncomplete: bool, items: [...] }`
/// We normalize all three into a CompletionList and prepend our snippets.
pub fn inject_into_completion(response: &Value) -> Value {
    let snippets = completion_items();
    if snippets.is_empty() {
        return response.clone();
    }

    let mut patched = response.clone();
    let result = match patched.pointer_mut("/result") {
        Some(r) => r,
        None => return patched,
    };

    let mut existing: Vec<Value> = match result {
        Value::Null => Vec::new(),
        Value::Array(a) => std::mem::take(a),
        Value::Object(o) => o
            .get_mut("items")
            .and_then(|v| v.as_array_mut())
            .map(std::mem::take)
            .unwrap_or_default(),
        _ => return patched,
    };

    let mut merged: Vec<Value> = Vec::with_capacity(snippets.len() + existing.len());
    merged.extend(snippets.iter().cloned());
    merged.append(&mut existing);

    *result = json!({
        "isIncomplete": false,
        "items": merged,
    });
    patched
}
