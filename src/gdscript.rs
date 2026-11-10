//! GDScript bridge configuration and the original Godot debug adapter.
//!
//! The GDScript language server is reached through the native
//! `gdfiles-lsp` executable in `--gdscript-lsp` mode. Proxy settings
//! live at `lsp.gdscript.settings.bridge` and are translated into explicit CLI
//! arguments, so no `nc`/`ncat` process is ever started by the extension.
//!
//! The Godot debug adapter is the original direct-TCP integration: it never
//! launches a debug adapter process and never changes the C#/netcoredbg path.

use std::net::{Ipv4Addr, Ipv6Addr};

use zed_extension_api::{
    self as zed, DebugAdapterBinary, StartDebuggingRequestArguments,
    StartDebuggingRequestArgumentsRequest, TcpArguments,
    serde_json::{self, Value},
};

pub(crate) const RESOURCE_SERVER_ID: &str = "gdfiles-lsp";
pub(crate) const GDSCRIPT_SERVER_ID: &str = "gdscript";
pub(crate) const GODOT_DAP_ADAPTER_ID: &str = "godot";

pub(crate) const DEFAULT_BRIDGE_HOST: &str = "127.0.0.1";
pub(crate) const DEFAULT_BRIDGE_PORT: u16 = 6005;
pub(crate) const DEFAULT_BRIDGE_CONNECT_TIMEOUT_MS: u64 = 5000;

pub(crate) const DEFAULT_DAP_HOST: Ipv4Addr = Ipv4Addr::new(127, 0, 0, 1);
pub(crate) const DEFAULT_DAP_PORT: u16 = 6006;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LanguageServerKind {
    Resource,
    Gdscript,
}

/// Resolves which native CLI mode a Zed language-server id should use. Only
/// the two declared ids are accepted, and GDScript never falls back to the
/// resource/stdio mode.
pub(crate) fn language_server_kind(id: &str) -> Result<LanguageServerKind, String> {
    match id {
        RESOURCE_SERVER_ID => Ok(LanguageServerKind::Resource),
        GDSCRIPT_SERVER_ID => Ok(LanguageServerKind::Gdscript),
        other => Err(format!("Unknown language server: {other}")),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BridgeConfig {
    pub(crate) host: String,
    pub(crate) port: u16,
    pub(crate) connect_timeout_ms: u64,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            host: DEFAULT_BRIDGE_HOST.to_string(),
            port: DEFAULT_BRIDGE_PORT,
            connect_timeout_ms: DEFAULT_BRIDGE_CONNECT_TIMEOUT_MS,
        }
    }
}

/// Parses `lsp.gdscript.settings`. An absent `bridge` object keeps the
/// documented defaults. Present but malformed fields are rejected instead of
/// being silently replaced by defaults or truncated to fit a port.
pub(crate) fn bridge_config(settings: Option<&Value>) -> Result<BridgeConfig, String> {
    let Some(settings) = settings else {
        return Ok(BridgeConfig::default());
    };
    if settings.is_null() {
        return Ok(BridgeConfig::default());
    }

    let settings = settings.as_object().ok_or_else(|| {
        "lsp.gdscript.settings must be an object with an optional `bridge` object.".to_string()
    })?;
    let Some(bridge) = settings.get("bridge") else {
        return Ok(BridgeConfig::default());
    };
    if bridge.is_null() {
        return Ok(BridgeConfig::default());
    }

    let bridge = bridge.as_object().ok_or_else(|| {
        "lsp.gdscript.settings.bridge must be an object with `host`, `port`, and \
         `connectTimeoutMs` fields."
            .to_string()
    })?;

    let host = match bridge.get("host") {
        None => DEFAULT_BRIDGE_HOST.to_string(),
        Some(Value::String(host)) => validate_bridge_host(host)?,
        Some(_) => {
            return Err(
                "lsp.gdscript.settings.bridge.host must be a non-empty hostname or IP address \
                 string."
                    .to_string(),
            );
        }
    };

    let port = match bridge.get("port") {
        None => DEFAULT_BRIDGE_PORT,
        Some(value) => parse_bridge_port(value)?,
    };

    let connect_timeout_ms = match bridge.get("connectTimeoutMs") {
        None => DEFAULT_BRIDGE_CONNECT_TIMEOUT_MS,
        Some(value) => parse_bridge_timeout(value)?,
    };

    Ok(BridgeConfig {
        host,
        port,
        connect_timeout_ms,
    })
}

fn validate_bridge_host(host: &str) -> Result<String, String> {
    let host = host.trim();
    if host.is_empty() {
        return Err("lsp.gdscript.settings.bridge.host must not be empty.".to_string());
    }
    if host.parse::<Ipv4Addr>().is_ok() || host.parse::<Ipv6Addr>().is_ok() || is_dns_name(host) {
        return Ok(host.to_string());
    }

    Err(format!(
        "lsp.gdscript.settings.bridge.host `{host}` is not a valid hostname or IP address."
    ))
}

