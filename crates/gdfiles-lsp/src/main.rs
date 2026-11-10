use std::{env, process::ExitCode, time::Duration};

mod bridge;
mod preview;
mod server;

// The resource index and its wire-neutral contract types live in the crate
// library. Re-export them at this binary root so the protocol modules can use
// the exact same shared interface.
pub use gdfiles_lsp::{
    Position, Range, Resolution, ResourceIndex, Target, project_root, uid_to_id,
};

/// What the command line asked this executable to do.
enum Command {
    /// Read-only resource language server over stdio (the default).
    ResourceServer,
    /// Bidirectional byte bridge to a Godot GDScript TCP language server.
    GdscriptBridge(bridge::Options),
    Version,
    Help,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("gdfiles-lsp: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), server::DynError> {
    let command = match parse_args(env::args().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("gdfiles-lsp: {error}");
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    match command {
        Command::Version => {
            println!("gdfiles-lsp {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::GdscriptBridge(options) => bridge::run(options),
        Command::ResourceServer => {
            let (connection, _io_threads) = lsp_server::Connection::stdio();
            // Returning from `main` terminates the process even if a client
            // keeps its stdin pipe open, so `exit` never depends on
            // networking/thread shutdown.
            server::run(connection)
        }
    }
}

const USAGE: &str = "usage: gdfiles-lsp [--stdio] | --gdscript-lsp [--host HOST] [--port PORT] [--connect-timeout-ms MILLISECONDS] [--version]";

fn parse_args(arguments: impl IntoIterator<Item = String>) -> Result<Command, String> {
    let mut arguments = arguments.into_iter();
    let mut stdio = false;
    let mut gdscript = false;
    let mut host = None;
    let mut port = None;
    let mut connect_timeout = None;

    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            // stdio is also the default transport.
            "--stdio" => stdio = true,
            "--gdscript-lsp" => gdscript = true,
            "--version" | "-V" => return Ok(Command::Version),
            "--help" | "-h" => return Ok(Command::Help),
            "--host" => host = Some(parse_host(&take_value(&mut arguments, "--host")?)?),
            "--port" => port = Some(parse_port(&take_value(&mut arguments, "--port")?)?),
            "--connect-timeout-ms" => {
                connect_timeout = Some(parse_connect_timeout(&take_value(
                    &mut arguments,
                    "--connect-timeout-ms",
                )?)?);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    if stdio && gdscript {
        return Err("--stdio and --gdscript-lsp are mutually exclusive".to_string());
    }
    if !gdscript && (host.is_some() || port.is_some() || connect_timeout.is_some()) {
        return Err("--host, --port and --connect-timeout-ms require --gdscript-lsp".to_string());
    }
    if !gdscript {
        return Ok(Command::ResourceServer);
    }

    let defaults = bridge::Options::default();
    Ok(Command::GdscriptBridge(bridge::Options {
        host: host.unwrap_or(defaults.host),
        port: port.unwrap_or(defaults.port),
        connect_timeout: connect_timeout.unwrap_or(defaults.connect_timeout),
    }))
}

fn take_value(
    arguments: &mut impl Iterator<Item = String>,
    option: &str,
) -> Result<String, String> {
    arguments
        .next()
        .ok_or_else(|| format!("{option} requires a value"))
}

fn parse_host(value: &str) -> Result<String, String> {
    if value.is_empty() || value.chars().any(char::is_whitespace) {
        return Err(format!(
            "invalid host '{value}': expected a non-empty host name or IP address"
        ));
    }
    Ok(value.to_string())
}

fn parse_port(value: &str) -> Result<u16, String> {
    let port: u16 = value
        .parse()
        .map_err(|_| format!("invalid port '{value}': expected 1-65535"))?;
    if port == 0 {
        return Err("invalid port '0': expected 1-65535".to_string());
    }
    Ok(port)
}

fn parse_connect_timeout(value: &str) -> Result<Duration, String> {
    let millis: u64 = value.parse().map_err(|_| {
        format!("invalid connect timeout '{value}': expected a positive number of milliseconds")
    })?;
    if millis == 0 {
        return Err(
            "invalid connect timeout '0': expected a positive number of milliseconds".to_string(),
        );
    }
    Ok(Duration::from_millis(millis))
}

fn print_help() {
    println!(
        "gdfiles-lsp {}\n\
         Read-only Godot resource navigation and hover over stdio JSON-RPC,\n\
         plus a raw TCP bridge for Godot's GDScript language server.\n\n\
         {USAGE}\n\n\
         Options:\n\
           --stdio                    Use standard input/output (default)\n\
           --gdscript-lsp             Bridge stdio to a GDScript TCP server\n\
           --host HOST                GDScript bridge host (default {})\n\
           --port PORT                GDScript bridge port (default {})\n\
           --connect-timeout-ms MS    GDScript bridge connect timeout (default {})\n\
           --version                  Print the server version\n\
           -h, --help                 Print this help",
        env!("CARGO_PKG_VERSION"),
        bridge::DEFAULT_HOST,
        bridge::DEFAULT_PORT,
        bridge::DEFAULT_CONNECT_TIMEOUT_MS
    );
}
