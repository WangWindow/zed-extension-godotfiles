//! Integration tests for the built-in GDScript TCP bridge.
//!
//! Every test spawns the real `gdfiles-lsp` executable and connects it
//! to a local TCP listener; no Godot process, network access or external
//! utility (`nc`, `socat`, ...) is involved. All waits are bounded and any
//! surviving child is killed before a test panics.

use std::{
    io::{Read, Write},
    net::{Shutdown, TcpListener, TcpStream},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(8);

fn executable() -> &'static str {
    env!("CARGO_BIN_EXE_gdfiles-lsp")
}

/// Owns a child process so a panic cannot leave it running.
struct ChildHandle {
    child: Child,
}

impl ChildHandle {
    fn spawn(arguments: &[&str]) -> Self {
        let child = Command::new(executable())
            .args(arguments)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn gdfiles-lsp");
        Self { child }
    }

    fn child(&mut self) -> &mut Child {
        &mut self.child
    }
}

impl Drop for ChildHandle {
    fn drop(&mut self) {
        let _ = self.child.try_wait();
        if let Ok(None) = self.child.try_wait() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

fn spawn_bridge(port: &str) -> ChildHandle {
    ChildHandle::spawn(&[
        "--gdscript-lsp",
        "--host",
        "127.0.0.1",
        "--port",
        port,
        "--connect-timeout-ms",
        "3000",
    ])
}

fn wait_for_exit(child: &mut Child) -> ExitStatus {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait().expect("poll child status") {
            Some(status) => return status,
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("bridge process did not exit within {TIMEOUT:?}");
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

fn accept_bounded(listener: &TcpListener) -> TcpStream {
    listener
        .set_nonblocking(true)
        .expect("listener nonblocking mode");
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_nonblocking(false)
                    .expect("accepted stream blocking mode");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    panic!("bridge did not connect within {TIMEOUT:?}");
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("listener accept failed: {error}"),
        }
    }
}

fn listener_for_nondefault_port() -> (TcpListener, String) {
    loop {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind local listener");
        let port = listener.local_addr().expect("listener address").port();
        if port != 6005 {
            return (listener, port.to_string());
        }
    }
}

fn framed(body: &str) -> Vec<u8> {
    let mut bytes = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    bytes.extend_from_slice(body.as_bytes());
    bytes
}

/// Writes `bytes` split at `cuts`, flushing after every chunk. This exercises
/// framing headers, bodies and arbitrary UTF-8 boundaries arriving separately.
fn write_in_chunks(writer: &mut impl Write, bytes: &[u8], cuts: &[usize]) {
    let mut start = 0;
    for &cut in cuts {
        if cut > start && cut < bytes.len() {
            writer.write_all(&bytes[start..cut]).expect("write chunk");
            writer.flush().expect("flush chunk");
            start = cut;
            thread::sleep(Duration::from_millis(2));
        }
    }
    writer.write_all(&bytes[start..]).expect("write tail");
    writer.flush().expect("flush tail");
}

fn run_to_completion(arguments: &[&str]) -> (ExitStatus, Vec<u8>, Vec<u8>) {
    let mut child = Command::new(executable())
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn gdfiles-lsp");
    let status = wait_for_exit(&mut child);
    let mut stdout = Vec::new();
    child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_end(&mut stdout)
        .expect("read child stdout");
    let mut stderr = Vec::new();
    child
        .stderr
        .take()
        .expect("child stderr")
        .read_to_end(&mut stderr)
        .expect("read child stderr");
    (status, stdout, stderr)
}

#[test]
fn bridge_forwards_exact_bytes_in_both_directions_with_split_lsp_frames() {
    let (listener, port) = listener_for_nondefault_port();

    // Two complete JSON-RPC frames, including non-ASCII UTF-8 and no final
    // newline. Exact byte equality is the core bridge guarantee.
    let client_payload = [
        framed(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"café ✓"}}}"#,
        ),
        framed(r#"{"jsonrpc":"2.0","method":"initialized","params":{}}"#),
    ]
    .concat();
    let server_payload = [
        framed(
            r#"{"jsonrpc":"2.0","id":1,"result":{"capabilities":{"hoverProvider":true,"definitionProvider":true}}}"#,
        ),
        framed(
            r#"{"jsonrpc":"2.0","method":"window/logMessage","params":{"type":3,"message":"héllo ✓"}}"#,
        ),
    ]
    .concat();
    assert!(!client_payload.ends_with(b"\n"));
    assert!(!server_payload.ends_with(b"\n"));

    let mut child = spawn_bridge(&port);
    let mut stdin = child.child.stdin.take().expect("child stdin");

    let expected_client_len = client_payload.len();
    let server_payload_for_thread = server_payload.clone();
    let server = thread::spawn(move || {
        let mut stream = accept_bounded(&listener);
        let mut received = vec![0u8; expected_client_len];
        stream
            .read_exact(&mut received)
            .expect("server read of forwarded client bytes");
        let length = server_payload_for_thread.len();
        let cuts = [1, 5, 17, length / 2, length - 1];
        write_in_chunks(&mut stream, &server_payload_for_thread, &cuts);
        // Closing the TCP side must terminate the bridge even though the
        // client's stdin remains open.
        let _ = stream.shutdown(Shutdown::Both);
        received
    });

    let length = client_payload.len();
    let cuts = [1, 4, 19, length / 2, length - 1];
    write_in_chunks(&mut stdin, &client_payload, &cuts);

    let received = server.join().expect("server fixture thread");
    assert_eq!(
        received, client_payload,
        "stdin -> TCP bytes must be forwarded verbatim"
    );

