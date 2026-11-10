//! Real stdio preview-format tests.
//!
//! These spawn the actual `gdfiles-lsp` binary over Content-Length
//! framed JSON-RPC and use tempfile fixtures only. No Godot process, editor GUI,
//! network or system fonts are involved. Every wait is bounded and the child is
//! killed on failure.

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    path::Path,
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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_PREVIEW_BYTES: usize = 48 * 1024;
const MAX_PREVIEW_WIDTH: u32 = 192;
const MAX_PREVIEW_HEIGHT: u32 = 128;

#[test]
fn new_raster_formats_preview_and_are_never_text_definitions() {
    let fixture = Fixture::new();
    let (mut client, registration) = start_client(fixture.root());
    assert_registration(&registration);

    let main = fixture.root().join("scenes/main.tscn");
    let main_text = fs::read_to_string(&main).unwrap();
    client.did_open(&main, &main_text, "godot-resource");

    for (needle, expected_aspect) in [
        ("res://assets/icon.ico", 64.0 / 64.0),
        ("res://assets/icon.bmp", 120.0 / 80.0),
        ("res://assets/icon.gif", 80.0 / 60.0),
        ("res://assets/icon.tga", 100.0 / 40.0),
        ("res://assets/icon.tiff", 90.0 / 70.0),
        ("res://assets/icon.tif", 90.0 / 70.0),
        ("res://assets/icon.dds", 4.0 / 4.0),
        ("res://assets/icon.hdr", 80.0 / 50.0),
    ] {
        let position = position_at(&main_text, needle, 1);
        let hover = client.hover(&main, position.clone());
        let markdown = hover_markdown(&hover).expect("image metadata hover");
        assert!(
            markdown.contains(needle.rsplit('/').next().unwrap()),
            "hover metadata for {needle}: {markdown}"
        );

        let png = preview_png(markdown);
        assert!(
            png.starts_with(b"\x89PNG\r\n\x1a\n"),
            "{needle} preview must be PNG"
        );
        let image = image::load_from_memory(&png).expect("decode preview PNG");
        assert!(
            image.width() > 0
                && image.height() > 0
                && image.width() <= MAX_PREVIEW_WIDTH
                && image.height() <= MAX_PREVIEW_HEIGHT,
            "{needle} preview bounds: {}x{}",
            image.width(),
            image.height()
        );
        let aspect = image.width() as f32 / image.height() as f32;
        assert!(
            (aspect - expected_aspect).abs() < 0.05,
            "{needle} aspect ratio: {}x{} (expected {expected_aspect})",
            image.width(),
            image.height()
        );

        let definition = client.definition(&main, position);
        assert!(
            definition["result"].is_null(),
            "{needle} must never be a text definition: {definition}"
        );
    }

    // Corrupt files are metadata-only: no preview and no definition.
    for needle in ["res://assets/corrupt.ico", "res://assets/corrupt.png"] {
        let position = position_at(&main_text, needle, 1);
        let hover = client.hover(&main, position.clone());
        let markdown = hover_markdown(&hover).expect("corrupt image metadata hover");
        assert!(
            markdown.contains(needle.rsplit('/').next().unwrap()),
            "corrupt hover metadata for {needle}: {markdown}"
        );
        assert!(
            !markdown.contains("data:image/"),
            "corrupt image produced a preview: {markdown}"
        );
        let definition = client.definition(&main, position);
        assert!(
            definition["result"].is_null(),
            "corrupt binary definition for {needle}: {definition}"
        );
    }

    client.shutdown();
}

