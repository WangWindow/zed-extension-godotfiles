//! Native stdio JSON-RPC server.
//!
//! The protocol layer deliberately owns only transport concerns: lifecycle,
//! document overlays, capability negotiation, watcher registration, request
//! validation and Markdown rendering. All resource resolution lives in the core
//! `ResourceIndex` contract; all image work lives in [`crate::preview`].

use std::{
    collections::HashMap,
    error::Error,
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};

use lsp_server::{Connection, Message, Notification, Request, RequestId, Response};
use serde_json::{Value, json};
use url::Url;

use crate::{
    Position, Range, Resolution, ResourceIndex, Target,
    preview::{self, PreviewCache},
    project_root,
};

pub type DynError = Box<dyn Error + Send + Sync>;

const INVALID_PARAMS: i32 = -32602;
const METHOD_NOT_FOUND: i32 = -32601;

/// Bounded read for an open/requested text document. Core limits its own
/// metadata reads; the transport must not slurp unbounded scene files either.
const MAX_DOCUMENT_BYTES: u64 = 4 * 1024 * 1024;
const TEXT_SNIFF_BYTES: usize = 4 * 1024;
const MAX_HOVER_BYTES: usize = 100_000;
const MAX_HOVER_TEXT_BYTES: usize = 16 * 1024;
const MAX_MARKDOWN_FIELD_BYTES: usize = 2 * 1024;
const MAX_TARGETS_IN_HOVER: usize = 32;

const WATCH_GLOBS: &[&str] = &[
    "**/*.tscn",
    "**/*.tres",
    "**/*.gdshader",
    "**/*.gdshaderinc",
    "**/*.uid",
    "**/*.import",
    "**/*.godot",
    "**/uid_cache.bin",
];

/// LSP `FileSystemWatcher.kind`: create | change | delete.
const WATCH_KIND_ALL: u32 = 7;

#[derive(Debug)]
struct InitConfig {
    enable_preview: bool,
    dynamic_watch_registration: bool,
    relative_pattern_support: bool,
    workspace_folder_uris: Vec<String>,
}

impl InitConfig {
    fn from_initialize(params: &Value) -> Self {
        let enable_preview = params
            .pointer("/initializationOptions/enablePreview")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let dynamic_watch_registration = params
            .pointer("/capabilities/workspace/didChangeWatchedFiles/dynamicRegistration")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let relative_pattern_support = params
            .pointer("/capabilities/workspace/didChangeWatchedFiles/relativePatternSupport")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut workspace_folder_uris = Vec::new();
        if let Some(folders) = params.get("workspaceFolders").and_then(Value::as_array) {
            for folder in folders {
                if let Some(uri) = folder.get("uri").and_then(Value::as_str)
                    && valid_file_uri(uri)
                {
                    workspace_folder_uris.push(uri.to_string());
                }
            }
        }
        if workspace_folder_uris.is_empty()
            && let Some(uri) = params.get("rootUri").and_then(Value::as_str)
            && valid_file_uri(uri)
        {
            workspace_folder_uris.push(uri.to_string());
        }

        Self {
            enable_preview,
            dynamic_watch_registration,
            relative_pattern_support,
            workspace_folder_uris,
        }
    }
}

