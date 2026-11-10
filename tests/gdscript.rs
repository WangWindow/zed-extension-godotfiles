#[allow(dead_code)]
#[path = "../src/gdscript.rs"]
mod gdscript;

use std::net::Ipv4Addr;

use gdscript::{
    BridgeConfig, DEFAULT_BRIDGE_CONNECT_TIMEOUT_MS, DEFAULT_BRIDGE_HOST, DEFAULT_BRIDGE_PORT,
    DEFAULT_DAP_HOST, DEFAULT_DAP_PORT, GODOT_DAP_ADAPTER_ID, LanguageServerKind, bridge_arguments,
    bridge_config, build_dap_binary, dap_connection, dap_request, dap_request_kind,
    language_server_arguments, language_server_kind,
};
use zed_extension_api::serde_json::{self, json};
use zed_extension_api::{DebugTaskDefinition, StartDebuggingRequestArgumentsRequest};

fn definition(config: serde_json::Value) -> DebugTaskDefinition {
    DebugTaskDefinition {
        label: "Godot".to_string(),
        adapter: GODOT_DAP_ADAPTER_ID.to_string(),
        config: config.to_string(),
        tcp_connection: None,
    }
}

#[test]
fn bridge_defaults_match_the_documented_endpoint() {
    let config = bridge_config(None).expect("defaults");
    assert_eq!(
        config,
        BridgeConfig {
            host: DEFAULT_BRIDGE_HOST.to_string(),
            port: DEFAULT_BRIDGE_PORT,
            connect_timeout_ms: DEFAULT_BRIDGE_CONNECT_TIMEOUT_MS,
        }
    );
    assert_eq!(
        bridge_arguments(&config),
        vec![
            "--gdscript-lsp",
            "--host",
            "127.0.0.1",
            "--port",
            "6005",
            "--connect-timeout-ms",
            "5000",
        ]
    );
}

#[test]
fn bridge_reads_host_port_and_timeout_from_settings() {
    let settings = json!({
        "bridge": {
            "host": "localhost",
            "port": 6007,
            "connectTimeoutMs": 1500
        }
    });
    let config = bridge_config(Some(&settings)).expect("valid bridge settings");
    assert_eq!(config.host, "localhost");
    assert_eq!(config.port, 6007);
    assert_eq!(config.connect_timeout_ms, 1500);

    assert_eq!(
        language_server_arguments(None, Some(&settings)).expect("bridge arguments"),
        vec![
            "--gdscript-lsp",
            "--host",
            "localhost",
            "--port",
            "6007",
            "--connect-timeout-ms",
            "1500",
        ]
    );
}

#[test]
fn absent_bridge_object_keeps_defaults() {
    for settings in [
        json!({}),
        json!({"unrelated": true}),
        json!({"bridge": null}),
        json!(null),
    ] {
        assert_eq!(
            bridge_config(Some(&settings)).expect("defaults"),
            BridgeConfig::default(),
            "settings: {settings}"
        );
    }
}

#[test]
fn custom_binary_arguments_replace_bridge_defaults() {
    let custom = vec!["--gdscript-lsp".to_string(), "--custom".to_string()];
    let settings = json!({"bridge": {"host": "localhost", "port": 6007}});
    assert_eq!(
        language_server_arguments(Some(custom.clone()), Some(&settings)).expect("custom args"),
        custom
    );
}

#[test]
fn rejects_malformed_bridge_settings() {
    for (settings, needle) in [
        (json!({"bridge": "127.0.0.1"}), "must be an object"),
        (json!({"bridge": {"host": ""}}), "must not be empty"),
        (json!({"bridge": {"host": "   "}}), "must not be empty"),
        (json!({"bridge": {"host": 42}}), "host must be"),
        (
            json!({"bridge": {"host": "http://localhost"}}),
            "not a valid hostname",
        ),
        (
            json!({"bridge": {"host": "127.0.0.1:6005"}}),
            "not a valid hostname",
        ),
        (
            json!({"bridge": {"host": "bad host"}}),
            "not a valid hostname",
        ),
        (
            json!({"bridge": {"port": 0}}),
            "outside the valid range 1..=65535",
        ),
        (
            json!({"bridge": {"port": 65536}}),
            "outside the valid range 1..=65535",
        ),
        (json!({"bridge": {"port": 1.5}}), "must be an integer"),
        (json!({"bridge": {"port": "6005"}}), "must be an integer"),
        (json!({"bridge": {"port": -1}}), "must be an integer"),
        (
            json!({"bridge": {"connectTimeoutMs": 0}}),
            "must be greater than 0",
        ),
        (
            json!({"bridge": {"connectTimeoutMs": "5000"}}),
            "must be a positive integer",
        ),
        (
            json!({"bridge": {"connectTimeoutMs": 1.5}}),
            "must be a positive integer",
        ),
    ] {
        let error = bridge_config(Some(&settings)).expect_err("malformed bridge settings");
        assert!(
            error.contains(needle),
            "expected {needle:?} in {error:?} for {settings}"
        );
    }
}