#[test]
fn svg_preview_rasterizes_locally_and_never_loads_external_resources() {
    let fixture = Fixture::new();
    let (mut client, registration) = start_client(fixture.root());
    assert_registration(&registration);

    let main = fixture.root().join("scenes/main.tscn");
    let main_text = fs::read_to_string(&main).unwrap();
    client.did_open(&main, &main_text, "godot-resource");

    // A normal vector image keeps its aspect ratio and is bounded to the
    // preview box.
    let position = position_at(&main_text, "res://assets/vector.svg", 1);
    let hover = client.hover(&main, position);
    let markdown = hover_markdown(&hover).expect("SVG metadata hover");
    assert!(markdown.contains("vector.svg"), "SVG hover: {markdown}");
    let png = preview_png(markdown);
    let image = image::load_from_memory(&png).expect("decode SVG preview");
    assert_eq!(image.width(), MAX_PREVIEW_WIDTH);
    assert!(
        (75..=79).contains(&image.height()),
        "SVG aspect ratio: {}x{}",
        image.width(),
        image.height()
    );
    let rgba = image.to_rgba8();
    let center = rgba.get_pixel(MAX_PREVIEW_WIDTH / 2, image.height() / 2);
    assert!(
        center[3] > 200 && center[0] > 200,
        "SVG fill was not rasterized: {center:?}"
    );

    // The fixture SVG references an absolute path to a bright PNG outside the
    // project. If the renderer loaded it, the preview would contain opaque
    // pixels. Deterministically, every pixel must stay transparent.
    let position = position_at(&main_text, "res://assets/external.svg", 1);
    let hover = client.hover(&main, position);
    let markdown = hover_markdown(&hover).expect("external SVG metadata hover");
    let png = preview_png(markdown);
    let image = image::load_from_memory(&png).expect("decode external SVG preview");
    let rgba = image.to_rgba8();
    assert!(
        rgba.pixels().all(|pixel| pixel[3] == 0),
        "external SVG resource was loaded: {}x{}",
        image.width(),
        image.height()
    );

    // Filtered SVG is deliberately metadata-only because filters are not
    // bounded by pixel dimensions.
    let position = position_at(&main_text, "res://assets/filtered.svg", 1);
    let hover = client.hover(&main, position);
    let markdown = hover_markdown(&hover).expect("filtered SVG metadata hover");
    assert!(
        markdown.contains("filtered.svg"),
        "filtered hover: {markdown}"
    );
    assert!(
        !markdown.contains("data:image/"),
        "filtered SVG produced an unbounded preview: {markdown}"
    );

    client.shutdown();
}

