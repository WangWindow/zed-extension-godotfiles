//! Optional external GDShader language server support.
//!
//! The user installs a compatible `gdshader-lsp-cpp` executable under the
//! command name `gdshader-lsp` or `gdshader_lsp`. The extension does not download,
//! build, inspect, or substitute the server binary. The server is registered as
//! opt-in in `extension.toml`, so this resolution path only runs when the user
//! explicitly enables `gdshader-lsp` for `GDShader`.

use zed_extension_api::{
    self as zed, Command,
    serde_json::Value,
    settings::{CommandSettings, LspSettings},
};

pub(crate) const SERVER_ID: &str = "gdshader-lsp";
pub(crate) const BINARY_NAME: &str = "gdshader-lsp";
pub(crate) const DEFAULT_ARGUMENTS: [&str; 2] = ["--stdio", "--log-path=none"];

/// Registration is exact: only the manifest key `gdshader-lsp` is accepted.
pub(crate) fn is_server_id(id: &str) -> bool {
    id == SERVER_ID
}

fn missing_binary_error() -> String {
    format!(
        "Neither `{BINARY_NAME}` nor `gdshader_lsp` was found on PATH. Install gdshader-lsp-cpp as `{BINARY_NAME}` or \
         set lsp.gdshader-lsp.binary.path to its executable. To disable GDShader support, set \
         languages.GDShader.language_servers to [\"gdfiles-lsp\"] in your Zed settings."
    )
}

fn empty_binary_path_error() -> String {
    format!(
        "lsp.gdshader-lsp.binary.path is empty. Set it to the exact path of an installed \
         GDShader LSP executable, or remove it to search PATH for `{BINARY_NAME}` or `gdshader_lsp`. This \
         extension does not download or build the server. To disable GDShader support again, \
         set languages.GDShader.language_servers to [\"gdfiles-lsp\"] in your Zed settings."
    )
}

/// Resolves the executable while keeping an explicit, valid path byte-for-byte
/// untouched. PATH lookup only runs when no explicit path was configured.
pub(crate) fn resolve_binary_path(
    explicit_path: Option<String>,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> Result<String, String> {
    match explicit_path {
        Some(path) if path.trim().is_empty() => Err(empty_binary_path_error()),
        Some(path) => Ok(path),
        None => lookup(BINARY_NAME)
            .or_else(|| lookup("gdshader_lsp"))
            .ok_or_else(missing_binary_error),
    }
}

/// Builds the launch command from `lsp.gdshader-lsp.binary` settings. Custom
/// arguments replace the default stdio/no-file-log list entirely, including an
/// explicit empty list.
pub(crate) fn build_command(
    binary: Option<CommandSettings>,
    lookup: impl FnMut(&str) -> Option<String>,
) -> Result<Command, String> {
    let (path, arguments, env) = match binary {
        Some(binary) => (binary.path, binary.arguments, binary.env),
        None => (None, None, None),
    };
    let command = resolve_binary_path(path, lookup)?;

    Ok(Command {
        command,
        args: arguments.unwrap_or_else(|| {
            DEFAULT_ARGUMENTS
                .iter()
                .map(|argument| argument.to_string())
                .collect()
        }),
        env: env.unwrap_or_default().into_iter().collect(),
    })
}

pub(crate) fn language_server_command(worktree: &zed::Worktree) -> Result<Command, String> {
    let settings = LspSettings::for_worktree(SERVER_ID, worktree)?;
    build_command(settings.binary, |name| worktree.which(name))
}

pub(crate) fn initialization_options(worktree: &zed::Worktree) -> Result<Option<Value>, String> {
    Ok(LspSettings::for_worktree(SERVER_ID, worktree)?.initialization_options)
}

pub(crate) fn workspace_configuration(worktree: &zed::Worktree) -> Result<Option<Value>, String> {
    Ok(LspSettings::for_worktree(SERVER_ID, worktree)?.settings)
}
