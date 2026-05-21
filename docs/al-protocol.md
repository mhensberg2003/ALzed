# Microsoft AL Language Server — Custom Protocol

The MS AL Language Server (`Microsoft.Dynamics.Nav.EditorServices.Host`) speaks
a superset of LSP. For most features it exposes proprietary `al/*` methods
instead of (or in addition to) the standard LSP equivalents.

This document captures what we learn by inspecting the bundled JS client in
`ms-dynamics-smb.al/dist/extension.js`. It is the spec we implement against in
`crates/bridge`.

## Source of truth

- VS Code AL extension: `ms-dynamics-smb.al` (proprietary, but the JS client is
  shipped readable inside the VSIX/installed extension folder).
- Inspected version at time of writing: **17.0.2273547**.
- Path on disk:
  `~/.vscode/extensions/ms-dynamics-smb.al-17.0.2273547/dist/extension.js`

## Method inventory (incomplete — populate as we map each one)

The methods below were extracted by grepping for `"al/...":` and
`sendRequest("al/...")` patterns in the bundle. **TODO** for each: capture the
exact request params shape and response shape from the JS bundle and add a
TypeScript-style signature.

### Lifecycle / workspace

| Method | Kind | Notes |
|---|---|---|
| `al/loadManifest` | request | Tells the server about an `app.json`. Must be called per workspace folder before the server will analyze anything. **Init handshake critical.** |
| `al/didChangeWorkspaceFolders` | notification (?) | Replaces standard `workspace/didChangeWorkspaceFolders` for AL. |
| `al/didChangeActiveDocument` | notification | Tells the server which document is focused — feeds the "active project" concept. |
| `al/activeProjectLoaded` | notification (server→client) | Server signals it has loaded a project. |
| `al/manifestMissing` | notification (server→client?) | Server signals a missing `app.json`. |
| `al/hasProjectClosureLoadedRequest` | request | Probe — has the project's dependency closure been loaded? |

### IntelliSense (replaces vanilla LSP methods)

| Method | Kind | Replaces |
|---|---|---|
| `al/completions` | request | `textDocument/completion` |
| `al/gotodefinition` | request | `textDocument/definition` |

(Hover appears to use vanilla `textDocument/hover` but the server returns `{}`
instead of `null` when no info — we'll need a small response normalizer.)

### Symbols / packages

| Method | Notes |
|---|---|
| `al/checkSymbols` | Validates `.alpackages` cache integrity. |
| `al/downloadSymbols` | Pulls `.app` symbol files from configured BC server. |
| `al/downloadSymbolsFromGlobalSources` | Variant pulling from public symbol feeds. |
| `al/getPackageDependencies` | Returns dependency graph for the loaded app. |
| `al/createPackage` | Build / packaging command. |

### Application object queries

| Method | Notes |
|---|---|
| `al/getApiPages` | Lists API page objects. |
| `al/getApplicationObject` | Get a single object by id/type. |
| `al/getApplicationObjects` | List objects. |
| `al/getExtensibleEnums` | Used for enum extension scaffolding. |
| `al/getEventPublishersRequest` | Used for event subscriber scaffolding. |
| `al/getListOfPermissionSets` | Permission set enumeration. |
| `al/getWorkflowChain` | Workflow definitions. |

### Code generation / commands

| Method | Notes |
|---|---|
| `al/generatePermissionSet` | Generates `.permissionset.al`. |
| `al/generatePermissionSetInALObject` | Inline permission scaffolding. |
| `al/getErrorTemplate` | Error template generation. |
| `al/openPageDesigner` | Launches Page Designer UI (likely returns a URI for client to open). |
| `al/openEventRecorder` | Launches Event Recorder UI. |
| `al/openUri` | Generic URI open delegate. |

### Debugger

| Method | Notes |
|---|---|
| `al/debuggerConsoleCompletionRequest` | Completions in debug REPL. |
| `al/initializeSnapshotDebuggerAttach` | Snapshot debugger attach. |
| `al/finishSnapshotDebuggerSessionRequest` | Snapshot session teardown. |
| `al/generatecpuprofile` | CPU profile generation. |

### Auth

| Method | Notes |
|---|---|
| `al/checkAuthenticated` | OAuth state check. |
| `al/clearCredentialsCache` | Clear cached token. |
| `al/deviceLogin` | Device-code login flow. |
| `al/launchDeviceLoginWindow` | Client should open browser for OAuth. |

### Testing

| Method | Notes |
|---|---|
| `al/discoverTests` | Enumerate test codeunits. |

### Telemetry

| Method | Notes |
|---|---|
| `al/didChangeTelemetrySettings` | Pushes user telemetry prefs. |
| `al/getTelemetrySettings` | Pull current settings. |
| `al/getNstSessionInfo` | NST diagnostics. |

### Publish / deploy

| Method | Notes |
|---|---|
| `al/fullDependencyPublish` | Publish app + dependencies to a BC server. |
| `al/downloadSource` | Download source for a published app. |

## Implementation priority (drives bridge work)

1. **`al/loadManifest`** — without this, the server is inert. Bridge must call
   it for each workspace folder once standard `initialize`/`initialized`
   completes.
2. **`al/didChangeWorkspaceFolders`** — keep the server in sync as folders are
   added/removed.
3. **Hover `{}` normalization** — translate empty `{}` hover responses to
   `null` so Zed's deserializer doesn't error.
4. **`textDocument/completion` -> `al/completions`** — request and response
   translation. Highest user-value standard feature.
5. **`textDocument/definition` -> `al/gotodefinition`** — same.
6. **`al/didChangeActiveDocument`** — wire to Zed's active-buffer changes so
   the server can prioritize analysis.
7. **`al/checkSymbols`** at init — surfaces missing-symbol warnings early.

The rest (codegen, debugger, auth, telemetry) become commands or are wired in
later phases.

## Open questions

- Are `al/loadManifest` params just `{ uri: string }` or does the server want
  the parsed manifest contents?
- Does the server send `al/activeProjectLoaded` *before* it'll respond to any
  IntelliSense request? If yes, the bridge should treat it as a readiness gate
  and buffer client requests until it fires.
- Does `al/completions` accept the same params shape as
  `textDocument/completion` or does it use a custom shape?

Each becomes a tracing experiment once the passthrough bridge is running and we
can observe a real session.