#[test]
fn small_sources_preview_at_native_size_and_large_sources_fit_without_cropping() {
    let fixture = Fixture::new();
    let (mut client, registration) = start_client(fixture.root());
    assert_registration(&registration);

    let main = fixture.root().join("scenes/main.tscn");
    let main_text = fs::read_to_string(&main).unwrap();
    client.did_open(&main, &main_text, "godot-resource");

    // 8x8 ball-shaped asset with transparent magenta pixels, mirroring a tiny
    // pixel-art PNG whose transparent palette entries must never bleed into
    // visible purple fringes.
    let source = image::open(fixture.root().join("assets/ball.png"))
        .expect("decode ball fixture")
        .to_rgba8();
    let position = position_at(&main_text, "res://assets/ball.png", 1);
    let hover = client.hover(&main, position);
    let markdown = hover_markdown(&hover).expect("ball preview hover");
    let preview = image::load_from_memory(&preview_png(markdown))
        .expect("decode ball preview")
        .to_rgba8();

    assert_eq!(
        preview.dimensions(),
        source.dimensions(),
        "an 8x8 preview must not be upscaled"
    );
    for (x, y, pixel) in preview.enumerate_pixels() {
        let expected = source.get_pixel(x, y);
        assert_eq!(
            pixel[3], expected[3],
            "ball preview alpha changed at {x},{y}: {pixel:?} vs {expected:?}"
        );
        if pixel[3] > 0 {
            assert_eq!(
                pixel[3], 255,
                "ball preview gained a semi-transparent fringe at {x},{y}: {pixel:?}"
            );
            assert_eq!(
                pixel, expected,
                "ball preview pixel changed at {x},{y}: {pixel:?} vs {expected:?}"
            );
        }
    }

    // The reported user flow: hovering `source_file=` in a `.import` file. The
    // hover must keep the import type and open link while showing the tiny
    // source at its native size.
    let import = fixture.root().join("assets/ball.png.import");
    let import_text = fs::read_to_string(&import).expect("read import fixture");
    client.did_open(&import, &import_text, "godot-resource");
    let position = position_at(&import_text, "res://assets/ball.png", 1);
    let hover = client.hover(&import, position);
    let markdown = hover_markdown(&hover).expect("import source hover");
    assert!(
        markdown.contains("- res://assets/ball.png (CompressedTexture2D) [open](<file://"),
        "import hover should keep its type and open link: {markdown}"
    );
    let preview = image::load_from_memory(&preview_png(markdown))
        .expect("decode import source preview")
        .to_rgba8();
    assert_eq!(
        preview.dimensions(),
        (8, 8),
        "import source preview must stay at the 8x8 native size"
    );

    // Small portrait and wide sources are exact too: no upscale, no crop and
    // no aspect-ratio distortion.
    for (file_name, width, height) in [("tall.png", 7, 13), ("banner.png", 13, 7)] {
        let source = image::open(fixture.root().join("assets").join(file_name))
            .expect("decode small fixture")
            .to_rgba8();
        assert_eq!((source.width(), source.height()), (width, height));

        let needle = format!("res://assets/{file_name}");
        let position = position_at(&main_text, &needle, 1);
        let hover = client.hover(&main, position);
        let markdown = hover_markdown(&hover).expect("small preview hover");
        let preview = image::load_from_memory(&preview_png(markdown))
            .expect("decode small preview")
            .to_rgba8();

        assert_eq!(
            (preview.width(), preview.height()),
            (width, height),
            "{file_name} must keep native preview dimensions"
        );
        assert_eq!(
            preview.as_raw(),
            source.as_raw(),
            "{file_name} preview pixels were cropped or resampled"
        );
    }

    // A larger raster is downscaled into the hover box and keeps all four
    // quadrants, so the thumbnail is bounded without cropping.
    let position = position_at(&main_text, "res://assets/large.png", 1);
    let hover = client.hover(&main, position);
    let markdown = hover_markdown(&hover).expect("large preview hover");
    let preview = image::load_from_memory(&preview_png(markdown))
        .expect("decode large preview")
        .to_rgba8();
    assert_eq!(
        (preview.width(), preview.height()),
        (MAX_PREVIEW_WIDTH, MAX_PREVIEW_HEIGHT),
        "480x320 source should fill the 192x128 preview box"
    );
    for (x, y, expected) in [
        (20, 20, [220, 20, 20]),
        (MAX_PREVIEW_WIDTH - 20, 20, [20, 220, 20]),
        (20, MAX_PREVIEW_HEIGHT - 20, [20, 20, 220]),
        (
            MAX_PREVIEW_WIDTH - 20,
            MAX_PREVIEW_HEIGHT - 20,
            [230, 230, 20],
        ),
    ] {
        let pixel = preview.get_pixel(x, y);
        for (channel, value) in expected.into_iter().enumerate() {
            let actual = pixel[channel];
            if value > 128 {
                assert!(
                    actual > 170,
                    "quadrant {expected:?} missing at {x},{y}: {pixel:?}"
                );
            } else {
                assert!(
                    actual < 80,
                    "quadrant {expected:?} leaked at {x},{y}: {pixel:?}"
                );
            }
        }
    }

    client.shutdown();
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

    fn initialize(&mut self, root: &Path) -> Value {
        let root_uri = file_uri(root);
        self.request(
            "initialize",
            json!({
                "processId": null,
                "clientInfo": { "name": "gdfiles-preview-format-test", "version": "1" },
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
            }),
        )
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

        self.notify("exit", Value::Null);
        drop(self.stdin.take());

        let deadline = Instant::now() + PROCESS_EXIT_TIMEOUT;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    assert!(status.success(), "server exit status: {status}");
                    break;
                }
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
        }

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
        let _ = self.stdin.take();
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn start_client(root: &Path) -> (LspClient, Value) {
    let mut client = LspClient::spawn();
    let initialize = client.initialize(root);
    assert!(
        initialize.get("error").is_none(),
        "initialize failed: {initialize}"
    );
    client.notify("initialized", json!({}));
    let registration = client.expect_server_request("client/registerCapability");
    (client, registration)
}

