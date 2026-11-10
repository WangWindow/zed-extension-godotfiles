//! Real stdio JSON-RPC integration tests for `gdfiles-lsp`.
//!
//! These tests spawn the actual binary, speak Content-Length framed JSON-RPC and
//! use tempfile fixtures only. No Godot process, editor GUI or network is used.
//! Every wait is bounded; on timeout the child is killed before the test panics.

use std::{
    collections::BTreeMap,
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, RecvTimeoutError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};
use tempfile::TempDir;
use url::Url;

const A_UID: &str = "uid://c82j4r3k4n2";
const B_UID: &str = "uid://b7k2m9x4p1q8s";
const MISSING_UID: &str = "uid://m1s2s3n4g5";
const NEW_UID: &str = "uid://n3w9d7x2k5";
const CACHE_UID: &str = "uid://c4ch3f4b4ck";
const HIDDEN_UID: &str = "uid://h1dd3n5c4n";

const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_PREVIEW_BYTES: usize = 48 * 1024;
const MAX_PREVIEW_WIDTH: u32 = 192;
const MAX_PREVIEW_HEIGHT: u32 = 128;
const MAX_HOVER_BYTES: usize = 100_000;

#[test]
fn native_stdio_lifecycle_navigation_preview_overlay_and_roots() {
    let fixture = Fixture::new();
    let before = snapshot(fixture.root());
    let (mut client, registration) = start_client(fixture.root());

    assert_watcher_registration(&registration, fixture.root());

    let main = fixture.root().join("scenes/main.tscn");
    let main_text = fs::read_to_string(&main).unwrap();
    client.did_open(&main, &main_text, "godot-resource");

    // UID definitions. The duplicate UID deliberately maps to both a text
    // resource and a binary image in this fixture; only the text target may be
    // returned.
    let response = client.definition(&main, position_at(&main_text, A_UID, 1));
    assert_definition_targets(&response, &["resources/a.tres"]);
    assert!(
        target_uris(&response)
            .iter()
            .all(|uri| !uri.contains("dup.png")),
        "binary duplicate leaked into definition: {response}"
    );

    let response = client.definition(&main, position_at(&main_text, B_UID, 1));
    assert_definition_targets(&response, &["resources/b.tres"]);

    // Binary-only target: no definition at all (Zed cannot open binary buffers).
    let icon_reference = position_at(&main_text, "res://assets/icon.png", 1);
    let response = client.definition(&main, icon_reference.clone());
    assert!(
        response["result"].is_null(),
        "binary definition: {response}"
    );

    // Unknown extensions with invalid UTF-8, and explicit binary Godot formats
    // that happen to look textual, are never text definition targets.
    for needle in ["res://assets/blob.custom", "res://assets/looks_text.res"] {
        let position = position_at(&main_text, needle, 1);
        let hover = client.hover(&main, position.clone());
        let markdown = hover_markdown(&hover).expect("binary metadata hover");
        assert!(
            markdown.contains(needle.rsplit('/').next().unwrap()),
            "hover metadata for {needle}: {markdown}"
        );
        let response = client.definition(&main, position);
        assert!(
            response["result"].is_null(),
            "non-text definition for {needle}: {response}"
        );
    }

    // Out-of-range positions are safe, malformed positions are invalid params,
    // and neither kills the process.
    let response = client.definition(&main, json!({ "line": 99_999u32, "character": 99_999u32 }));
    assert!(response["result"].is_null(), "out of range: {response}");

    let response = client.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": file_uri(&main) },
            "position": { "line": "not-a-number", "character": 0 }
        }),
    );
    assert_eq!(
        response["error"]["code"], -32602,
        "malformed position: {response}"
    );

    let response = client.request(
        "textDocument/definition",
        json!({
            "textDocument": { "uri": "res://not-a-file-uri" },
            "position": { "line": 0, "character": 0 }
        }),
    );
    assert_eq!(
        response["error"]["code"], -32602,
        "malformed URI: {response}"
    );

    // The process still answers after invalid requests.
    let response = client.definition(&main, position_at(&main_text, B_UID, 1));
    assert_definition_targets(&response, &["resources/b.tres"]);

    // Hover metadata for a text target.
    let response = client.hover(&main, position_at(&main_text, A_UID, 1));
    let markdown = hover_markdown(&response).expect("text hover");
    assert!(markdown.contains(A_UID), "hover token: {markdown}");
    let target_line = markdown
        .lines()
        .find(|line| line.starts_with("- res://resources/a.tres"))
        .unwrap_or_else(|| panic!("hover target should be project-relative: {markdown}"));
    assert!(
        target_line.contains("[open](<file://"),
        "hover target should keep the open link: {target_line}"
    );
    assert!(markdown.len() < MAX_HOVER_BYTES);

    // Hover for a missing resource: still a hover, with an explanation.
    let response = client.hover(
        &main,
        position_at(&main_text, "res://resources/missing.tres", 1),
    );
    let markdown = hover_markdown(&response).expect("unresolved hover");
    assert!(
        markdown.contains("missing") || markdown.contains(MISSING_UID),
        "unresolved hover: {markdown}"
    );
    assert!(response["result"]["contents"]["kind"] == "markdown");
    let missing_definition = client.definition(
        &main,
        position_at(&main_text, "res://resources/missing.tres", 1),
    );
    assert!(
        missing_definition["result"].is_null(),
        "missing definition: {missing_definition}"
    );

    // On-demand image hover: plain PNG data URI, 192x128/48KiB bounds.
    let response = client.hover(&main, icon_reference);
    let markdown = hover_markdown(&response).expect("image hover");
    assert!(
        markdown.contains("data:image/png;base64,"),
        "image hover: {markdown}"
    );
    assert!(
        markdown.contains("- res://assets/icon.png [open](<file://"),
        "image hover should show a project-relative path but keep the open link: {markdown}"
    );
    assert!(
        !markdown.lines().any(|line| line.starts_with("- /")),
        "image hover must not show an absolute target path: {markdown}"
    );
    assert!(markdown.len() < MAX_HOVER_BYTES);
    let encoded = extract_data_uri_payload(markdown);
    let png = BASE64.decode(encoded).expect("base64 preview");
    assert!(
        png.len() <= MAX_PREVIEW_BYTES,
        "preview bytes: {}",
        png.len()
    );
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "preview must be PNG");
    let image = image::load_from_memory(&png).expect("decode preview");
    assert!(
        image.width() <= MAX_PREVIEW_WIDTH && image.height() <= MAX_PREVIEW_HEIGHT,
        "preview exceeds the hover box: {}x{}",
        image.width(),
        image.height()
    );
    let aspect = image.width() as f32 / image.height() as f32;
    assert!(
        (aspect - 1.5).abs() < 0.02,
        "300x200 icon preview aspect: {}x{}",
        image.width(),
        image.height()
    );

    // Input dimension and byte caps are enforced before decoding/reading; both
    // hovers still return metadata but never a preview.
    for (needle, file_name, label) in [
        ("res://assets/wide.png", "wide.png", "dimension limit"),
        ("res://assets/huge.png", "huge.png", "input byte limit"),
    ] {
        let response = client.hover(&main, position_at(&main_text, needle, 1));
        let markdown = hover_markdown(&response).expect("bounded image hover");
        assert!(markdown.contains(file_name), "{label} hover: {markdown}");
        assert!(
            !markdown.contains("data:image/"),
            "{label} produced a preview: {markdown}"
        );
    }

    // A symlink inside the project that points outside must not be read or
    // previewed.
    #[cfg(unix)]
    {
        let escape = position_at(&main_text, "res://assets/escape.png", 1);
        let response = client.hover(&main, escape.clone());
        if let Some(markdown) = hover_markdown(&response) {
            assert!(
                !markdown.contains("data:image/"),
                "symlink escape preview leaked: {markdown}"
            );
        }
        let response = client.definition(&main, escape);
        assert!(response["result"].is_null(), "symlink escape: {response}");
    }

    client.did_save(&main, &main_text);

    // Unsaved full-document overlay: UID wins over a stale literal path.
    let overlay = format!(
        "[gd_scene load_steps=1 format=3]\n\n\
         [ext_resource type=\"Resource\" uid=\"{B_UID}\" path=\"res://resources/a.tres\" id=\"1_a\"]\n\n\
         [node name=\"Main\" type=\"Node\"]\n"
    );
    client.did_change(&main, &overlay, 2);
    let response = client.definition(&main, position_at(&overlay, B_UID, 1));
    assert_definition_targets(&response, &["resources/b.tres"]);

    // Closing restores the disk text, so the stale-path UID resolves to A again.
    client.did_close(&main);
    let response = client.definition(&main, position_at(&main_text, A_UID, 1));
    assert_definition_targets(&response, &["resources/a.tres"]);

    // Grammar-less Godot UID documents: ordinary URI handling, plain UID resolves
    // to its owner.
    let uid_document = fixture.root().join("resources/a.tres.uid");
    let uid_text = fs::read_to_string(&uid_document).unwrap();
    client.did_open(&uid_document, &uid_text, "godot-uid");
    let response = client.definition(&uid_document, position_at(&uid_text, A_UID, 1));
    assert_definition_targets(&response, &["resources/a.tres"]);
    client.did_close(&uid_document);

    // Two independent nearest-Godot roots: the same UID in the nested project
    // must resolve inside that project, not into the outer one.
    let nested_scene = fixture.root().join("nested/scene.tscn");
    let nested_text = fs::read_to_string(&nested_scene).unwrap();
    let response = client.definition(&nested_scene, position_at(&nested_text, A_UID, 1));
    assert_definition_targets(&response, &["nested/resources/c.tres"]);

    let response = client.definition(&main, position_at(&main_text, A_UID, 1));
    assert_definition_targets(&response, &["resources/a.tres"]);

    client.shutdown();

    assert_eq!(
        before,
        snapshot(fixture.root()),
        "the resource server must not mutate the project"
    );
}

