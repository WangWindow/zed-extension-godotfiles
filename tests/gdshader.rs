#[allow(dead_code)]
#[path = "../src/gdscript.rs"]
mod gdscript;

#[allow(dead_code)]
#[path = "../src/gdshader.rs"]
mod gdshader;

use std::collections::HashMap;

use gdshader::{BINARY_NAME, DEFAULT_ARGUMENTS, build_command, is_server_id};
use zed_extension_api::Command;
use zed_extension_api::settings::CommandSettings;

fn binary(path: Option<&str>) -> CommandSettings {
    CommandSettings {
        path: path.map(str::to_string),
        arguments: None,
        env: None,
    }
}

#[track_caller]
fn error(command: Result<Command, String>) -> String {
    match command {
        Ok(_) => panic!("expected command resolution to fail"),
        Err(error) => error,
    }
}

#[test]
fn dispatch_accepts_only_the_gdshader_server_id() {
    assert!(is_server_id("gdshader-lsp"));

    for unrelated in [
        "gdshader",
        "gdfiles-lsp",
        "gdscript",
        "godot-resource",
        "godot-uid",
        "GDShader LSP",
        "",
    ] {
        assert!(
            !is_server_id(unrelated),
            "accepted unrelated id {unrelated:?}"
        );
    }

    // The GDScript bridge stays exactly as strict as before: adding GDShader
    // support must not smuggle a new kind into its dispatch.
    let error = gdscript::language_server_kind("gdshader-lsp")
        .expect_err("GDShader must not be a GDScript language server kind");
    assert!(error.contains("Unknown language server"), "{error}");
}

#[test]
fn explicit_binary_path_wins_unchanged_and_skips_path_lookup() {
    let command = build_command(Some(binary(Some("/opt/gdshader/bin/gdshader-lsp"))), |_| {
        panic!("PATH must not be consulted when binary.path is set")
    })
    .expect("explicit binary path");

    assert_eq!(command.command, "/opt/gdshader/bin/gdshader-lsp");
    assert_eq!(command.args, DEFAULT_ARGUMENTS);
    assert!(command.env.is_empty());

    let spaced = build_command(
        Some(binary(Some(" /opt/path with spaces/custom-server "))),
        |_| panic!("PATH must not be consulted when binary.path is set"),
    )
    .expect("valid path is kept exactly");

    assert_eq!(spaced.command, " /opt/path with spaces/custom-server ");
}

#[test]
fn path_lookup_prefers_the_hyphenated_binary_name() {
    let mut requested = Vec::new();
    let command = build_command(None, |name| {
        requested.push(name.to_string());
        Some("/opt/gdshader/bin/gdshader-lsp".to_string())
    })
    .expect("canonical binary on PATH");

    assert_eq!(requested, vec![BINARY_NAME.to_string()]);
    assert_eq!(BINARY_NAME, "gdshader-lsp");
    assert_eq!(command.command, "/opt/gdshader/bin/gdshader-lsp");
    assert_eq!(command.args, vec!["--stdio", "--log-path=none"]);
}

#[test]
fn missing_binary_reports_actionable_opt_in_guidance() {
    let mut attempts = Vec::new();
    let error = error(build_command(None, |name| {
        attempts.push(name.to_string());
        None
    }));

    assert_eq!(attempts, vec!["gdshader-lsp", "gdshader_lsp"]);
    for needle in [
        "Neither `gdshader-lsp` nor `gdshader_lsp` was found on PATH",
        "Install gdshader-lsp-cpp as `gdshader-lsp`",
        "lsp.gdshader-lsp.binary.path",
        "gdfiles-lsp",
    ] {
        assert!(error.contains(needle), "expected {needle:?} in {error:?}");
    }
}

#[test]
fn path_lookup_accepts_the_underscored_binary_name() {
    let mut requested = Vec::new();
    let command = build_command(None, |name| {
        requested.push(name.to_string());
        (name == "gdshader_lsp").then(|| "/opt/bin/gdshader_lsp".to_string())
    })
    .expect("underscored binary on PATH");
    assert_eq!(requested, vec!["gdshader-lsp", "gdshader_lsp"]);
    assert_eq!(command.command, "/opt/bin/gdshader_lsp");
    assert_eq!(command.args, DEFAULT_ARGUMENTS);
}

#[test]
fn empty_explicit_path_is_rejected_before_path_lookup() {
    for path in ["", " ", "\t \n"] {
        let error = error(build_command(Some(binary(Some(path))), |_| {
            panic!("PATH must not be consulted for an invalid explicit path")
        }));
        for needle in [
            "lsp.gdshader-lsp.binary.path is empty",
            "`gdshader-lsp`",
            "gdfiles-lsp",
        ] {
            assert!(
                error.contains(needle),
                "expected {needle:?} in error for {path:?}: {error}"
            );
        }
    }
}

#[test]
fn default_arguments_suppress_logging_and_custom_arguments_replace_them() {
    let default = build_command(Some(binary(Some("/opt/gdshader-lsp"))), |_| {
        panic!("PATH must not be consulted")
    })
    .expect("default arguments");
    assert_eq!(default.args, vec!["--stdio", "--log-path=none"]);

    let custom = build_command(
        Some(CommandSettings {
            path: Some("/opt/gdshader-lsp".to_string()),
            arguments: Some(vec![
                "--stdio".to_string(),
                "--log-path=/tmp/gdshader-debug.log".to_string(),
            ]),
            env: None,
        }),
        |_| panic!("PATH must not be consulted"),
    )
    .expect("custom arguments");
    assert_eq!(
        custom.args,
        vec!["--stdio", "--log-path=/tmp/gdshader-debug.log"]
    );

    let empty = build_command(
        Some(CommandSettings {
            path: Some("/opt/gdshader-lsp".to_string()),
            arguments: Some(vec![]),
            env: None,
        }),
        |_| panic!("PATH must not be consulted"),
    )
    .expect("explicit empty arguments");
    assert!(
        empty.args.is_empty(),
        "an explicit empty argument list must not restore the defaults"
    );
}

#[test]
fn binary_environment_and_path_settings_are_preserved() {
    let env = HashMap::from([
        ("CUSTOM_ENV_A".to_string(), "alpha".to_string()),
        ("CUSTOM_ENV_B".to_string(), "beta".to_string()),
    ]);

    let command = build_command(
        Some(CommandSettings {
            path: None,
            arguments: None,
            env: Some(env),
        }),
        |name| {
            assert_eq!(name, BINARY_NAME);
            Some("/opt/gdshader-lsp".to_string())
        },
    )
    .expect("settings without an explicit path");

    assert_eq!(command.command, "/opt/gdshader-lsp");
    assert_eq!(command.args, vec!["--stdio", "--log-path=none"]);

    let mut env = command.env;
    env.sort();
    assert_eq!(
        env,
        vec![
            ("CUSTOM_ENV_A".to_string(), "alpha".to_string()),
            ("CUSTOM_ENV_B".to_string(), "beta".to_string()),
        ]
    );
}