    let status = wait_for_exit(child.child());
    let mut stdout = Vec::new();
    child
        .child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_end(&mut stdout)
        .expect("read child stdout");
    let mut stderr = Vec::new();
    child
        .child
        .stderr
        .take()
        .expect("child stderr")
        .read_to_end(&mut stderr)
        .expect("read child stderr");

    assert!(
        status.success(),
        "bridge exit status {status}; stderr: {}",
        String::from_utf8_lossy(&stderr)
    );
    assert_eq!(
        stdout, server_payload,
        "TCP -> stdout bytes must be forwarded verbatim without added metadata"
    );
    assert!(
        stderr.is_empty(),
        "bridge wrote diagnostics to stderr: {}",
        String::from_utf8_lossy(&stderr)
    );

    // Kept open until after the process exited to prove TCP-close shutdown.
    drop(stdin);
}

#[test]
fn bridge_uses_explicit_nondefault_port() {
    let (listener, port) = listener_for_nondefault_port();
    // Raw bytes, including invalid UTF-8 and NUL, must not be reinterpreted.
    let server_payload = b"server\xff\x00\x80-says-hi-without-newline".to_vec();
    let client_payload = b"client\xfe\x00\x81-says-hi-without-newline".to_vec();

    let mut child = spawn_bridge(&port);
    let mut stdin = child.child.stdin.take().expect("child stdin");

    let response = server_payload.clone();
    let expected = client_payload.clone();
    let server = thread::spawn(move || {
        let mut stream = accept_bounded(&listener);
        let mut received = vec![0u8; expected.len()];
        stream
            .read_exact(&mut received)
            .expect("server read of client payload");
        stream.write_all(&response).expect("server write");
        stream.flush().expect("server flush");
        let _ = stream.shutdown(Shutdown::Both);
        received
    });

    write_in_chunks(&mut stdin, &client_payload, &[3, 11]);
    let received = server.join().expect("server fixture thread");
    assert_eq!(received, client_payload);

    let status = wait_for_exit(child.child());
    let mut stdout = Vec::new();
    child
        .child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_end(&mut stdout)
        .expect("read child stdout");
    assert!(status.success(), "bridge exit status {status}");
    assert_eq!(stdout, server_payload);
    drop(stdin);
}

#[test]
fn tcp_close_terminates_with_stdin_still_open() {
    let (listener, port) = listener_for_nondefault_port();
    let mut child = spawn_bridge(&port);
    let stdin = child.child.stdin.take().expect("child stdin");

    let stream = accept_bounded(&listener);
    drop(stream);

    let status = wait_for_exit(child.child());
    let mut stdout = Vec::new();
    child
        .child
        .stdout
        .take()
        .expect("child stdout")
        .read_to_end(&mut stdout)
        .expect("read child stdout");
    assert!(
        status.success(),
        "TCP close should shut the bridge down cleanly, got {status}"
    );
    assert!(
        stdout.is_empty(),
        "bridge wrote non-forwarded bytes to stdout: {:?}",
        String::from_utf8_lossy(&stdout)
    );
    drop(stdin);
}

#[test]
fn stdin_eof_terminates_with_tcp_connection_alive() {
    let (listener, port) = listener_for_nondefault_port();
    let mut child = spawn_bridge(&port);
    let stdin = child.child.stdin.take().expect("child stdin");

    let stream = accept_bounded(&listener);
    drop(stdin);

    let status = wait_for_exit(child.child());
    assert!(
        status.success(),
        "stdin EOF should shut the bridge down cleanly, got {status}"
    );
    drop(stream);
}

#[test]
fn invalid_cli_arguments_fail_without_stdout() {
    let cases: &[&[&str]] = &[
        &["--gdscript-lsp", "--port", "70000"],
        &["--gdscript-lsp", "--port", "not-a-port"],
        &["--gdscript-lsp", "--port", "0"],
        &["--gdscript-lsp", "--connect-timeout-ms", "0"],
        &["--gdscript-lsp", "--connect-timeout-ms", "soon"],
        &["--gdscript-lsp", "--host"],
        &["--gdscript-lsp", "--port"],
        &["--stdio", "--gdscript-lsp"],
        &["--host", "127.0.0.1"],
        &["--port", "6005"],
        &["--bogus"],
    ];

    for arguments in cases {
        let (status, stdout, stderr) = run_to_completion(arguments);
        assert!(!status.success(), "{arguments:?} unexpectedly succeeded");
        assert!(
            stdout.is_empty(),
            "{arguments:?} wrote to stdout: {:?}",
            String::from_utf8_lossy(&stdout)
        );
        assert!(
            !stderr.is_empty(),
            "{arguments:?} produced no diagnostic on stderr"
        );
    }
}

#[test]
fn connect_failure_is_reported_without_polluting_stdout() {
    // Reserve an ephemeral port and release it so the connection is refused.
    let port = {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind reservation");
        listener.local_addr().expect("reserved address").port()
    }
    .to_string();

    let (status, stdout, stderr) = run_to_completion(&[
        "--gdscript-lsp",
        "--host",
        "127.0.0.1",
        "--port",
        &port,
        "--connect-timeout-ms",
        "1000",
    ]);

    assert!(!status.success(), "connect failure must exit nonzero");
    assert!(
        stdout.is_empty(),
        "connect failure wrote to stdout: {:?}",
        String::from_utf8_lossy(&stdout)
    );
    let stderr = String::from_utf8_lossy(&stderr);
    assert!(
        stderr.contains("connect"),
        "connect failure diagnostic missing: {stderr}"
    );
}