#[test]
fn cli_version_flag_exits_without_starting_the_transport() {
    let output = Command::new(env!("CARGO_BIN_EXE_gdfiles-lsp"))
        .arg("--version")
        .output()
        .expect("run --version");
    assert!(output.status.success(), "status: {}", output.status);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("gdfiles-lsp"), "stdout: {stdout}");
    assert!(!stdout.contains("Content-Length"), "stdout: {stdout}");
}

#[test]
fn preview_can_be_disabled_with_initialization_options() {
    let fixture = Fixture::new();
    let (mut client, registration) = start_client_with(fixture.root(), Some(false));
    assert_watcher_registration(&registration, fixture.root());

    let main = fixture.root().join("scenes/main.tscn");
    let main_text = fs::read_to_string(&main).unwrap();
    client.did_open(&main, &main_text, "godot-resource");

    let response = client.hover(&main, position_at(&main_text, "res://assets/icon.png", 1));
    let markdown = hover_markdown(&response).expect("image hover with preview disabled");
    assert!(markdown.contains("icon.png"), "hover metadata: {markdown}");
    assert!(
        !markdown.contains("data:image/"),
        "preview should be disabled: {markdown}"
    );

    client.shutdown();
}

#[test]
fn uid_cache_watch_reloads_only_the_project_cache_path() {
    let fixture = Fixture::new();
    let root = fixture.root();

    let hidden = root.join(".godot/hidden.tres");
    write(
        &hidden,
        &format!(
            "[gd_resource type=\"Resource\" load_steps=2 format=3 uid=\"{HIDDEN_UID}\"]\n\n[resource]\n"
        ),
    );
    let cache = root.join(".godot/uid_cache.bin");
    write_uid_cache(&cache, &[(CACHE_UID, "res://resources/a.tres")]);

    let query = root.join("scenes/cache_query.tscn");
    let query_text = format!(
        "[gd_scene load_steps=3 format=3]\n\n\
         [ext_resource type=\"Resource\" uid=\"{CACHE_UID}\" path=\"res://resources/missing.tres\" id=\"1_cache\"]\n\
         [ext_resource type=\"Resource\" uid=\"{HIDDEN_UID}\" path=\"res://resources/missing.tres\" id=\"2_hidden\"]\n\n\
         [node name=\"CacheQuery\" type=\"Node\"]\n"
    );
    write(&query, &query_text);

    let (mut client, registration) = start_client(root);
    assert_watcher_registration(&registration, root);

    let hidden_position = position_at(&query_text, HIDDEN_UID, 1);
    let response = client.definition(&query, hidden_position.clone());
    assert!(
        response["result"].is_null(),
        ".godot resources are not indexed by refresh: {response}"
    );

    // Changing an unrelated file under .godot must stay excluded even after a
    // watched-file notification.
    client.did_change_watched_files(&[(hidden.as_path(), 2)]);
    let response = client.definition(&query, hidden_position);
    assert!(
        response["result"].is_null(),
        "non-cache .godot file leaked into the index: {response}"
    );

    let cache_position = position_at(&query_text, CACHE_UID, 1);
    let response = client.definition(&query, cache_position.clone());
    assert_definition_targets(&response, &["resources/a.tres"]);

    // The cache itself is allowed through the .godot exclusion so its fallback
    // owner mapping can be reloaded incrementally.
    write_uid_cache(&cache, &[(CACHE_UID, "res://resources/b.tres")]);
    client.did_change_watched_files(&[(cache.as_path(), 2)]);
    let response = client.definition(&query, cache_position);
    assert_definition_targets(&response, &["resources/b.tres"]);

    client.shutdown();
}

