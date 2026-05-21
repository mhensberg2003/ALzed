use zed_extension_api::{
    self as zed, settings::LspSettings, LanguageServerId, Result,
};

struct AlExtension;

impl AlExtension {
    fn resolve_language_server_binary(
        &self,
        worktree: &zed::Worktree,
    ) -> Result<(String, Vec<String>)> {
        let lsp_settings = LspSettings::for_worktree("al", worktree).ok();

        if let Some(settings) = &lsp_settings {
            if let Some(binary) = settings.binary.as_ref() {
                if let Some(path) = binary.path.clone() {
                    let args = binary.arguments.clone().unwrap_or_default();
                    return Ok((path, args));
                }
            }
        }

        let env = worktree.shell_env();
        if let Some(path) = env_var(&env, "AL_LANGUAGE_SERVER_PATH") {
            return Ok((path, vec![]));
        }

        for candidate in [
            "Microsoft.Dynamics.Nav.EditorServices.Host",
            "al-language-server",
        ] {
            if let Some(path) = worktree.which(candidate) {
                return Ok((path, vec![]));
            }
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
        "AL Language Server binary not found.",
        "",
        "Configure it in your Zed settings:",
        "  \"lsp\": {",
        "    \"al\": {",
        "      \"binary\": {",
        "        \"path\": \"/absolute/path/to/Microsoft.Dynamics.Nav.EditorServices.Host\"",
        "      }",
        "    }",
        "  }",
        "",
        "Or export AL_LANGUAGE_SERVER_PATH in your shell.",
        "",
        "The binary ships inside the official VS Code AL extension (ms-dynamics-smb.al)",
        "under bin/<platform>/. See README.md for details.",
    ]
    .join("\n")
}

zed::register_extension!(AlExtension);