fn is_dns_name(host: &str) -> bool {
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() || host.len() > 253 {
        return false;
    }
    host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

fn parse_bridge_port(value: &Value) -> Result<u16, String> {
    let port = value.as_u64().ok_or_else(|| {
        "lsp.gdscript.settings.bridge.port must be an integer between 1 and 65535.".to_string()
    })?;
    if port == 0 || port > u64::from(u16::MAX) {
        return Err(format!(
            "lsp.gdscript.settings.bridge.port `{port}` is outside the valid range 1..=65535."
        ));
    }
    Ok(port as u16)
}

fn parse_bridge_timeout(value: &Value) -> Result<u64, String> {
    let timeout = value.as_u64().ok_or_else(|| {
        "lsp.gdscript.settings.bridge.connectTimeoutMs must be a positive integer number of \
         milliseconds."
            .to_string()
    })?;
    if timeout == 0 {
        return Err(
            "lsp.gdscript.settings.bridge.connectTimeoutMs must be greater than 0.".to_string(),
        );
    }
    Ok(timeout)
}

/// Default bridge CLI arguments. A custom `binary.arguments` list replaces
/// this entire vector, matching standard Zed extension behavior.
pub(crate) fn bridge_arguments(config: &BridgeConfig) -> Vec<String> {
    vec![
        "--gdscript-lsp".to_string(),
        "--host".to_string(),
        config.host.clone(),
        "--port".to_string(),
        config.port.to_string(),
        "--connect-timeout-ms".to_string(),
        config.connect_timeout_ms.to_string(),
    ]
}

/// Resolves the launch arguments for the GDScript language server, preserving
/// user-provided `binary.arguments` as a full override of the bridge defaults.
pub(crate) fn language_server_arguments(
    configured_arguments: Option<Vec<String>>,
    settings: Option<&Value>,
) -> Result<Vec<String>, String> {
    if let Some(arguments) = configured_arguments {
        return Ok(arguments);
    }
    Ok(bridge_arguments(&bridge_config(settings)?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DapConnection {
    pub(crate) host: Ipv4Addr,
    pub(crate) port: u16,
}

/// Reads the Godot DAP endpoint from the adapter configuration. Values are
/// validated rather than truncated; only a missing field uses the default.
pub(crate) fn dap_connection(configuration: &Value) -> Result<DapConnection, String> {
    let host = match configuration.get("host") {
        None => DEFAULT_DAP_HOST,
        Some(Value::String(host)) => host.parse::<Ipv4Addr>().map_err(|_| {
            format!(
                "The Godot debug `host` must be an IPv4 address such as 127.0.0.1, got `{host}`."
            )
        })?,
        Some(_) => {
            return Err(
                "The Godot debug `host` must be an IPv4 address string such as 127.0.0.1."
                    .to_string(),
            );
        }
    };

    let port = match configuration.get("port") {
        None => DEFAULT_DAP_PORT,
        Some(value) => {
            let port = value.as_u64().ok_or_else(|| {
                "The Godot debug `port` must be an integer between 1 and 65535.".to_string()
            })?;
            if port == 0 || port > u64::from(u16::MAX) {
                return Err(format!(
                    "The Godot debug `port` `{port}` is outside the valid range 1..=65535."
                ));
            }
            port as u16
        }
    };

    Ok(DapConnection { host, port })
}

pub(crate) fn dap_request(
    configuration: &Value,
) -> Result<StartDebuggingRequestArgumentsRequest, String> {
    configuration
        .get("request")
        .and_then(|value| value.as_str())
        .ok_or_else(|| "`request` was not specified".to_string())
        .and_then(|request| match request {
            "launch" => Ok(StartDebuggingRequestArgumentsRequest::Launch),
            "attach" => Ok(StartDebuggingRequestArgumentsRequest::Attach),
            _ => Err("`request` must be either `launch` or `attach`".to_string()),
        })
}

/// Builds the direct-TCP Godot debug adapter description. The adapter has no
/// command: Zed connects to the Godot editor's DAP listener itself.
pub(crate) fn build_dap_binary(
    adapter_name: &str,
    definition: &zed::DebugTaskDefinition,
) -> Result<DebugAdapterBinary, String> {
    if adapter_name != GODOT_DAP_ADAPTER_ID {
        return Err(format!("Unknown debug adapter: {adapter_name}"));
    }

    let configuration = serde_json::from_str::<Value>(&definition.config)
        .map_err(|error| format!("Error parsing the Godot debug configuration: {error}"))?;
    let request = dap_request(&configuration)?;
    let connection = dap_connection(&configuration)?;

    Ok(DebugAdapterBinary {
        command: None,
        arguments: vec![],
        envs: Default::default(),
        cwd: None,
        connection: Some(TcpArguments {
            host: connection.host.to_bits(),
            port: connection.port,
            timeout: None,
        }),
        request_args: StartDebuggingRequestArguments {
            configuration: definition.config.clone(),
            request,
        },
    })
}

pub(crate) fn dap_request_kind(
    adapter_name: &str,
    configuration: Value,
) -> Result<StartDebuggingRequestArgumentsRequest, String> {
    if adapter_name != GODOT_DAP_ADAPTER_ID {
        return Err(format!("Unknown debug adapter: {adapter_name}"));
    }
    dap_request(&configuration)
}