#[test]
fn oversized_overlays_are_rejected_without_reusing_stale_text() {
    let fixture = Fixture::new();
    let (mut client, registration) = start_client(fixture.root());
    assert_watcher_registration(&registration, fixture.root());

    let main = fixture.root().join("scenes/main.tscn");
    let disk_text = fs::read_to_string(&main).unwrap();
    let overlay = format!(
        "[gd_scene load_steps=1 format=3]\n\n\
         [ext_resource type=\"Resource\" uid=\"{B_UID}\" path=\"res://resources/a.tres\" id=\"1_a\"]\n\n\
         [node name=\"Main\" type=\"Node\"]\n"
    );

    client.did_open(&main, &overlay, "godot-resource");
    let b_position = position_at(&overlay, B_UID, 1);
    let response = client.definition(&main, b_position.clone());
    assert_definition_targets(&response, &["resources/b.tres"]);

    // Oversized didChange: the old unsaved overlay must not remain active.
    let mut oversized = overlay.clone();
    oversized.push_str(&"x".repeat(4 * 1024 * 1024 + 1));
    client.did_change(&main, &oversized, 2);
    let response = client.definition(&main, b_position);
    assert_definition_targets(&response, &["resources/a.tres"]);

    // Oversized didOpen is rejected before storing/indexing, so the on-disk
    // document remains authoritative.
    client.did_open(&main, &oversized, "godot-resource");
    let response = client.definition(&main, position_at(&disk_text, A_UID, 1));
    assert_definition_targets(&response, &["resources/a.tres"]);

    client.shutdown();
}

