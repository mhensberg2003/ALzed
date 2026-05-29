Adds **AL** — language support for Microsoft Dynamics 365 Business Central
(the AL language), as used in the official VS Code AL extension.

Repo: https://github.com/mhensberg2003/ALzed

## What it provides

Syntax highlighting (`.al`/`.dal`), hover/completion/go-to-definition,
diagnostics, symbol package management, build, publish, run-in-browser, and
inline snippets — feature parity with the daily inner loop of the VS Code AL
extension (everything except debug/F5).

## Architecture note (please read — explains the user-installed binaries)

The AL Language Server is Microsoft's proprietary
`Microsoft.Dynamics.Nav.EditorServices.Host`. It is **not redistributable**
(MS EULA) and does not speak vanilla LSP — it requires a custom
`al/loadManifest` + `al/setActiveWorkspace` handshake.

So this extension does **not** bundle or download a language server. It is a
thin WASM shim that launches a small native binary, `alzed-bridge`, which the
user installs separately; the bridge performs the AL handshake and proxies LSP
to the user's own copy of the MS AL server (obtained via the official VS Code
AL extension).

This matches the registry's "check for the language server in the user's
environment" pattern already used by extensions like **veryl** (`veryl-ls`),
**slint** (`slint-lsp`), **ocaml** (`ocamllsp`), and **crystal**. The only
difference is the underlying LSP is proprietary and user-supplied — which the
policy permits, since nothing is bundled. If the bridge isn't found, the
extension returns a clear, actionable error with setup instructions.

## Checklist

- [x] Submodule added at `extensions/al` (HTTPS, tracking `main`).
- [x] `extensions.toml` entry with `path = "crates/extension"` and matching `version`.
- [x] `pnpm sort-extensions` run.
- [x] Recognized `LICENSE` (MIT) at the manifest path.
- [x] No bundled language server; grammar fetched via `[grammars.al]` repo+commit.