/// Server capabilities, kept deliberately minimal: definition, hover and full
/// document sync with save.
pub fn server_capabilities() -> Value {
    json!({
        "capabilities": {
            "textDocumentSync": {
                "openClose": true,
                "change": 1,
                "save": { "includeText": false }
            },
            "definitionProvider": true,
            "hoverProvider": true
        },
        "serverInfo": {
            "name": "gdfiles-lsp",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

/// Run the initialized server until `shutdown` + `exit` (or EOF).
pub fn run(connection: Connection) -> Result<(), DynError> {
    let (initialize_id, initialize_params) = connection.initialize_start()?;
    let config = InitConfig::from_initialize(&initialize_params);
    connection.initialize_finish(initialize_id, server_capabilities())?;

    let mut server = Server {
        connection,
        config,
        roots: HashMap::new(),
        overlays: HashMap::new(),
        previews: PreviewCache::new(),
        watchers_registered: false,
    };
    // `initialize_finish` already consumes the `initialized` notification, so
    // registration happens here rather than in the notification loop.
    server.register_watchers()?;
    server.message_loop()
}

struct Server {
    connection: Connection,
    config: InitConfig,
    roots: HashMap<PathBuf, ResourceIndex>,
    overlays: HashMap<PathBuf, String>,
    previews: PreviewCache,
    watchers_registered: bool,
}

impl Server {
    fn message_loop(&mut self) -> Result<(), DynError> {
        loop {
            let message = match self.connection.receiver.recv() {
                Ok(message) => message,
                Err(_) => return Ok(()),
            };
            match message {
                Message::Request(request) => {
                    if self.connection.handle_shutdown(&request)? {
                        return Ok(());
                    }
                    self.handle_request(request)?;
                }
                Message::Notification(notification) => {
                    if notification.method == "exit" {
                        return Ok(());
                    }
                    self.handle_notification(notification)?;
                }
                Message::Response(_) => {
                    // Responses to server-initiated watcher registrations need
                    // no follow-up state.
                }
            }
        }
    }

    fn handle_request(&mut self, request: Request) -> Result<(), DynError> {
        let Request { id, method, params } = request;
        match method.as_str() {
            "textDocument/definition" => match self.definition(&params) {
                Ok(result) => self.respond(id, result),
                Err(error) => self.respond_error(id, error),
            },
            "textDocument/hover" => match self.hover(&params) {
                Ok(result) => self.respond(id, result),
                Err(error) => self.respond_error(id, error),
            },
            _ => self.respond_error(
                id,
                RpcError::new(METHOD_NOT_FOUND, format!("unsupported method: {method}")),
            ),
        }
    }

    fn handle_notification(&mut self, notification: Notification) -> Result<(), DynError> {
        let Notification { method, params, .. } = notification;
        match method.as_str() {
            "initialized" => self.register_watchers()?,
            "textDocument/didOpen" => self.did_open(&params),
            "textDocument/didChange" => self.did_change(&params),
            "textDocument/didClose" => self.did_close(&params),
            "textDocument/didSave" => self.did_save(&params),
            "workspace/didChangeWatchedFiles" => self.did_change_watched_files(&params),
            // All other notifications are intentionally ignored.
            _ => {}
        }
        Ok(())
    }

    fn definition(&mut self, params: &Value) -> Result<Value, RpcError> {
        let path = document_path(params)?;
        let position = position(params)?;

        let Some(root) = self.ensure_root(&path) else {
            return Ok(Value::Null);
        };
        let Some(text) = self.document_text(&path) else {
            return Ok(Value::Null);
        };
        let Some(resolution) = self
            .roots
            .get(&root)
            .and_then(|index| index.resolve(&path, &text, position))
        else {
            return Ok(Value::Null);
        };

        let origin = range_json(resolution.range);
        let mut links = Vec::new();
        for target in &resolution.targets {
            if !target_is_text(&root, &target.path) {
                continue;
            }
            let Some(target_path) = safe_target_path(&root, &target.path) else {
                continue;
            };
            let Ok(uri) = Url::from_file_path(&target_path) else {
                continue;
            };
            let target_range = range_json(target.range);
            links.push(json!({
                "targetUri": uri.to_string(),
                "targetRange": target_range,
                "targetSelectionRange": target_range,
                "originSelectionRange": origin,
            }));
        }

        if links.is_empty() {
            Ok(Value::Null)
        } else {
            Ok(Value::Array(links))
        }
    }

    fn hover(&mut self, params: &Value) -> Result<Value, RpcError> {
        let path = document_path(params)?;
        let position = position(params)?;

        let Some(root) = self.ensure_root(&path) else {
            return Ok(Value::Null);
        };
        let Some(text) = self.document_text(&path) else {
            return Ok(Value::Null);
        };
        let Some(resolution) = self
            .roots
            .get(&root)
            .and_then(|index| index.resolve(&path, &text, position))
        else {
            return Ok(Value::Null);
        };

        let markdown = self.render_hover(&root, &resolution);
        Ok(json!({
            "contents": { "kind": "markdown", "value": markdown },
            "range": range_json(resolution.range),
        }))
    }

    fn render_hover(&mut self, root: &Path, resolution: &Resolution) -> String {
        let mut markdown = String::from("**Godot resource reference**\n");

        let token = truncate_utf8(&resolution.token, MAX_MARKDOWN_FIELD_BYTES);
        if !token.is_empty() {
            markdown.push_str(&format!("\n- Token: {}\n", code_span(token)));
        }

        let mut safe_targets = Vec::new();
        for target in &resolution.targets {
            if let Some(target) = safe_target(root, target) {
                safe_targets.push(target);
            }
        }

        if safe_targets.is_empty() {
            markdown.push_str("\nUnresolved resource reference.");
        } else {
            markdown.push('\n');
            for target in safe_targets.iter().take(MAX_TARGETS_IN_HOVER) {
                markdown.push_str("- ");
                let display = display_target_path(root, &target.path);
                let display = truncate_utf8(&display, MAX_MARKDOWN_FIELD_BYTES);
                markdown.push_str(&escape_markdown(display));
                if let Some(resource_type) = target.resource_type.as_deref() {
                    let resource_type = truncate_utf8(resource_type, MAX_MARKDOWN_FIELD_BYTES);
                    markdown.push_str(&format!(" ({})", escape_markdown(resource_type)));
                }
                if let Some(target_path) = safe_target_path(root, &target.path)
                    && let Ok(uri) = Url::from_file_path(&target_path)
                {
                    markdown.push_str(&format!(" [open](<{}>)", uri));
                }
                markdown.push('\n');
            }
            if resolution.targets.len() > MAX_TARGETS_IN_HOVER {
                markdown.push_str(&format!(
                    "- … {} more candidate(s)\n",
                    resolution.targets.len() - MAX_TARGETS_IN_HOVER
                ));
            }
        }

        if let Some(message) = resolution.message.as_deref() {
            let message = truncate_utf8(message, MAX_MARKDOWN_FIELD_BYTES * 2);
            markdown.push_str(&format!("\n{}\n", escape_markdown(message)));
        }

        markdown = truncate_utf8(&markdown, MAX_HOVER_TEXT_BYTES).to_string();

        if self.config.enable_preview
            && let Some(image_target) = safe_targets
                .iter()
                .find(|target| preview::is_supported_image(&target.path))
            && let Some(preview) = self.previews.get(&image_target.path, root)
        {
            let uri = preview.data_uri();
            if markdown.len() + uri.len() + 64 < MAX_HOVER_BYTES {
                markdown.push_str("\n\n![Preview](");
                markdown.push_str(&uri);
                markdown.push_str(")\n");
            }
        }

        truncate_utf8(&markdown, MAX_HOVER_BYTES - 1).to_string()
    }

    fn respond(&self, id: RequestId, result: Value) -> Result<(), DynError> {
        self.connection
            .sender
            .send(Message::Response(Response::new_ok(id, result)))?;
        Ok(())
    }

    fn respond_error(&self, id: RequestId, error: RpcError) -> Result<(), DynError> {
        self.connection
            .sender
            .send(Message::Response(Response::new_err(
                id,
                error.code,
                error.message,
            )))?;
        Ok(())
    }

    fn did_open(&mut self, params: &Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        let Some(text) = params.pointer("/textDocument/text").and_then(Value::as_str) else {
            return;
        };
        let Some(path) = uri_to_path(uri) else {
            return;
        };

        // Language id is intentionally not filtered: grammar-less Godot UID
        // documents (`(*.uid)`) use ordinary URI/document handling here.
        if text.len() as u64 > MAX_DOCUMENT_BYTES {
            self.discard_overlay(&path);
            return;
        }
        self.overlays.insert(path.clone(), text.to_string());
        if let Some(root) = self.ensure_root(&path)
            && let Some(index) = self.roots.get_mut(&root)
        {
            let _ = index.update(&path, Some(text));
        }
    }

    fn did_change(&mut self, params: &Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        // Full document sync: the first change must carry the complete text.
        let Some(text) = params
            .pointer("/contentChanges/0/text")
            .and_then(Value::as_str)
        else {
            return;
        };
        let Some(path) = uri_to_path(uri) else {
            return;
        };

        if text.len() as u64 > MAX_DOCUMENT_BYTES {
            // Do not keep serving the previous overlay as if it were this
            // version: fall back to the deterministic on-disk document.
            self.discard_overlay(&path);
            return;
        }
        self.overlays.insert(path.clone(), text.to_string());
        if let Some(root) = self.ensure_root(&path)
            && let Some(index) = self.roots.get_mut(&root)
        {
            let _ = index.update(&path, Some(text));
        }
    }

    fn did_close(&mut self, params: &Value) {
        let Some(path) = document_path_from_value(params) else {
            return;
        };
        self.overlays.remove(&path);
        if let Some(root) = self.ensure_root(&path)
            && let Some(index) = self.roots.get_mut(&root)
        {
            // None means "reread disk or remove metadata if missing".
            let _ = index.update(&path, None);
        }
    }

    fn did_save(&mut self, params: &Value) {
        let Some(path) = document_path_from_value(params) else {
            return;
        };
        let text = params.get("text").and_then(Value::as_str);
        if let Some(text) = text {
            if text.len() as u64 > MAX_DOCUMENT_BYTES {
                self.discard_overlay(&path);
                return;
            }
            self.overlays.insert(path.clone(), text.to_string());
        }
        if let Some(root) = self.ensure_root(&path)
            && let Some(index) = self.roots.get_mut(&root)
        {
            let _ = index.update(&path, text);
        }
    }

    fn did_change_watched_files(&mut self, params: &Value) {
        let Some(changes) = params.get("changes").and_then(Value::as_array) else {
            return;
        };
        for change in changes {
            let Some(uri) = change.get("uri").and_then(Value::as_str) else {
                continue;
            };
            let Some(path) = uri_to_path(uri) else {
                continue;
            };
            let Some(root) = self.ensure_root(&path) else {
                continue;
            };
            let is_uid_cache = is_project_uid_cache(&root, &path);
            if is_excluded(&root, &path) && !is_uid_cache {
                continue;
            }
            let overlay = self.overlays.get(&path).cloned();
            if let Some(index) = self.roots.get_mut(&root) {
                // Change/create rereads metadata; deletion removes it.
                let _ = index.update(&path, overlay.as_deref());
            }
        }
    }

    /// Lazily discover and initialize the nearest Godot root for a path.
    ///
    /// Workspace/rootUri folders are deliberately not assumed to be Godot
    /// roots: `project_root` finds the nearest `project.godot` ancestor.
    fn ensure_root(&mut self, path: &Path) -> Option<PathBuf> {
        let normalized = normalize_path(path)?;
        let root = project_root(&normalized)?;
        let root = root.canonicalize().ok()?;
        if !normalized.starts_with(&root) {
            return None;
        }

        if !self.roots.contains_key(&root) {
            let mut index = ResourceIndex::new(root.clone());
            // Bounded, one-time metadata scan per root; never on hover.
            let _ = index.refresh();
            self.roots.insert(root.clone(), index);
        }
        Some(root)
    }

    fn discard_overlay(&mut self, path: &Path) {
        self.overlays.remove(path);
        if let Some(root) = self.ensure_root(path)
            && let Some(index) = self.roots.get_mut(&root)
        {
            // Fall back to disk rather than serving the previous version's
            // overlay text as the current document.
            let _ = index.update(path, None);
        }
    }

    fn document_text(&self, path: &Path) -> Option<String> {
        if let Some(text) = self.overlays.get(path) {
            return Some(text.clone());
        }
        read_text_file(path)
    }

    fn register_watchers(&mut self) -> Result<(), DynError> {
        if self.watchers_registered || !self.config.dynamic_watch_registration {
            return Ok(());
        }
        self.watchers_registered = true;

        let mut watchers = Vec::new();
        if self.config.relative_pattern_support && !self.config.workspace_folder_uris.is_empty() {
            for base_uri in &self.config.workspace_folder_uris {
                for glob in WATCH_GLOBS {
                    watchers.push(json!({
                        "globPattern": { "baseUri": base_uri, "pattern": glob },
                        "kind": WATCH_KIND_ALL
                    }));
                }
            }
        } else {
            // Older clients only understand plain glob strings. These remain
            // narrow: never `**/*`.
            for glob in WATCH_GLOBS {
                watchers.push(json!({
                    "globPattern": glob,
                    "kind": WATCH_KIND_ALL
                }));
            }
        }

        let request = Request {
            id: RequestId::from("gdfiles-register-watchers".to_string()),
            method: "client/registerCapability".to_string(),
            params: json!({
                "registrations": [{
                    "id": "gdfiles-resource-watchers",
                    "method": "workspace/didChangeWatchedFiles",
                    "registerOptions": { "watchers": watchers }
                }]
            }),
        };
        self.connection.sender.send(Message::Request(request))?;
        Ok(())
    }
}

fn document_path(params: &Value) -> Result<PathBuf, RpcError> {
    let uri = params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "missing textDocument.uri"))?;
    uri_to_path(uri).ok_or_else(|| RpcError::new(INVALID_PARAMS, "invalid file URI"))
}

fn document_path_from_value(params: &Value) -> Option<PathBuf> {
    let uri = params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)?;
    uri_to_path(uri)
}

fn position(params: &Value) -> Result<Position, RpcError> {
    let position = params
        .get("position")
        .ok_or_else(|| RpcError::new(INVALID_PARAMS, "missing position"))?;
    let line = position
        .get("line")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            RpcError::new(INVALID_PARAMS, "position.line must be an unsigned integer")
        })?;
    let character = position
        .get("character")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            RpcError::new(
                INVALID_PARAMS,
                "position.character must be an unsigned integer",
            )
        })?;
    let line = u32::try_from(line)
        .map_err(|_| RpcError::new(INVALID_PARAMS, "position.line is too large"))?;
    let character = u32::try_from(character)
        .map_err(|_| RpcError::new(INVALID_PARAMS, "position.character is too large"))?;
    Ok(Position { line, character })
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let url = Url::parse(uri).ok()?;
    if url.scheme() != "file" {
        return None;
    }
    let path = url.to_file_path().ok()?;
    normalize_path(&path)
}