#[test]
fn watched_sidecar_updates_and_deletion_are_incremental() {
    let fixture = Fixture::new();
    let before = snapshot(fixture.root());

    let (mut client, _registration) = start_client(fixture.root());

    let main = fixture.root().join("scenes/main.tscn");
    let overlay = format!(
        "[gd_scene load_steps=1 format=3]\n\n\
         [ext_resource type=\"Resource\" uid=\"{NEW_UID}\" path=\"res://resources/a.tres\" id=\"1_a\"]\n\n\
         [node name=\"Main\" type=\"Node\"]\n"
    );
    client.did_open(&main, &overlay, "godot-resource");
    let new_reference = position_at(&overlay, NEW_UID, 1);

    // Before the sidecar is updated the UID is unknown, so the valid literal
    // path wins.
    let response = client.definition(&main, new_reference.clone());
    assert_definition_targets(&response, &["resources/a.tres"]);

    // Point B's owner sidecar at the new UID and notify through an actual
    // watched-files notification. No full rescan or editor restart is involved.
    let b_sidecar = fixture.root().join("resources/b.tres.uid");
    fs::write(&b_sidecar, format!("{NEW_UID}\n")).unwrap();
    client.did_change_watched_files(&[(b_sidecar.as_path(), 2)]);
    let response = client.definition(&main, new_reference.clone());
    assert_definition_targets(&response, &["resources/b.tres"]);

    // Deleting the sidecar removes its metadata and falls back to the path.
    fs::remove_file(&b_sidecar).unwrap();
    client.did_change_watched_files(&[(b_sidecar.as_path(), 3)]);
    let response = client.definition(&main, new_reference.clone());
    assert_definition_targets(&response, &["resources/a.tres"]);

    // A valid request still works after the delete notification.
    let response = client.hover(&main, new_reference);
    let markdown = hover_markdown(&response).expect("hover after deletion");
    assert!(markdown.contains("a.tres"), "fallback hover: {markdown}");

    client.shutdown();

    // Only the intentionally removed sidecar differs; the server must not have
    // created or modified anything else (not even a .godot cache).
    let after = snapshot(fixture.root());
    for (path, bytes) in &before {
        if path == Path::new("resources/b.tres.uid") {
            continue;
        }
        assert_eq!(
            after.get(path),
            Some(bytes),
            "server changed {}",
            path.display()
        );
    }
    for path in after.keys() {
        assert!(
            before.contains_key(path),
            "server created unexpected file {}",
            path.display()
        );
    }
    assert!(!after.contains_key(Path::new("resources/b.tres.uid")));
}

// ---------------------------------------------------------------------------
// LSP client
// ---------------------------------------------------------------------------

struct LspClient {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: Receiver<Value>,
    stderr: Arc<Mutex<String>>,
    next_id: i64,
    reader_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
}

impl LspClient {
    fn spawn() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_gdfiles-lsp"))
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn gdfiles-lsp");

        let stdin = child.stdin.take().expect("child stdin");
        let stdout = child.stdout.take().expect("child stdout");
        let stderr_pipe = child.stderr.take().expect("child stderr");

        let (sender, messages) = mpsc::channel();
        let reader_thread = thread::spawn(move || read_messages(stdout, sender));