#[test]
fn dispatch_accepts_only_the_declared_gdfiles_lsp_server_id() {
    assert_eq!(
        language_server_kind("gdfiles-lsp").expect("resource server"),
        LanguageServerKind::Resource
    );
    assert_eq!(
        language_server_kind("gdscript").expect("gdscript server"),
        LanguageServerKind::Gdscript
    );
    // Registration is exact: language ids and near-miss server ids are not
    // accepted as aliases, so a stale configuration fails loudly.
    for unrelated in [
        "gdshader",
        "godot-resource",
        "godot-uid",
        "csharp",
        "godot",
        "",
    ] {
        let error = language_server_kind(unrelated).expect_err("unrelated server id");
        assert!(error.contains("Unknown language server"), "{error}");
    }
}

#[test]
fn dap_defaults_to_loopback_6006_and_extracts_launch_or_attach() {
    let launch = json!({"request": "launch"});
    assert_eq!(
        dap_connection(&launch).expect("default connection"),
        gdscript::DapConnection {
            host: DEFAULT_DAP_HOST,
            port: DEFAULT_DAP_PORT,
        }
    );
    assert_eq!(
        dap_request(&launch).expect("request"),
        StartDebuggingRequestArgumentsRequest::Launch
    );
    assert_eq!(
        dap_request(&json!({"request": "attach"})).expect("request"),
        StartDebuggingRequestArgumentsRequest::Attach
    );

    for config in [
        json!({}),
        json!({"request": "restart"}),
        json!({"request": 1}),
    ] {
        assert!(dap_request(&config).is_err(), "config: {config}");
    }
}

#[test]
fn dap_reads_explicit_endpoint_and_rejects_bad_values() {
    assert_eq!(
        dap_connection(&json!({"host": "192.0.2.7", "port": 7001})).expect("connection"),
        gdscript::DapConnection {
            host: Ipv4Addr::new(192, 0, 2, 7),
            port: 7001,
        }
    );

    for (config, needle) in [
        (json!({"host": "localhost"}), "must be an IPv4 address"),
        (json!({"host": "999.1.1.1"}), "must be an IPv4 address"),
        (json!({"host": 127}), "must be an IPv4 address string"),
        (json!({"port": 0}), "outside the valid range"),
        (json!({"port": 70000}), "outside the valid range"),
        (json!({"port": 1.5}), "must be an integer"),
        (json!({"port": "6006"}), "must be an integer"),
        (json!({"port": -1}), "must be an integer"),
    ] {
        let error = dap_connection(&config).expect_err("invalid DAP endpoint");
        assert!(
            error.contains(needle),
            "expected {needle:?} in {error:?} for {config}"
        );
    }
}

#[test]
fn dap_binary_is_direct_tcp_and_preserves_the_configuration() {
    let config = json!({
        "request": "attach",
        "scene": "main",
        "project": "/tmp/project",
        "platform": "host",
        "device": 3,
        "playArgs": ["--headless"],
        "host": "127.0.0.1",
        "port": 6006
    });
    let task = definition(config.clone());
    let binary = build_dap_binary(GODOT_DAP_ADAPTER_ID, &task).expect("Godot DAP binary");

    assert!(binary.command.is_none());
    assert!(binary.arguments.is_empty());
    let connection = binary.connection.expect("direct TCP connection");
    assert_eq!(connection.host, u32::from(Ipv4Addr::new(127, 0, 0, 1)));
    assert_eq!(connection.port, 6006);
    assert_eq!(connection.timeout, None);
    assert_eq!(
        binary.request_args.request,
        StartDebuggingRequestArgumentsRequest::Attach
    );
    assert_eq!(binary.request_args.configuration, config.to_string());
}

#[test]
fn dap_rejects_other_adapters_and_malformed_config_json() {
    let error = build_dap_binary("netcoredbg", &definition(json!({"request": "launch"})))
        .expect_err("C# adapter stays untouched");
    assert!(error.contains("Unknown debug adapter"), "{error}");
    assert!(dap_request_kind("godot", json!({"request": "launch"})).is_ok());
    assert!(dap_request_kind("csharp", json!({"request": "launch"})).is_err());

    let task = DebugTaskDefinition {
        label: "Godot".to_string(),
        adapter: GODOT_DAP_ADAPTER_ID.to_string(),
        config: "{not json".to_string(),
        tcp_connection: None,
    };
    let error = build_dap_binary(GODOT_DAP_ADAPTER_ID, &task).expect_err("bad JSON");
    assert!(error.contains("Error parsing"), "{error}");
}

#[test]
fn godot_schema_keeps_the_original_godot_fields() {
    let schema = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("debug_adapter_schemas/godot.json"),
    )
    .expect("Godot debug adapter schema");
    let schema: serde_json::Value = serde_json::from_str(&schema).expect("valid JSON schema");

    let properties = schema["properties"]
        .as_object()
        .expect("schema properties object");
    for field in [
        "request", "project", "platform", "device", "host", "port", "playArgs", "scene",
    ] {
        assert!(
            properties.contains_key(field),
            "missing Godot schema field {field}"
        );
    }
    assert_eq!(schema["required"], json!(["request"]));
}