fn valid_file_uri(uri: &str) -> bool {
    Url::parse(uri)
        .map(|url| url.scheme() == "file" && url.to_file_path().is_ok())
        .unwrap_or(false)
}

fn normalize_path(path: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = path.canonicalize() {
        return Some(canonical);
    }
    let file_name = path.file_name()?;
    let parent = path.parent()?.canonicalize().ok()?;
    Some(parent.join(file_name))
}

fn read_text_file(path: &Path) -> Option<String> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_DOCUMENT_BYTES {
        return None;
    }
    let file = File::open(path).ok()?;
    let mut bytes = Vec::with_capacity(metadata.len().min(MAX_DOCUMENT_BYTES) as usize);
    file.take(MAX_DOCUMENT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_DOCUMENT_BYTES {
        return None;
    }
    String::from_utf8(bytes).ok()
}

fn safe_target_path(root: &Path, path: &Path) -> Option<PathBuf> {
    let root = root.canonicalize().ok()?;
    let canonical = path.canonicalize().ok()?;
    if !canonical.starts_with(&root) || !canonical.is_file() {
        return None;
    }
    Some(canonical)
}

fn safe_target(root: &Path, target: &Target) -> Option<Target> {
    safe_target_path(root, &target.path)?;
    Some(target.clone())
}