        let stderr = Arc::new(Mutex::new(String::new()));
        let stderr_for_thread = Arc::clone(&stderr);
        let stderr_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stderr_pipe);
            let mut buffer = String::new();
            let _ = reader.read_to_string(&mut buffer);
            if let Ok(mut target) = stderr_for_thread.lock() {
                target.push_str(&buffer);
            }
        });

        Self {
            child,
            stdin: Some(stdin),
            messages,
            stderr,
            next_id: 1,
            reader_thread: Some(reader_thread),
            stderr_thread: Some(stderr_thread),
        }
    }

    fn send(&mut self, message: Value) {
        let body = serde_json::to_vec(&message).expect("serialize JSON-RPC");
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let stdin = self.stdin.as_mut().expect("stdin still open");
        stdin.write_all(header.as_bytes()).expect("write header");
        stdin.write_all(&body).expect("write body");
        stdin.flush().expect("flush request");
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }));
        self.wait_for_response(id)
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }));
    }

    fn respond(&mut self, id: Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn wait_for_response(&mut self, id: i64) -> Value {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let message = self.recv_until(deadline, &format!("response {id}"));
            if message.get("method").is_some() {
                if let Some(request_id) = message.get("id").cloned() {
                    // Client-side request (watcher registration etc.).
                    self.respond(request_id, Value::Null);
                }
                continue;
            }
            if message.get("id").and_then(Value::as_i64) == Some(id) {
                return message;
            }
        }
    }

    fn expect_server_request(&mut self, method: &str) -> Value {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let message = self.recv_until(deadline, method);
            if message.get("method").and_then(Value::as_str) == Some(method) {
                if let Some(id) = message.get("id").cloned() {
                    self.respond(id, Value::Null);
                    return message;
                }
            } else if message.get("method").is_some()
                && let Some(id) = message.get("id").cloned()
            {
                self.respond(id, Value::Null);
            }
        }
    }

    fn recv_until(&mut self, deadline: Instant, what: &str) -> Value {
        let now = Instant::now();
        if now >= deadline {
            self.fail(&format!("timed out waiting for {what}"));
        }
        match self.messages.recv_timeout(deadline - now) {
            Ok(message) => message,
            Err(RecvTimeoutError::Timeout) => self.fail(&format!("timed out waiting for {what}")),
            Err(RecvTimeoutError::Disconnected) => {
                self.fail(&format!("server exited while waiting for {what}"))
            }
        }
    }

    fn fail(&mut self, message: &str) -> ! {
        let stderr = self
            .stderr
            .lock()
            .map(|stderr| stderr.clone())
            .unwrap_or_default();
        let _ = self.child.kill();
        let _ = self.child.wait();
        panic!("{message}\nserver stderr:\n{stderr}");
    }

    fn initialize(&mut self, root: &Path, enable_preview: Option<bool>) -> Value {
        let root_uri = file_uri(root);
        let mut params = json!({
            "processId": null,
            "clientInfo": { "name": "gdfiles-protocol-test", "version": "1" },
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": "fixture" }],
            "capabilities": {
                "textDocument": {
                    "definition": { "linkSupport": true },
                    "hover": { "contentFormat": ["markdown"] }
                },
                "workspace": {
                    "didChangeWatchedFiles": {
                        "dynamicRegistration": true,
                        "relativePatternSupport": true
                    }
                }
            }
        });
        if let Some(enable_preview) = enable_preview {
            params["initializationOptions"] = json!({ "enablePreview": enable_preview });
        }
        self.request("initialize", params)
    }

    fn did_open(&mut self, path: &Path, text: &str, language_id: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": file_uri(path),
                    "languageId": language_id,
                    "version": 1,
                    "text": text
                }
            }),
        );
    }

    fn did_change(&mut self, path: &Path, text: &str, version: i64) {
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": file_uri(path), "version": version },
                "contentChanges": [{ "text": text }]
            }),
        );
    }

    fn did_save(&mut self, path: &Path, text: &str) {
        self.notify(
            "textDocument/didSave",
            json!({ "textDocument": { "uri": file_uri(path) }, "text": text }),
        );
    }

    fn did_close(&mut self, path: &Path) {
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": file_uri(path) } }),
        );
    }

    fn did_change_watched_files(&mut self, changes: &[(&Path, u32)]) {
        let changes: Vec<Value> = changes
            .iter()
            .map(|(path, kind)| json!({ "uri": file_uri(path), "type": kind }))
            .collect();
        self.notify(
            "workspace/didChangeWatchedFiles",
            json!({ "changes": changes }),
        );
    }

    fn definition(&mut self, path: &Path, position: Value) -> Value {
        self.request(
            "textDocument/definition",
            json!({
                "textDocument": { "uri": file_uri(path) },
                "position": position
            }),
        )
    }

    fn hover(&mut self, path: &Path, position: Value) -> Value {
        self.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": file_uri(path) },
                "position": position
            }),
        )
    }

    fn shutdown(&mut self) {
        let response = self.request("shutdown", Value::Null);
        assert!(
            response.get("error").is_none(),
            "shutdown response: {response}"
        );
        assert_eq!(response["result"], Value::Null);

        self.notify("exit", Value::Null);
        // The server joins its stdio threads after `exit`, so close our end too.
        drop(self.stdin.take());

        let deadline = Instant::now() + PROCESS_EXIT_TIMEOUT;
        let status = loop {
            match self.child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        panic!("server did not exit after shutdown/exit");
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                Err(error) => panic!("waiting for server shutdown: {error}"),
            }
        };
        assert!(status.success(), "server exit status: {status}");

        if let Some(thread) = self.reader_thread.take() {
            let _ = thread.join();
        }
        if let Some(thread) = self.stderr_thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        // Ensure no test can leave a child process behind, including on panic.
        let _ = self.stdin.take();
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn start_client(root: &Path) -> (LspClient, Value) {
    start_client_with(root, None)
}