fn assert_registration(registration: &Value) {
    let registrations = registration["params"]["registrations"]
        .as_array()
        .expect("registrations array");
    assert!(!registrations.is_empty());
    assert_eq!(
        registrations[0]["method"],
        "workspace/didChangeWatchedFiles"
    );
}

// ---------------------------------------------------------------------------
// Wire helpers
// ---------------------------------------------------------------------------

fn hover_markdown(response: &Value) -> Option<&str> {
    response
        .pointer("/result/contents/value")
        .and_then(Value::as_str)
}

fn preview_png(markdown: &str) -> Vec<u8> {
    const PREFIX: &str = "data:image/png;base64,";
    let start = markdown.find(PREFIX).expect("preview data URI") + PREFIX.len();
    let rest = &markdown[start..];
    let end = rest
        .find(|character: char| {
            character == '>' || character == ')' || character == '"' || character.is_whitespace()
        })
        .unwrap_or(rest.len());
    let png = BASE64.decode(&rest[..end]).expect("base64 preview");
    assert!(
        png.len() <= MAX_PREVIEW_BYTES,
        "preview bytes: {}",
        png.len()
    );
    png
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
            "; Engine configuration file.\n[application]\nconfig/name=\"Preview Formats\"\n",
        );

        write_noise_image(
            &root.join("assets/icon.ico"),
            image::ImageFormat::Ico,
            64,
            64,
        );
        write_noise_image(
            &root.join("assets/icon.bmp"),
            image::ImageFormat::Bmp,
            120,
            80,
        );
        write_noise_image(
            &root.join("assets/icon.gif"),
            image::ImageFormat::Gif,
            80,
            60,
        );
        write_noise_image(
            &root.join("assets/icon.tga"),
            image::ImageFormat::Tga,
            100,
            40,
        );
        write_noise_image(
            &root.join("assets/icon.tiff"),
            image::ImageFormat::Tiff,
            90,
            70,
        );
        fs::copy(root.join("assets/icon.tiff"), root.join("assets/icon.tif"))
            .expect("copy tif fixture");
        write_dds(&root.join("assets/icon.dds"));
        write_hdr(&root.join("assets/icon.hdr"), 80, 50);

        write_pixels_png(&root.join("assets/ball.png"), 8, 8, &ball_pixels());
        write_pixels_png(&root.join("assets/tall.png"), 7, 13, &pattern_pixels(7, 13));
        write_pixels_png(
            &root.join("assets/banner.png"),
            13,
            7,
            &pattern_pixels(13, 7),
        );
        write_pixels_png(
            &root.join("assets/large.png"),
            480,
            320,
            &quadrant_pixels(480, 320),
        );
        write(
            &root.join("assets/ball.png.import"),
            "[remap]\n\n\
             importer=\"texture\"\n\
             type=\"CompressedTexture2D\"\n\
             uid=\"uid://b8pubh3ddqqmd\"\n\
             path=\"res://.godot/imported/ball.png-abc.ctex\"\n\n\
             [deps]\n\n\
             source_file=\"res://assets/ball.png\"\n\
             dest_files=[\"res://.godot/imported/ball.png-abc.ctex\"]\n",
        );

        fs::write(root.join("assets/corrupt.ico"), b"not an icon").expect("corrupt ico");
        fs::write(root.join("assets/corrupt.png"), [0xffu8, 0xfe, 0xfd, 0xfc])
            .expect("corrupt png");

        let outside_png = outside.path().join("outside-bright.png");
        write_solid_png(&outside_png, 64, 64, [255, 0, 255, 255]);
        write(
            &root.join("assets/external.svg"),
            &format!(
                "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\">\
                 <image href=\"{}\" width=\"64\" height=\"64\"/>\
                 </svg>",
                outside_png.display()
            ),
        );
        write(
            &root.join("assets/vector.svg"),
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"300\" height=\"120\">\
             <rect width=\"300\" height=\"120\" fill=\"#ff0000\"/>\
             </svg>",
        );
        write(
            &root.join("assets/filtered.svg"),
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"64\" height=\"64\">\
             <defs><filter id=\"b\"><feGaussianBlur stdDeviation=\"2\"/></filter></defs>\
             <rect width=\"64\" height=\"64\" fill=\"#00ff00\" filter=\"url(#b)\"/>\
             </svg>",
        );

        write(&root.join("scenes/main.tscn"), &main_scene_text());

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
    let assets = [
        ("Texture2D", "res://assets/icon.ico", "1_ico"),
        ("Texture2D", "res://assets/icon.bmp", "2_bmp"),
        ("Texture2D", "res://assets/icon.gif", "3_gif"),
        ("Texture2D", "res://assets/icon.tga", "4_tga"),
        ("Texture2D", "res://assets/icon.tiff", "5_tiff"),
        ("Texture2D", "res://assets/icon.tif", "6_tif"),
        ("Texture2D", "res://assets/icon.dds", "7_dds"),
        ("Texture2D", "res://assets/icon.hdr", "8_hdr"),
        ("Texture2D", "res://assets/vector.svg", "9_svg"),
        ("Texture2D", "res://assets/external.svg", "10_external"),
        ("Texture2D", "res://assets/filtered.svg", "11_filtered"),
        ("Texture2D", "res://assets/corrupt.ico", "12_corrupt_ico"),
        ("Texture2D", "res://assets/corrupt.png", "13_corrupt_png"),
        ("Texture2D", "res://assets/ball.png", "14_ball"),
        ("Texture2D", "res://assets/tall.png", "15_tall"),
        ("Texture2D", "res://assets/banner.png", "16_banner"),
        ("Texture2D", "res://assets/large.png", "17_large"),
    ];

    let mut text = format!("[gd_scene load_steps={} format=3]\n\n", assets.len() + 1);
    for (resource_type, path, id) in assets {
        text.push_str(&format!(
            "[ext_resource type=\"{resource_type}\" path=\"{path}\" id=\"{id}\"]\n"
        ));
    }
    text.push_str("\n[node name=\"Main\" type=\"Node\"]\n");
    text
}