/// Render a validated target as a Godot project path for the hover body.
///
/// The absolute path stays in the clickable `[open]` link; only its visible
/// label is shortened. Falling back to the absolute path keeps the display
/// truthful if a target ever fails the project-relative conversion.
fn display_target_path(root: &Path, path: &Path) -> String {
    let relative = path
        .strip_prefix(root)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| {
            let canonical = path.canonicalize().ok()?;
            canonical.strip_prefix(root).ok().map(Path::to_path_buf)
        });

    match relative {
        Some(relative) => {
            let relative = relative.to_string_lossy().replace('\\', "/");
            format!("res://{relative}")
        }
        None => path.to_string_lossy().into_owned(),
    }
}

fn target_is_text(root: &Path, path: &Path) -> bool {
    let Some(canonical) = safe_target_path(root, path) else {
        return false;
    };
    if is_binary_extension(&canonical) {
        return false;
    }
    if is_text_extension(&canonical) {
        return true;
    }

    // Unknown extension: sniff a bounded prefix rather than trusting the name.
    let Ok(mut file) = File::open(&canonical) else {
        return false;
    };
    let mut bytes = [0u8; TEXT_SNIFF_BYTES];
    let Ok(read) = file.read(&mut bytes) else {
        return false;
    };
    let prefix = &bytes[..read];
    if prefix.contains(&0) {
        return false;
    }
    // Reject invalid UTF-8 with a definite error length. An incomplete
    // multi-byte sequence at the sniff boundary is tolerated.
    match std::str::from_utf8(prefix) {
        Ok(_) => true,
        Err(error) => error.error_len().is_none(),
    }
}