fn start_client_with(root: &Path, enable_preview: Option<bool>) -> (LspClient, Value) {
    let mut client = LspClient::spawn();
    let initialize = client.initialize(root, enable_preview);
    assert!(
        initialize.get("error").is_none(),
        "initialize failed: {initialize}"
    );
    let capabilities = &initialize["result"]["capabilities"];
    assert_eq!(capabilities["definitionProvider"], true);
    assert_eq!(capabilities["hoverProvider"], true);
    assert_eq!(capabilities["textDocumentSync"]["openClose"], true);
    assert_eq!(capabilities["textDocumentSync"]["change"], 1);
    assert!(capabilities["textDocumentSync"]["save"].is_object());

    client.notify("initialized", json!({}));
    let registration = client.expect_server_request("client/registerCapability");
    (client, registration)
}

fn assert_watcher_registration(registration: &Value, root: &Path) {
    let registrations = registration["params"]["registrations"]
        .as_array()
        .expect("registrations array");
    assert_eq!(registrations.len(), 1);
    let registration = &registrations[0];
    assert_eq!(registration["method"], "workspace/didChangeWatchedFiles");

    let watchers = registration["registerOptions"]["watchers"]
        .as_array()
        .expect("watchers array");
    let mut patterns = Vec::new();
    for watcher in watchers {
        let glob = &watcher["globPattern"];
        if let Some(pattern) = glob.as_str() {
            patterns.push(pattern.to_string());
        } else {
            let pattern = glob["pattern"].as_str().expect("relative glob pattern");
            patterns.push(pattern.to_string());
            let base = glob["baseUri"].as_str().expect("relative base URI");
            assert_eq!(
                Url::parse(base).unwrap(),
                Url::from_file_path(root).unwrap()
            );
        }
        assert_eq!(watcher["kind"], 7);
    }

    for expected in [
        "**/*.tscn",
        "**/*.tres",
        "**/*.gdshader",
        "**/*.gdshaderinc",
        "**/*.uid",
        "**/*.import",
        "**/*.godot",
        "**/uid_cache.bin",
    ] {
        assert!(
            patterns.iter().any(|pattern| pattern == expected),
            "missing watcher {expected}: {patterns:?}"
        );
    }
    assert!(
        patterns.iter().all(|pattern| pattern != "**/*"),
        "watchers must stay narrow: {patterns:?}"
    );
}

// ---------------------------------------------------------------------------
// Assertions and wire helpers
// ---------------------------------------------------------------------------

fn assert_definition_targets(response: &Value, expected_suffixes: &[&str]) {
    let uris = target_uris(response);
    assert_eq!(
        uris.len(),
        expected_suffixes.len(),
        "expected exactly {expected_suffixes:?}, got {uris:?} ({response})"
    );
    let mut unmatched = uris.clone();
    for suffix in expected_suffixes {
        let Some(index) = unmatched.iter().position(|uri| uri.ends_with(suffix)) else {
            panic!("expected target {suffix}, got {uris:?}");
        };
        unmatched.remove(index);
    }
    assert!(
        unmatched.is_empty(),
        "unexpected extra definitions: {unmatched:?}"
    );
    for uri in &uris {
        assert!(
            uri.starts_with("file://"),
            "definition target must be a file URI: {uri}"
        );
    }
    assert!(
        !response["result"][0]["targetSelectionRange"]["start"]["line"].is_null(),
        "definition link needs a target selection range: {response}"
    );
}