fn write(path: &Path, contents: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create fixture directory");
    }
    fs::write(path, contents).expect("write fixture file");
}

fn write_noise_image(path: &Path, format: image::ImageFormat, width: u32, height: u32) {
    let mut image = image::RgbaImage::new(width, height);
    let mut state = 0x1234_5678u32;
    for pixel in image.pixels_mut() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        *pixel = image::Rgba([
            (state >> 24) as u8,
            (state >> 16) as u8,
            (state >> 8) as u8,
            255,
        ]);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create image directory");
    }
    image::DynamicImage::ImageRgba8(image)
        .save_with_format(path, format)
        .expect("write image fixture");
}

fn write_solid_png(path: &Path, width: u32, height: u32, color: [u8; 4]) {
    image::RgbaImage::from_pixel(width, height, image::Rgba(color))
        .save_with_format(path, image::ImageFormat::Png)
        .expect("write solid PNG");
}

fn write_pixels_png(path: &Path, width: u32, height: u32, pixels: &[[u8; 4]]) {
    assert_eq!(pixels.len(), (width * height) as usize);
    let data: Vec<u8> = pixels.iter().flatten().copied().collect();
    let image = image::RgbaImage::from_raw(width, height, data).expect("matching pixel buffer");
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create image directory");
    }
    image
        .save_with_format(path, image::ImageFormat::Png)
        .expect("write pixel PNG");
}

