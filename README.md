# ALzed

AL language support for [Zed](https://zed.dev) — targeting feature parity with the
Microsoft VS Code AL extension for Dynamics 365 Business Central.

## Status

| Feature | Status |
|---|---|
| Syntax highlighting | working (tree-sitter-al) |
| File association (`.al`, `.dal`) | working |
| LSP transport (server starts) | working |
| Diagnostics / IntelliSense / hover / go-to-def | **blocked on protocol bridge** — see below |
| Symbols / outline | planned |
| Snippets | planned |
| Build / publish / debug commands | planned |

## Why the bridge

The Microsoft AL Language Server (`Microsoft.Dynamics.Nav.EditorServices.Host`)
does **not** speak vanilla LSP for the features that matter. It exposes a
custom `al/*` protocol — for example, IntelliSense uses `al/completions`
instead of `textDocument/completion`, and the server is inert until it
receives an `al/loadManifest` request per workspace folder.

The VS Code AL extension ships a TypeScript shim that performs this
translation. Zed cannot do it natively, so ALzed includes a small Rust process
— **`alzed-bridge`** — that sits between Zed and the AL Language Server and
translates the protocols.

See [docs/al-protocol.md](docs/al-protocol.md) for the custom method inventory
and translation plan.

## Repo layout

```
ALzed/
├── crates/
│   ├── extension/      Zed extension (compiled to wasm32-wasip2, loaded by Zed)
│   └── bridge/         alzed-bridge — native binary, runs on the host
├── docs/
│   └── al-protocol.md  Reverse-engineered AL custom LSP protocol notes
└── README.md
```

## Installation (dev)

```sh
git clone https://github.com/mhensberg2003/ALzed.git
cd ALzed
rustup target add wasm32-wasip2

# Build the bridge for your host:
cargo build --release --manifest-path crates/bridge/Cargo.toml
```

Then in Zed: `Ctrl+Shift+P` → **"zed: install dev extension"** → pick
**`crates/extension`** (not the repo root).

## Configuring the AL Language Server

The MS AL Language Server is closed-source and ships inside the official
VS Code AL extension (`ms-dynamics-smb.al`). ALzed does not redistribute it.

Typical paths after installing the VS Code AL extension:

- **Windows**: `%USERPROFILE%\.vscode\extensions\ms-dynamics-smb.al-<version>\bin\win32\Microsoft.Dynamics.Nav.EditorServices.Host.exe`
- **macOS**: `~/.vscode/extensions/ms-dynamics-smb.al-<version>/bin/darwin/Microsoft.Dynamics.Nav.EditorServices.Host`
- **Linux**: `~/.vscode/extensions/ms-dynamics-smb.al-<version>/bin/linux/Microsoft.Dynamics.Nav.EditorServices.Host`

In your Zed `settings.json`, point Zed at **`alzed-bridge`** and pass the AL
server path through it:

```jsonc
{
  "lsp": {
    "al": {
      "binary": {
        "path": "/absolute/path/to/target/release/alzed-bridge",
        "arguments": [
          "--al-server",
          "C:\\Users\\you\\.vscode\\extensions\\ms-dynamics-smb.al-17.0.2273547\\bin\\win32\\Microsoft.Dynamics.Nav.EditorServices.Host.exe"
        ]
      },
      "settings": {
        "packageCachePath": "./.alpackages",
        "enableCodeAnalysis": true
      }
    }
  }
}
```

Alternatively set `ALZED_AL_SERVER` in your environment instead of passing
`--al-server`.

To see every LSP frame the bridge handles, set `RUST_LOG=alzed_bridge=trace`.

## License

MIT. The tree-sitter grammar is MIT-licensed by its author. The Microsoft AL
Language Server is proprietary and is **not** redistributed — users must obtain
it via the official VS Code AL extension.
