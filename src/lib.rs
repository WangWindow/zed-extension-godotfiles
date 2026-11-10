mod gdscript;
mod gdshader;
mod installer;

use zed_extension_api::{self as zed, LanguageServerId, Result, settings::LspSettings};

use gdscript::LanguageServerKind;

struct GodotFilesExtension;

impl zed::Extension for GodotFilesExtension {
    fn new() -> Self {
        Self
    }

    fn language_server_command(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<zed::Command> {
        if gdshader::is_server_id(language_server_id.as_ref()) {
            return gdshader::language_server_command(worktree);
        }

        let kind = gdscript::language_server_kind(language_server_id.as_ref())?;

        match kind {
            LanguageServerKind::Resource => {
                let binary = LspSettings::for_worktree(gdscript::RESOURCE_SERVER_ID, worktree)
                    .ok()
                    .and_then(|settings| settings.binary);
                let path = installer::resolve_binary_path(
                    binary.as_ref().and_then(|binary| binary.path.clone()),
                    worktree,
                )?;

                Ok(zed::Command {
                    command: path,
                    args: binary
                        .as_ref()
                        .and_then(|binary| binary.arguments.clone())
                        .unwrap_or_else(|| vec!["--stdio".to_string()]),
                    env: binary
                        .and_then(|binary| binary.env)
                        .unwrap_or_default()
                        .into_iter()
                        .collect(),
                })
            }
            LanguageServerKind::Gdscript => {
                let settings = LspSettings::for_worktree(gdscript::GDSCRIPT_SERVER_ID, worktree)?;
                let binary = settings.binary;
                let path = installer::resolve_binary_path(
                    binary.as_ref().and_then(|binary| binary.path.clone()),
                    worktree,
                )?;
                let args = gdscript::language_server_arguments(
                    binary.as_ref().and_then(|binary| binary.arguments.clone()),
                    settings.settings.as_ref(),
                )?;

                Ok(zed::Command {
                    command: path,
                    args,
                    env: binary
                        .and_then(|binary| binary.env)
                        .unwrap_or_default()
                        .into_iter()
                        .collect(),
                })
            }
        }
    }

    fn language_server_initialization_options(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        if gdshader::is_server_id(language_server_id.as_ref()) {
            return gdshader::initialization_options(worktree);
        }

        let server_id = match gdscript::language_server_kind(language_server_id.as_ref()) {
            Ok(LanguageServerKind::Resource) => gdscript::RESOURCE_SERVER_ID,
            Ok(LanguageServerKind::Gdscript) => gdscript::GDSCRIPT_SERVER_ID,
            Err(_) => return Ok(None),
        };

        Ok(LspSettings::for_worktree(server_id, worktree)
            .ok()
            .and_then(|settings| settings.initialization_options))
    }

    fn language_server_workspace_configuration(
        &mut self,
        language_server_id: &LanguageServerId,
        worktree: &zed::Worktree,
    ) -> Result<Option<zed::serde_json::Value>> {
        if gdshader::is_server_id(language_server_id.as_ref()) {
            return gdshader::workspace_configuration(worktree);
        }

        // The GDScript bridge keeps its settings in `lsp.gdscript.settings`;
        // they are translated into CLI arguments and must not be forwarded to
        // the server as workspace configuration.
        Ok(None)
    }

    fn get_dap_binary(
        &mut self,
        adapter_name: String,
        config: zed::DebugTaskDefinition,
        _user_provided_debug_adapter_path: Option<String>,
        _worktree: &zed::Worktree,
    ) -> Result<zed::DebugAdapterBinary, String> {
        gdscript::build_dap_binary(&adapter_name, &config)
    }

    fn dap_request_kind(
        &mut self,
        adapter_name: String,
        config: zed::serde_json::Value,
    ) -> Result<zed::StartDebuggingRequestArgumentsRequest, String> {
        gdscript::dap_request_kind(&adapter_name, config)
    }
}

zed::register_extension!(GodotFilesExtension);