fn target_uris(response: &Value) -> Vec<String> {
    response["result"]
        .as_array()
        .map(|targets| {
            targets
                .iter()
                .filter_map(|target| target["targetUri"].as_str().map(|uri| uri.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn hover_markdown(response: &Value) -> Option<&str> {
    response
        .pointer("/result/contents/value")
        .and_then(Value::as_str)
}

fn extract_data_uri_payload(markdown: &str) -> &str {
    const PREFIX: &str = "data:image/png;base64,";
    let start = markdown.find(PREFIX).expect("data URI prefix") + PREFIX.len();
    let rest = &markdown[start..];
    let end = rest
        .find(|character: char| {
            character == '>' || character == ')' || character == '"' || character.is_whitespace()
        })
        .unwrap_or(rest.len());
    &rest[..end]
}

fn position_at(text: &str, needle: &str, offset: usize) -> Value {
    let byte_index = text
        .find(needle)
        .unwrap_or_else(|| panic!("needle not found in fixture: {needle:?}"))
        + offset;
    assert!(byte_index <= text.len(), "offset outside fixture");
    let before = &text[..byte_index];
    let line = before.bytes().filter(|byte| *byte == b'\n').count() as u32;
    let line_start = before.rfind('\n').map(|index| index + 1).unwrap_or(0);
    let character = text[line_start..byte_index].encode_utf16().count() as u32;
    json!({ "line": line, "character": character })
}

fn file_uri(path: &Path) -> String {
    Url::from_file_path(path)
        .expect("absolute fixture path")
        .to_string()
}

fn read_messages(stdout: ChildStdout, sender: mpsc::Sender<Value>) {
    let mut reader = BufReader::new(stdout);
    while let Ok(Some(message)) = read_message(&mut reader) {
        if sender.send(message).is_err() {
            break;
        }
    }
}

fn read_message(reader: &mut BufReader<ChildStdout>) -> std::io::Result<Option<Value>> {
    let mut content_length = None;
    loop {
        let mut line = String::new();
        let read = reader.read_line(&mut line)?;
        if read == 0 {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value.trim().parse::<usize>().ok();
        }
    }

    let length = content_length.ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing Content-Length")
    })?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    let value = serde_json::from_slice(&body).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid JSON-RPC body: {error}"),
        )
    })?;
    Ok(Some(value))
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    temp: TempDir,
    _outside: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("project tempdir");
        let outside = tempfile::tempdir().expect("outside tempdir");
        let root = temp.path();

        write(
            &root.join("project.godot"),
            "; Engine configuration file.\n[application]\nconfig/name=\"Resource LSP Fixture\"\n",
        );
        write(
            &root.join("resources/a.tres"),
            "[gd_resource type=\"Resource\" script_class=\"Resource\" load_steps=2 format=3]\n\n[resource]\nmetadata/name = \"A\"\n",
        );
        write(&root.join("resources/a.tres.uid"), &format!("{A_UID}\n"));
        write(
            &root.join("resources/b.tres"),
            "[gd_resource type=\"Resource\" script_class=\"Resource\" load_steps=2 format=3]\n\n[resource]\nmetadata/name = \"B\"\n",
        );
        write(&root.join("resources/b.tres.uid"), &format!("{B_UID}\n"));

        write_noise_png(&root.join("assets/icon.png"), 300, 200);
        write_noise_png(&root.join("assets/dup.png"), 300, 200);
        // Input dimension guard: wider than the decoder's maximum.
        write_noise_png(&root.join("assets/wide.png"), 9_000, 1);
        // Input byte guard: larger than the 8 MiB read cap. Contents are never
        // decoded because metadata rejects it first.
        fs::write(root.join("assets/huge.png"), vec![0u8; 9 * 1024 * 1024])
            .expect("oversized image fixture");
        // Invalid UTF-8 with no NUL: the unknown-extension sniff must reject it.
        fs::write(
            root.join("assets/blob.custom"),
            [0xffu8, 0xfe, 0xfd, 0xfc, 0x80, 0x81],
        )
        .expect("invalid UTF-8 fixture");
        // `.res` is an explicit binary Godot format even when its bytes happen
        // to look like UTF-8 text.
        write(
            &root.join("assets/looks_text.res"),
            "this is text-shaped but .res is a binary Godot resource\n",
        );
        write(
            &root.join("assets/dup.png.import"),
            &format!(
                "[remap]\n\nuid=\"{A_UID}\"\npath=\"res://.godot/imported/dup.png-abc.ctex\"\nsource_file=\"res://assets/dup.png\"\n"
            ),
        );

        #[cfg(unix)]
        {
            let outside_png = outside.path().join("outside.png");
            write_noise_png(&outside_png, 32, 32);
            std::os::unix::fs::symlink(&outside_png, root.join("assets/escape.png"))
                .expect("symlink escape fixture");
        }

        write(&root.join("scenes/main.tscn"), &main_scene_text());

        // A second, nested Godot project deliberately reuses A_UID so the two
        // roots must not share a UID namespace.
        write(
            &root.join("nested/project.godot"),
            "; nested project\n[application]\nconfig/name=\"Nested\"\n",
        );
        write(
            &root.join("nested/resources/c.tres"),
            "[gd_resource type=\"Resource\" load_steps=2 format=3]\n\n[resource]\nmetadata/name = \"C\"\n",
        );
        write(
            &root.join("nested/resources/c.tres.uid"),
            &format!("{A_UID}\n"),
        );
        write(
            &root.join("nested/scene.tscn"),
            &format!(
                "[gd_scene load_steps=2 format=3]\n\n[ext_resource type=\"Resource\" uid=\"{A_UID}\" path=\"res://resources/c.tres\" id=\"1_c\"]\n\n[node name=\"Nested\" type=\"Node\"]\n"
            ),
        );

        Self {
            temp,
            _outside: outside,
        }
    }

    fn root(&self) -> &Path {
        self.temp.path()
    }
}

