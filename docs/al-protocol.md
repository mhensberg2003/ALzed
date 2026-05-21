# Microsoft AL Language Server — Custom Protocol

The MS AL Language Server (`Microsoft.Dynamics.Nav.EditorServices.Host`) speaks
**standard LSP** for editor features — hover, completion, definition, code
actions, formatting, document symbols, etc. all go through vanilla request
types from `vscode-languageserver-protocol`.

What's custom is everything **beyond** standard LSP: workspace activation,
symbol package management, code generation, debugger, AL-specific commands.
These live under the `al/*` namespace.

The catch: the server is **inert** until the client registers each workspace's
`app.json` via `al/loadManifest`. Until then, standard LSP requests return
empty responses — looking from the outside like a broken server.

This document captures the AL custom protocol as observed from the
behavior of the official VS Code AL extension client. It is the spec the
bridge implements.

## Source of truth

The Microsoft AL Language Server itself is closed-source, but the official
VS Code AL extension (`ms-dynamics-smb.al`) ships a JavaScript client that
talks to it. Observing that client's behavior against a running server tells
us exactly what handshake and methods the server expects.

Everything in this document is derived from external observation of method
names, request/response shapes, and protocol ordering — i.e. facts about
the wire protocol needed to interoperate with the server, not the
expression of Microsoft's code. The same conclusions could be reached by
black-box tracing of the stdio LSP channel.

- Inspected client version: **17.0.2273547** (April 2026 build).
- Observation method: enumerate symbol names, then trace which params and
  response shapes appear at each call site.

## The activation handshake

This is the bit Zed cannot natively do. Without it the server stays idle.

```text
1. LSP initialize / initialized            (standard)
2. For each workspace folder F:
     read F/app.json
     client -> server  al/loadManifest  { projectFolder: F, manifest: <text of app.json> }
     server -> client  { success: true, manifest: <processed-manifest-json> }
3. Server eventually -> client  al/activeProjectLoaded  { activeProjectFolder: <uri> }
4. Server begins analysis. Standard LSP requests now return real results.
```

In prose: the client opens each workspace folder's `app.json`, sends its
text plus the folder path as an `al/loadManifest` request, and expects
`{ success, manifest }` back. The server later pushes an
`al/activeProjectLoaded` notification with the folder URI once that
project's symbol closure has loaded; only then will standard LSP requests
on that project return real results. Workspace folder removal isn't
mirrored back to `al/loadManifest` — see "Open questions" below.

### `al/loadManifest`

**Request params:**
```ts
{
  projectFolder: string;  // absolute path to the project folder (the dir containing app.json)
  manifest: string;       // raw text of app.json
}
```

**Response:**
```ts
{
  success: boolean;
  manifest: string;       // server-processed manifest JSON (may add resolved fields)
}
```

### `al/activeProjectLoaded` (server → client)

Sent by the server when it has finished loading a project's symbol closure.
After this fires, standard LSP requests for that project return real results.

```ts
{ activeProjectFolder: string }  // file:// URI
```

### `al/didChangeWorkspaceFolders` (client → server, notification)

Standard `workspace/didChangeWorkspaceFolders` is **not** what AL listens to.
Mirror folder changes here with the same `{ added, removed }` shape.

### `al/didChangeActiveDocument` (client → server, notification)

Tells the server which document is focused so it can prioritize analysis. The
VS Code client emits this when the active text editor changes.

```ts
{ uri: string }
```

## Response quirks

**Empty hover.** The MS server returns `{}` (empty object) when no hover info
is available at a position. The LSP spec says return `null`. Zed's strict
deserializer rejects `{}` with `missing field 'contents'`. The bridge must
rewrite `{}` → `null` in hover responses.

This may apply to other "optional Hover-like" responses too. Watch for it in
trace logs and add normalizers as we hit them.

## Standard LSP requests the AL server handles

These are sent unchanged through the bridge; once `al/loadManifest` is done,
they return real data.