/// An 8x8 pixel-art ball: an opaque white circle on transparent corners. The
/// transparent palette entries intentionally carry magenta RGB values so a
/// resampling bug shows up as a purple fringe instead of staying invisible.
fn ball_pixels() -> Vec<[u8; 4]> {
    const TRANSPARENT: [u8; 4] = [255, 0, 255, 0];
    const WHITE: [u8; 4] = [255, 255, 255, 255];
    let mut pixels = Vec::with_capacity(8 * 8);
    for y in 0..8 {
        for x in 0..8 {
            let middle_rows = (2..=5).contains(&y);
            let middle_columns = (2..=5).contains(&x);
            pixels.push(if middle_rows || middle_columns {
                WHITE
            } else {
                TRANSPARENT
            });
        }
    }
    pixels
}

fn pattern_pixels(width: u32, height: u32) -> Vec<[u8; 4]> {
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.push([
                (x * 37 + y * 11) as u8,
                (x * 5 + y * 43) as u8,
                (x * 29 + y * 7) as u8,
                255,
            ]);
        }
    }
    pixels
}

/// Four solid quadrants make cropping immediately visible.
fn quadrant_pixels(width: u32, height: u32) -> Vec<[u8; 4]> {
    let mut pixels = Vec::with_capacity((width * height) as usize);
    for y in 0..height {
        for x in 0..width {
            pixels.push(match (x < width / 2, y < height / 2) {
                (true, true) => [220, 20, 20, 255],
                (false, true) => [20, 220, 20, 255],
                (true, false) => [20, 20, 220, 255],
                (false, false) => [230, 230, 20, 255],
            });
        }
    }
    pixels
}

fn write_hdr(path: &Path, width: u32, height: u32) {
    let mut image = image::Rgb32FImage::new(width, height);
    for pixel in image.pixels_mut() {
        *pixel = image::Rgb([0.25, 0.5, 0.75]);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create HDR directory");
    }
    image::DynamicImage::ImageRgb32F(image)
        .save_with_format(path, image::ImageFormat::Hdr)
        .expect("write HDR fixture");
}

/// Minimal valid 4x4 DXT1 DDS. The `image` crate has no DDS encoder, so this
/// is a hand-built header plus one solid-color compressed block.
fn write_dds(path: &Path) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"DDS ");
    bytes.extend_from_slice(&124u32.to_le_bytes()); // header size
    bytes.extend_from_slice(&0x0008_1007u32.to_le_bytes()); // caps|height|width|pixelformat|linearsize
    bytes.extend_from_slice(&4u32.to_le_bytes()); // height
    bytes.extend_from_slice(&4u32.to_le_bytes()); // width
    bytes.extend_from_slice(&8u32.to_le_bytes()); // linear size
    bytes.extend_from_slice(&0u32.to_le_bytes()); // depth
    bytes.extend_from_slice(&0u32.to_le_bytes()); // mipmap count
    bytes.extend_from_slice(&[0u8; 44]); // reserved
    bytes.extend_from_slice(&32u32.to_le_bytes()); // pixel format size
    bytes.extend_from_slice(&0x4u32.to_le_bytes()); // DDPF_FOURCC
    bytes.extend_from_slice(b"DXT1");
    bytes.extend_from_slice(&[0u8; 20]); // rgb bit counts/masks
    bytes.extend_from_slice(&0x1000u32.to_le_bytes()); // caps: texture
    bytes.extend_from_slice(&[0u8; 16]); // caps2..4 + reserved
    bytes.extend_from_slice(&0xF800u16.to_le_bytes()); // color0: red
    bytes.extend_from_slice(&0x001Fu16.to_le_bytes()); // color1: blue
    bytes.extend_from_slice(&0u32.to_le_bytes()); // all indices -> color0
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("create DDS directory");
    }
    fs::write(path, bytes).expect("write DDS fixture");
}