fn is_binary_extension(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    matches!(
        extension.as_deref(),
        Some(
            "png"
                | "jpg"
                | "jpeg"
                | "webp"
                | "gif"
                | "ico"
                | "bmp"
                | "tga"
                | "tif"
                | "tiff"
                | "dds"
                | "ktx"
                | "exr"
                | "hdr"
                | "ogg"
                | "wav"
                | "mp3"
                | "flac"
                | "m4a"
                | "pck"
                | "bin"
                | "res"
                | "scn"
                | "glb"
                | "fbx"
                | "ttf"
                | "otf"
                | "woff"
                | "woff2"
                | "so"
                | "dll"
                | "dylib"
                | "exe"
                | "zip"
                | "gz"
        )
    )
}

fn is_text_extension(path: &Path) -> bool {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase());
    matches!(
        extension.as_deref(),
        Some(
            "tres"
                | "tscn"
                | "gd"
                | "cs"
                | "gdshader"
                | "gdshaderinc"
                | "uid"
                | "godot"
                | "import"
                | "gdextension"
                | "json"
                | "cfg"
                | "txt"
                | "md"
                | "svg"
                | "gltf"
                | "tps"
                | "csv"
                | "xml"
                | "yaml"
                | "yml"
                | "shader"
        )
    )
}

fn is_project_uid_cache(root: &Path, path: &Path) -> bool {
    path == root.join(".godot/uid_cache.bin") || path == root.join("godot/uid_cache.bin")
}

fn is_excluded(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        matches!(
            name.as_ref(),
            ".git" | ".godot" | "bin" | "obj" | "node_modules"
        )
    })
}

fn range_json(range: Range) -> Value {
    json!({
        "start": { "line": range.start.line, "character": range.start.character },
        "end": { "line": range.end.line, "character": range.end.character }
    })
}

fn escape_markdown(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(
            character,
            '\\' | '`' | '*' | '_' | '[' | ']' | '<' | '>' | '#' | '|'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn code_span(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|character| {
            if character == '`' || character == '\n' || character == '\r' {
                '\''
            } else {
                character
            }
        })
        .collect();
    format!("`{cleaned}`")
}

fn truncate_utf8(value: &str, max_bytes: usize) -> &str {
    if value.len() <= max_bytes {
        return value;
    }
    let mut end = max_bytes;
    while end > 0 && !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

struct RpcError {
    code: i32,
    message: String,
}

impl RpcError {
    fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
