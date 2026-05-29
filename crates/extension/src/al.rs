use zed_extension_api::{
    self as zed, settings::LspSettings, LanguageServerId, Result,
};

struct AlExtension;

impl AlExtension {
    // ALzed never launches the MS AL server directly — it always launches the
    // native `alzed-bridge`, which performs the AL handshake and proxies LSP to
    // the MS server. Every resolution path below therefore points at the bridge,
    // not the raw `Microsoft.Dynamics.Nav.EditorServices.Host`.
    fn resolve_language_server_binary(
        &self,
        worktree: &zed::Worktree,
    ) -> Result<(String, Vec<String>)> {
        let lsp_settings = LspSettings::for_worktree("al", worktree).ok();

        // Preferred: user points `lsp.al.binary.path` at alzed-bridge and passes
        // `--al-server <MS host>` via `binary.arguments`.
        if let Some(settings) = &lsp_settings {
            if let Some(binary) = settings.binary.as_ref() {
                if let Some(path) = binary.path.clone() {
                    let args = binary.arguments.clone().unwrap_or_default();
                    return Ok((path, args));
                }
            }
        }

        // Fallback: path to the bridge supplied via env. The bridge locates the
        // MS server itself via its `--al-server` arg or the ALZED_AL_SERVER env.
        let env = worktree.shell_env();
        if let Some(path) = env_var(&env, "ALZED_BRIDGE_PATH") {
            return Ok((path, vec![]));
        }

        // Fallback: bridge on PATH. The user must set ALZED_AL_SERVER so the
        // bridge can find the MS server, since no `--al-server` arg is passed.
        if let Some(path) = worktree.which("alzed-bridge") {
            return Ok((path, vec![]));
        }

        Err(missing_server_message())
    }
}

impl zed::Extension for AlExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        _id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        let (command, args) = self.resolve_language_server_binary(worktree)?;
        Ok(zed::Command {
            command,
            args,
            env: vec![],
        })
    }

    fn language_server_workspace_configuration(
        &mut self,
        _id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<serde_json::Value>> {
        let user_settings = LspSettings::for_worktree("al", worktree)
            .ok()
            .and_then(|s| s.settings)
            .unwrap_or_else(|| serde_json::json!({}));

        Ok(Some(serde_json::json!({ "al": user_settings })))
    }

    fn language_server_initialization_options(
        &mut self,
        _id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<serde_json::Value>> {
        let init = LspSettings::for_worktree("al", worktree)
            .ok()
            .and_then(|s| s.initialization_options);
        Ok(init)
    }
}

fn env_var(env: &[(String, String)], key: &str) -> Option<String> {
    env.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .filter(|v| !v.is_empty())
}

fn missing_server_message() -> String {
    [
        "alzed-bridge not found.",
        "",
        "ALzed runs a small native binary, alzed-bridge, that performs the AL",
        "handshake and proxies LSP to Microsoft's AL Language Server. Both are",
        "installed separately (neither is bundled). Point Zed at the bridge and",
        "tell it where the MS AL server lives:",
        "",
        "  \"lsp\": {",
        "    \"al\": {",
        "      \"binary\": {",
        "        \"path\": \"/absolute/path/to/alzed-bridge\",",
        "        \"arguments\": [",
        "          \"--al-server\",",
        "          \"/path/to/Microsoft.Dynamics.Nav.EditorServices.Host\"",
        "        ]",
        "      }",
        "    }",
        "  }",
        "",
        "Alternatively, put alzed-bridge on your PATH and export ALZED_AL_SERVER",
        "(pointing at the MS AL server), or set ALZED_BRIDGE_PATH to the bridge.",
        "",
        "Get the bridge from the GitHub release (or build it); the MS AL server",
        "ships inside the official VS Code AL extension (ms-dynamics-smb.al) under",
        "bin/<platform>/. See README.md for the full setup.",
    ]
    .join("\n")
}

zed::register_extension!(AlExtension);