fn main_scene_text() -> String {
    format!(
        "[gd_scene load_steps=9 format=3]\n\n\
         [ext_resource type=\"Resource\" uid=\"{A_UID}\" path=\"res://resources/a.tres\" id=\"1_a\"]\n\
         [ext_resource type=\"Resource\" uid=\"{B_UID}\" path=\"res://resources/b.tres\" id=\"2_b\"]\n\
         [ext_resource type=\"Texture2D\" path=\"res://assets/icon.png\" id=\"3_icon\"]\n\
         [ext_resource type=\"Resource\" uid=\"{MISSING_UID}\" path=\"res://resources/missing.tres\" id=\"4_missing\"]\n\
         [ext_resource type=\"Texture2D\" path=\"res://assets/escape.png\" id=\"5_escape\"]\n\
         [ext_resource type=\"Texture2D\" path=\"res://assets/wide.png\" id=\"6_wide\"]\n\
         [ext_resource type=\"Texture2D\" path=\"res://assets/huge.png\" id=\"7_huge\"]\n\
         [ext_resource type=\"Resource\" path=\"res://assets/blob.custom\" id=\"8_custom\"]\n\
         [ext_resource type=\"Resource\" path=\"res://assets/looks_text.res\" id=\"9_res\"]\n\n\
         [node name=\"Main\" type=\"Node\"]\n\
         a = ExtResource(\"1_a\")\n\
         b = ExtResource(\"2_b\")\n\
         icon = ExtResource(\"3_icon\")\n\
         missing = ExtResource(\"4_missing\")\n\
         escape = ExtResource(\"5_escape\")\n"
    )
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture directory");
    }
    fs::write(path, contents).expect("write fixture file");
}

fn write_uid_cache(path: &Path, entries: &[(&str, &str)]) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (uid, resource_path) in entries {
        let id = gdfiles_lsp::uid_to_id(uid).expect("valid fixture UID");
        bytes.extend_from_slice(&id.to_le_bytes());
        let path_bytes = resource_path.as_bytes();
        bytes.extend_from_slice(&(path_bytes.len() as u32).to_le_bytes());
        bytes.extend_from_slice(path_bytes);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create uid_cache directory");
    }
    fs::write(path, bytes).expect("write uid_cache fixture");
}

fn write_noise_png(path: &Path, width: u32, height: u32) {
    let mut image = image::RgbaImage::new(width, height);
    let mut state = 0x1234_5678u32;
    for pixel in image.pixels_mut() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let red = (state >> 24) as u8;
        let green = (state >> 16) as u8;
        let blue = (state >> 8) as u8;
        *pixel = image::Rgba([red, green, blue, 255]);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create image directory");
    }
    image::DynamicImage::ImageRgba8(image)
        .save_with_format(path, image::ImageFormat::Png)
        .expect("write PNG fixture");
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    walk_snapshot(root, root, &mut files);
    files
}

fn walk_snapshot(root: &Path, current: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
    let entries = fs::read_dir(current).expect("read fixture directory");
    for entry in entries {
        let entry = entry.expect("fixture entry");
        let path = entry.path();
        let metadata = fs::symlink_metadata(&path).expect("fixture metadata");
        if metadata.file_type().is_symlink() {
            let target = fs::read_link(&path).expect("fixture symlink target");
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            files.insert(
                relative,
                format!("symlink:{}", target.display()).into_bytes(),
            );
        } else if metadata.is_dir() {
            walk_snapshot(root, &path, files);
        } else if metadata.is_file() {
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            files.insert(relative, fs::read(&path).expect("read fixture file"));
        }
    }
}
