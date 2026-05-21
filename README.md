# ALzed

AL language support for [Zed](https://zed.dev) — Microsoft Dynamics 365
Business Central development without leaving your editor of choice.

ALzed gives Zed feature parity with the daily inner loop of the official
VS Code AL extension: highlighting, IntelliSense, diagnostics, symbol
management, build, publish, run-in-browser, and inline snippets. The
only thing it doesn't do yet is debug (F5).

## Status

| Feature | Status |
|---|---|
| Syntax highlighting (`.al`, `.dal`) | ✅ |
| Hover, completion, go-to-definition | ✅ |
| Diagnostics (errors, warnings, code analyzers) | ✅ |
| Symbol package download (`al/downloadSymbols`) | ✅ |
| Symbol package check (`al/checkSymbols`) | ✅ |
| Build (`al/createPackage`) | ✅ |
| Publish (`al/fullDependencyPublish`) | ✅ |
| Run object → BC web client | ✅ |
| Snippets (`ttable`, `tpage`, `tcodeunit`, ...) | ✅ — 28 hand-authored |
| Toast feedback on command results | ✅ |
| `$/progress` notifications for long ops | ✅ |
| Debug (F5 via DAP) | ⛔ planned |
| Test runner | ⛔ planned |

## How it works

The Microsoft AL Language Server
(`Microsoft.Dynamics.Nav.EditorServices.Host.exe`) doesn't speak vanilla
LSP. It needs a custom `al/loadManifest` + `al/setActiveWorkspace`
handshake before it'll respond to anything, and most features go through
custom `al/*` methods rather than standard LSP ones. VS Code's AL
extension ships a TypeScript shim that translates between the two.

Zed extensions are sandboxed WASM modules, so they can't run the AL
server directly. ALzed solves this with a small native Rust binary,
**`alzed-bridge`**, that sits between Zed and the AL server:

```
Zed  <--stdio LSP-->  alzed-bridge  <--stdio LSP + al/*-->  AL Language Server
```

The bridge:

- Performs the AL handshake on workspace open (loadManifest +
  workspace/didChangeConfiguration + setActiveWorkspace).
- Normalizes the AL server's quirks (empty `{}` hovers → `null`,
  string IDs → numeric, etc.).
- Injects code actions on `.al` files: Download symbols, Check
  symbols, Build, Publish, Run.
- Translates `al/progressNotification` → `$/progress`, and wraps each
  user-facing command in its own progress task so Zed shows a status-bar
  spinner during the otherwise-silent multi-second operations.
- Prepends an inline snippet library to every `textDocument/completion`
  response, so `ttable<Tab>` works out of the box with no user config.

See [`docs/al-protocol.md`](docs/al-protocol.md) for the
reverse-engineered protocol notes.

## Repo layout

```
ALzed/
├── crates/
│   ├── extension/       Zed extension (wasm32-wasip2)
│   │   ├── extension.toml
│   │   ├── languages/al/{config.toml,highlights.scm}
│   │   └── src/al.rs    Tells Zed how to launch alzed-bridge
│   └── bridge/          alzed-bridge — native Rust binary
│       ├── src/{main.rs,snippets.rs,progress.rs}
│       └── snippets/al_snippets.json
├── docs/
│   └── al-protocol.md   Custom al/* protocol notes
└── README.md
```

## Installation

ALzed has two parts that need to be set up separately: the **Zed
extension** (loaded as a dev extension) and the **bridge binary**
(native, runs alongside Zed). There are two install paths — pick one:

- **[Prebuilt (recommended)](#install-from-prebuilt-release)**: download
  the bridge and extension tarball from the latest
  [GitHub Release](https://github.com/mhensberg2003/ALzed/releases).
- **[Build from source](#install-by-building-from-source)**: clone the
  repo and `cargo build`. Useful if you want to hack on ALzed itself or
  a release for your platform isn't published yet.

### Install from prebuilt release

1. Go to <https://github.com/mhensberg2003/ALzed/releases/latest>.
2. Download the bridge binary for your OS:
   - `alzed-bridge-windows-x86_64.exe`
   - `alzed-bridge-linux-x86_64`
   - `alzed-bridge-macos-arm64`
   - `alzed-bridge-macos-x86_64`
3. Move it to a stable path you'll reference later, e.g.
   `C:\Users\you\bin\alzed-bridge.exe` or `~/bin/alzed-bridge`.
   On macOS/Linux, mark it executable: `chmod +x ~/bin/alzed-bridge`.
4. Download `alzed-zed-extension.tar.gz` from the same release and
   extract it somewhere stable (e.g. `~/zed-extensions/al/`). You should
   see an `extension.toml` at the root of the extracted folder.
5. In Zed: `Ctrl+Shift+P` → **"zed: install dev extension"** → pick
   the extracted folder.
6. Continue to [Locate the AL Language Server](#locate-the-al-language-server)
   then [Wire it together in `settings.json`](#wire-it-together-in-settingsjson).

### Install by building from source

#### 1. Prerequisites

- Rust toolchain (`rustup`).
- `wasm32-wasip2` target: `rustup target add wasm32-wasip2`.
- The official VS Code AL extension installed — that's how you obtain
  the MS AL Language Server itself. ALzed does not redistribute it.
- (Linux/WSL → Windows only) `mingw-w64` for cross-compiling to
  `x86_64-pc-windows-gnu`: `sudo apt install mingw-w64` and
  `rustup target add x86_64-pc-windows-gnu`.

#### 2. Build the bridge

The bridge needs to be compiled for the OS where Zed runs (where the AL
server runs is the same OS in practice).

**Windows host, building on Windows:**

```sh
cargo build --release --manifest-path crates/bridge/Cargo.toml
# → crates/bridge/target/release/alzed-bridge.exe
```

**Windows host, cross-compiling from WSL/Linux:**

```sh
cd crates/bridge
cargo build --release --target x86_64-pc-windows-gnu
# → target/x86_64-pc-windows-gnu/release/alzed-bridge.exe
```

**macOS or Linux host (native Zed):**

```sh
cargo build --release --manifest-path crates/bridge/Cargo.toml
# → crates/bridge/target/release/alzed-bridge
```

Copy the resulting binary to a stable path (e.g. `~/bin/alzed-bridge`
or `C:\Users\you\bin\alzed-bridge.exe`) so you can reference it from
`settings.json`.

#### 3. Install the Zed extension

In Zed: `Ctrl+Shift+P` → **"zed: install dev extension"** → pick the
`crates/extension` directory (not the repo root).

> ⚠️ On Windows, install from a native Windows path
> (`C:\Users\you\ALzed\crates\extension`). UNC paths like
> `\\wsl.localhost\Ubuntu\...` cause the dev-extension build to fail.

### Locate the AL Language Server

After installing the VS Code AL extension, the server lives at:

- **Windows**: `%USERPROFILE%\.vscode\extensions\ms-dynamics-smb.al-<version>\bin\win32\Microsoft.Dynamics.Nav.EditorServices.Host.exe`
- **macOS**: `~/.vscode/extensions/ms-dynamics-smb.al-<version>/bin/darwin/Microsoft.Dynamics.Nav.EditorServices.Host`
- **Linux**: `~/.vscode/extensions/ms-dynamics-smb.al-<version>/bin/linux/Microsoft.Dynamics.Nav.EditorServices.Host`

### Wire it together in `settings.json`

```jsonc
{
  "lsp": {
    "al": {
      "binary": {
        "path": "C:\\Users\\you\\bin\\alzed-bridge.exe",
        "arguments": [
          "--al-server",
          "C:\\Users\\you\\.vscode\\extensions\\ms-dynamics-smb.al-17.0.2273547\\bin\\win32\\Microsoft.Dynamics.Nav.EditorServices.Host.exe"
        ]
      },
      "settings": {
        "packageCachePath": "./.alpackages",
        "enableCodeAnalysis": false
      }
    }
  }
}
```

Or set `ALZED_AL_SERVER` in your environment and drop the
`--al-server` arg.

## Using ALzed

Open any AL workspace (a folder with an `app.json`). Once Zed connects,
the bridge runs the AL handshake automatically. Diagnostics should
appear within seconds.

### Code actions

Press `Ctrl+.` on any `.al` file to surface ALzed's commands:

| Action | What it does |
|---|---|
| AL: Download symbols | Pulls symbol packages for all dependencies into `.alpackages/` |
| AL: Check symbols | Verifies all symbol packages are present and up to date |
| AL: Build package | Compiles the project into a `.app` file |
| AL: Publish | Builds + pushes to the BC server in `launch.json` |
| AL: Run object | Opens the configured `startupObject` in the BC web client |

Every command emits a toast on completion and a `$/progress` spinner
while running, so you can see what's happening.

### Snippets

Start typing one of these prefixes in a `.al` file:

```
ttable       tpage         tpagecard     tpageext     tcodeunit
treport      tquery        txmlport      tenum        tenumext
tinterface   tprocedure    tlocalproc    tinternalproc
ttrigger     tif           tcase         tfor         tforeach
twhile       trepeat       terror        ttest        teventsub
tfield       tpageaction   tpagefield    ttableext
```

They appear at the top of the completion menu with tab-stop placeholders.

### `launch.json`

ALzed reads `.vscode/launch.json` for Publish and Run. The first
configuration in the array is used. Minimal example for a cloud
sandbox:

```jsonc
{
  "version": "0.2.0",
  "configurations": [
    {
      "type": "al",
      "request": "launch",
      "name": "Sandbox",
      "environmentType": "Sandbox",
      "environmentName": "MyEnv",
      "tenant": "<tenant-guid>",
      "startupObjectType": "Page",
      "startupObjectId": 50100,
      "startupCompany": "CRONUS USA, Inc."
    }
  ]
}
```

## Troubleshooting

### Nothing happens / no diagnostics

Open Zed's LSP log (`Ctrl+Shift+P` → "zed: open lsp logs") and look for
`alzed-bridge` output. Useful environment variables for the bridge:

- `RUST_LOG=alzed_bridge=debug` — handshake details.
- `RUST_LOG=alzed_bridge=trace` — every LSP frame.

Set them via the `env` field in `settings.json` under the `al` language
server entry.

### `alzed-bridge.exe` is locked when redeploying

Zed sometimes leaves `alzed-bridge.exe` and one or more
`Microsoft.Dynamics.Nav.EditorServices.Host.exe` processes running
after closing. If `cp` / `Move-Item` can't overwrite the bridge on
update, kill the stale processes:

```powershell
taskkill /F /IM alzed-bridge.exe
taskkill /F /IM Microsoft.Dynamics.Nav.EditorServices.Host.exe
```

### Symbol download hangs

The MS AL server downloads symbols from BC's package feed; this takes
~30 s for a fresh project (≈100 MB of symbol packages). The
`$/progress` spinner in Zed's status bar should be active the whole
time. If it never finishes, check `RUST_LOG=alzed_bridge=debug` for
auth errors from the feed.

### Run object opens the wrong URL

ALzed builds the URL from `launch.json`'s `startupObjectType`,
`startupObjectId`, `environmentType`, `environmentName`, `server`,
`serverInstance`, `tenant`, and `startupCompany`. Double-check these
match what you'd configure in VS Code.

## License

MIT (see [LICENSE](LICENSE)). The tree-sitter grammar is MIT-licensed by
its author. The bundled snippets are hand-authored for this project.
The Microsoft AL Language Server is proprietary and is **not**
redistributed — users obtain it via the official VS Code AL extension.

## Trademarks and disclaimer

ALzed is an independent, community-built project. It is **not affiliated
with, endorsed by, or sponsored by Microsoft Corporation, Zed Industries,
or any of their subsidiaries**.

"Microsoft", "Dynamics 365", "Business Central", "Visual Studio Code",
and "AL" (in the context of the AL language) are trademarks of Microsoft
Corporation. "Zed" is a trademark of Zed Industries. All other trademarks
are the property of their respective owners. Trademarks are used here in
their nominative sense, solely to describe interoperability and
compatibility — the use of these names does not imply any partnership or
authorization.

ALzed interacts with Microsoft's AL Language Server only through the
public LSP-style stdio protocol it exposes. No proprietary Microsoft
binaries or source files are bundled, modified, or redistributed by this
project. To use ALzed, you must independently install the official
Microsoft VS Code AL extension under the license terms set by Microsoft.