- `textDocument/hover`
- `textDocument/completion` (and `completionItem/resolve`)
- `textDocument/definition`, `declaration`, `typeDefinition`, `implementation`
- `textDocument/references`
- `textDocument/documentHighlight`
- `textDocument/documentSymbol`
- `textDocument/formatting`, `rangeFormatting`, `onTypeFormatting`
- `textDocument/codeAction`, `codeAction/resolve`
- `textDocument/codeLens`, `codeLens/resolve`
- `textDocument/signatureHelp`
- `textDocument/documentLink`, `documentLink/resolve`
- `textDocument/documentColor`, `colorPresentation`
- `textDocument/callHierarchy/*`
- `workspace/symbol`
- `workspace/executeCommand`

`textDocument/publishDiagnostics` arrives as a server→client notification
(standard) and should reach Zed unchanged.

## `al/*` method inventory (commands, not editor features)

These are custom features beyond LSP. Bridge can pass them through or expose
as Zed slash-commands later.

### Lifecycle / workspace
- `al/loadManifest` (critical — see above)
- `al/didChangeWorkspaceFolders`
- `al/didChangeActiveDocument`
- `al/activeProjectLoaded` (server → client)
- `al/manifestMissing` (server → client)
- `al/hasProjectClosureLoadedRequest`
- `al/progressNotification` (server → client)
- `al/projectsLoadedNotification` (server → client)

### Symbols / packages
- `al/checkSymbols`
- `al/downloadSymbols`
- `al/downloadSymbolsFromGlobalSources`
- `al/getPackageDependencies`
- `al/createPackage`
- `al/fullDependencyPublish`
- `al/downloadSource`
- `al/publish`
- `al/symbolSearchRequest`

### Application object queries
- `al/getApiPages`
- `al/getApplicationObject`
- `al/getApplicationObjects`
- `al/getExtensibleEnums`
- `al/getEventPublishersRequest`
- `al/getListOfPermissionSets`
- `al/getWorkflowChain`

### Codegen / commands
- `al/generatePermissionSet`
- `al/generatePermissionSetInALObject`
- `al/getErrorTemplate`
- `al/openPageDesigner`
- `al/openEventRecorder`
- `al/openUri`
- `al/runObject`
- `al/refreshObjectsEvent`
- `al/setSymbolMembers`
- `al/setSymbolProperty`

### Debugger
- `al/debuggerConsoleCompletionRequest`
- `al/completions` (**debugger console** completions — NOT regular completion)
- `al/provideCompletions` (debugger)
- `al/initializeSnapshotDebuggerAttach`
- `al/finishSnapshotDebuggerSessionRequest`
- `al/generatecpuprofile`

### Auth
- `al/checkAuthenticated`
- `al/clearCredentialsCache`
- `al/deviceLogin`
- `al/launchDeviceLoginWindow`

### Testing
- `al/discoverTests`

### Telemetry
- `al/didChangeTelemetrySettings`
- `al/getTelemetrySettings`
- `al/getNstSessionInfo`

## Bridge implementation priority

In rough order of value-to-effort:

1. ✅ Stdio passthrough with frame tracing (done in v0.1).
2. **AL init handshake** — open each workspace's `app.json`, send
   `al/loadManifest` after standard `initialized`. Watch for
   `al/activeProjectLoaded`. **This alone is expected to unblock diagnostics,
   hover, completion, definition** — the headline parity features.
3. **`{}` hover normalizer** — rewrite empty-object responses to `null`.
4. **`al/didChangeWorkspaceFolders` mirror** — when standard
   `workspace/didChangeWorkspaceFolders` arrives, also send the AL variant.
5. **`al/didChangeActiveDocument` push** — driven by something Zed exposes
   (initial open / didOpen of `.al` files).
6. Filter unknown notifications (`al/progressNotification` etc.) to
   `window/logMessage` so Zed sees server progress and doesn't error.
7. Commands (codegen, downloadSymbols) — exposed as Zed extension
   slash-commands later.

## Open questions

- Does `al/loadManifest` need the full manifest text every time, or just the
  URI? (Bundle clearly passes full text — go with that.)
- Does the server send custom `window/logMessage` traffic we should keep, or
  noisy notifications we should drop?
- Are diagnostics scoped per-project — i.e., do we have to send
  `al/loadManifest` again after VS Code/Zed reopens?
- How does workspace folder removal interact with `al/loadManifest`? Is there
  a corresponding `al/unloadManifest` or does `al/didChangeWorkspaceFolders`
  cover it? (Need to observe the unload path in the VS Code client.)

Each becomes a quick experiment once the bridge is running against a real
session.
