//! Built-in TCP bridge used for the GDScript language server.
//!
//! Zed launches this process with piped stdio. The bridge connects to a Godot
//! editor's TCP language server and forwards raw bytes in both directions:
//! stdin -> TCP and TCP -> stdout. It performs no resource indexing, preview
//! work or editor-process management, and it keeps stdout strictly
//! protocol-only: every forwarded chunk is flushed explicitly because JSON-RPC
//! bodies are not required to end with a newline.
//!
//! Shutdown is intentionally process-exit based. Each forwarding direction runs
//! in a thread and reports its outcome through a channel; the first direction to
//! finish closes the connection (EOF, error, or write failure) and the main
//! thread returns, terminating the process. In particular the process never
//! joins the stdin reader, which may be blocked in `read` when the TCP peer
//! closes.

use std::{
    error::Error,
    fmt,
    io::{self, Read, Write},
    net::{TcpStream, ToSocketAddrs},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

pub const DEFAULT_HOST: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 6005;
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 5000;

/// Size of each forwarding chunk. Chunk boundaries have no protocol meaning;
/// bytes are copied verbatim.
const CHUNK_BYTES: usize = 16 * 1024;

pub type DynError = Box<dyn Error + Send + Sync>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    pub host: String,
    pub port: u16,
    pub connect_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.to_string(),
            port: DEFAULT_PORT,
            connect_timeout: Duration::from_millis(DEFAULT_CONNECT_TIMEOUT_MS),
        }
    }
}

/// Connect to the configured TCP endpoint and forward bytes until either
/// direction closes.
pub fn run(options: Options) -> Result<(), DynError> {
    let stream = connect(&options)?;
    // Latency matters more than throughput for language-server traffic. Failure
    // to set the option is not fatal; forwarding still works correctly.
    let _ = stream.set_nodelay(true);

    let read_stream = stream
        .try_clone()
        .map_err(|error| io::Error::other(format!("failed to clone TCP stream: {error}")))?;
    let write_stream = stream;

    let (sender, receiver) = mpsc::channel::<Result<(), ForwardError>>();

    let stdin_sender = sender.clone();
    thread::Builder::new()
        .name("gdfiles-bridge-stdin".to_string())
        .spawn(move || {
            let result = forward_stdin_to_tcp(write_stream).map_err(|source| ForwardError {
                direction: Direction::StdinToTcp,
                source,
            });
            let _ = stdin_sender.send(result);
        })?;

    thread::Builder::new()
        .name("gdfiles-bridge-stdout".to_string())
        .spawn(move || {
            let result = forward_tcp_to_stdout(read_stream).map_err(|source| ForwardError {
                direction: Direction::TcpToStdout,
                source,
            });
            let _ = sender.send(result);
        })?;

    // The first completed direction decides the process lifetime. Both threads
    // send exactly one result, so this is a blocking wait without polling.
    match receiver.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(Box::new(error)),
        Err(_) => Err(Box::new(io::Error::other(
            "bridge forwarding threads terminated without reporting a result",
        ))),
    }
}

fn connect(options: &Options) -> Result<TcpStream, DynError> {
    let deadline = Instant::now()
        .checked_add(options.connect_timeout)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "connect timeout is too large")
        })?;
    let addresses = (options.host.as_str(), options.port)
        .to_socket_addrs()
        .map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed to resolve host '{}': {error}", options.host),
            )
        })?;

    let mut last_error = None;
    let mut attempted = false;
    for address in addresses {
        attempted = true;
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        if remaining.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }

    let detail = match last_error {
        Some(error) => error.to_string(),
        None if !attempted => "host did not resolve to any address".to_string(),
        None => "connection attempt timed out".to_string(),
    };
    Err(Box::new(io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "failed to connect to {}:{} within {} ms: {detail}",
            options.host,
            options.port,
            options.connect_timeout.as_millis()
        ),
    )))
}

fn forward_stdin_to_tcp(mut stream: TcpStream) -> io::Result<()> {
    let stdin = io::stdin();
    let mut stdin = stdin.lock();
    copy_chunks(&mut stdin, &mut stream, false)
}

fn forward_tcp_to_stdout(mut stream: TcpStream) -> io::Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    copy_chunks(&mut stream, &mut stdout, true)
}

fn copy_chunks(
    reader: &mut impl Read,
    writer: &mut impl Write,
    flush_each_chunk: bool,
) -> io::Result<()> {
    let mut buffer = [0u8; CHUNK_BYTES];
    loop {
        let read = match reader.read(&mut buffer) {
            Ok(0) => return Ok(()),
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        writer.write_all(&buffer[..read])?;
        if flush_each_chunk {
            // JSON-RPC frames can be complete without a trailing newline, so
            // relying on line buffering would deadlock the client.
            writer.flush()?;
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Direction {
    StdinToTcp,
    TcpToStdout,
}

impl fmt::Display for Direction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StdinToTcp => formatter.write_str("stdin -> tcp"),
            Self::TcpToStdout => formatter.write_str("tcp -> stdout"),
        }
    }
}

#[derive(Debug)]
struct ForwardError {
    direction: Direction,
    source: io::Error,
}

impl fmt::Display for ForwardError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "bridge {} failed: {}",
            self.direction, self.source
        )
    }
}

impl Error for ForwardError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}
