# ALzed

AL language support for [Zed](https://zed.dev) — targeting feature parity with the
Microsoft VS Code AL extension for Dynamics 365 Business Central.

## Status: v0.1 (early)

| Feature | Status |
|---|---|
| Syntax highlighting | working (tree-sitter-al) |
| Indentation | working |
| File association (`.al`, `.dal`) | working |
| LSP — diagnostics / IntelliSense / hover / go-to-def | wired, requires binary (see below) |
| Symbols / outline | planned |
| Snippets | planned |
| Build / publish / debug commands | planned |
| Symbol package management (`.app` files) | planned |

## Installation (dev)

```sh
git clone <this repo> ALzed
cd ALzed
rustup target add wasm32-wasip2
```

Then in Zed: `cmd-shift-p` → **"zed: install dev extension"** → pick this directory.

## Configuring the AL Language Server

The Microsoft AL Language Server is closed-source and ships inside the official
VS Code AL extension (`ms-dynamics-smb.al`). ALzed does not redistribute it.

Locate the binary on your machine — typically:

- **Windows**: `%USERPROFILE%\.vscode\extensions\ms-dynamics-smb.al-<version>\bin\win32\Microsoft.Dynamics.Nav.EditorServices.Host.exe`
- **macOS**: `~/.vscode/extensions/ms-dynamics-smb.al-<version>/bin/darwin/Microsoft.Dynamics.Nav.EditorServices.Host`
- **Linux**: `~/.vscode/extensions/ms-dynamics-smb.al-<version>/bin/linux/Microsoft.Dynamics.Nav.EditorServices.Host`

Then point Zed at it. Open `~/.config/zed/settings.json`:

```jsonc
{
  "lsp": {
    "al": {
      "binary": {
        "path": "/absolute/path/to/Microsoft.Dynamics.Nav.EditorServices.Host"
      },
      "settings": {
        "packageCachePath": "./.alpackages",
        "enableCodeAnalysis": true
      }
    }
  }
}
```

Alternatively export `AL_LANGUAGE_SERVER_PATH` in your shell.

## Architecture

- **Grammar**: [`SShadowS/tree-sitter-al`](https://github.com/SShadowS/tree-sitter-al)
  pinned by commit. Fetched at install time by Zed.
- **Language server**: wraps Microsoft's `Microsoft.Dynamics.Nav.EditorServices.Host`
  over stdio LSP.
- **Settings**: surfaced under the `al` namespace via `workspace/configuration` so
  the MS server picks up `packageCachePath`, `codeAnalyzers`, `enableCodeAnalysis`,
  `ruleSetPath`, etc. exactly as it would in VS Code.

## License

MIT. The tree-sitter grammar is MIT-licensed by its author. The Microsoft AL
Language Server is proprietary and is **not** redistributed — users must obtain
it via the official VS Code AL extension.
