#[allow(dead_code)]
#[path = "../src/installer.rs"]
mod installer;

use installer::{
    CacheState, RELEASE_REPO, RELEASE_TAG, ReleaseTarget, explicit_binary_path, inspect_cache_at,
    standard_cargo_binary_candidates, validate_release_asset_url,
};
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;
use zed_extension_api::{Architecture, Os};

#[test]
fn maps_supported_platforms_to_contract_targets() {
    let cases = [
        (
            Os::Linux,
            Architecture::X8664,
            ReleaseTarget::LinuxX86_64,
            "x86_64-unknown-linux-gnu",
            "gdfiles-lsp-v0.1.0-x86_64-unknown-linux-gnu.tar.gz",
            "gdfiles-lsp",
        ),
        (
            Os::Linux,
            Architecture::Aarch64,
            ReleaseTarget::LinuxAarch64,
            "aarch64-unknown-linux-gnu",
            "gdfiles-lsp-v0.1.0-aarch64-unknown-linux-gnu.tar.gz",
            "gdfiles-lsp",
        ),
        (
            Os::Mac,
            Architecture::X8664,
            ReleaseTarget::MacX86_64,
            "x86_64-apple-darwin",
            "gdfiles-lsp-v0.1.0-x86_64-apple-darwin.tar.gz",
            "gdfiles-lsp",
        ),
        (
            Os::Mac,
            Architecture::Aarch64,
            ReleaseTarget::MacAarch64,
            "aarch64-apple-darwin",
            "gdfiles-lsp-v0.1.0-aarch64-apple-darwin.tar.gz",
            "gdfiles-lsp",
        ),
        (
            Os::Windows,
            Architecture::X8664,
            ReleaseTarget::WindowsX86_64,
            "x86_64-pc-windows-msvc",
            "gdfiles-lsp-v0.1.0-x86_64-pc-windows-msvc.zip",
            "gdfiles-lsp.exe",
        ),
    ];

    for (os, arch, expected_target, triple, archive, executable) in cases {
        let target = ReleaseTarget::from_platform(os, arch).expect("supported target");
        assert_eq!(target, expected_target);
        assert_eq!(target.triple(), triple);
        assert_eq!(target.archive_name(), archive);
        assert_eq!(target.executable_name(), executable);
    }
}

#[test]
fn rejects_unsupported_platforms_with_actionable_error() {
    for (os, arch) in [
        (Os::Linux, Architecture::X86),
        (Os::Mac, Architecture::X86),
        (Os::Windows, Architecture::Aarch64),
    ] {
        let error = ReleaseTarget::from_platform(os, arch).expect_err("unsupported target");
        assert!(error.contains("Unsupported platform"), "{error}");
        assert!(error.contains("lsp.gdfiles-lsp.binary.path"), "{error}");
        assert!(
            error.contains("cargo install --path crates/gdfiles-lsp"),
            "{error}"
        );
        assert!(!error.contains("--locked"), "{error}");
    }
}

#[test]
fn uses_exact_pinned_https_release_urls() {
    let target = ReleaseTarget::WindowsX86_64;
    let archive = target.archive_name();
    let expected =
        format!("https://github.com/{RELEASE_REPO}/releases/download/{RELEASE_TAG}/{archive}");

    assert!(validate_release_asset_url(&expected, target, &archive).is_ok());

    let wrong_tag =
        format!("https://github.com/{RELEASE_REPO}/releases/download/gdfiles-lsp-v9.9.9/{archive}");
    assert!(validate_release_asset_url(&wrong_tag, target, &archive).is_err());

    let http = expected.replacen("https://", "http://", 1);
    assert!(validate_release_asset_url(&http, target, &archive).is_err());

    assert!(validate_release_asset_url(&expected, target, "other-asset.zip").is_err());
}

#[test]
fn explicit_binary_path_wins_and_is_returned_unchanged() {
    let explicit = "C:\\tools\\gdfiles-lsp.exe".to_string();
    assert_eq!(
        explicit_binary_path(Some(explicit.clone())).unwrap(),
        Some(explicit)
    );
    assert_eq!(explicit_binary_path(None).unwrap(), None);
    assert!(explicit_binary_path(Some("   ".to_string())).is_err());
}

#[test]
fn accepts_an_existing_cached_executable_offline() {
    let temp = tempdir().unwrap();
    let target = ReleaseTarget::LinuxX86_64;
    let cache = temp.path().join("cache");
    fs::create_dir_all(&cache).unwrap();

    let executable = cache.join(target.executable_name());
    fs::write(&executable, b"previously downloaded binary").unwrap();

    assert_eq!(
        inspect_cache_at(&cache, target),
        CacheState::Present(executable)
    );
}

#[test]
fn absent_or_incomplete_cache_is_not_reused() {
    let temp = tempdir().unwrap();
    let target = ReleaseTarget::LinuxX86_64;

    assert_eq!(
        inspect_cache_at(&temp.path().join("missing"), target),
        CacheState::Absent
    );

    let empty_directory = temp.path().join("empty-directory");
    fs::create_dir_all(&empty_directory).unwrap();
    assert!(matches!(
        inspect_cache_at(&empty_directory, target),
        CacheState::Invalid(_)
    ));
}

#[test]
fn empty_cached_executable_is_invalid() {
    let temp = tempdir().unwrap();
    let target = ReleaseTarget::LinuxX86_64;
    let cache = temp.path().join("cache");
    fs::create_dir_all(&cache).unwrap();

    fs::write(cache.join(target.executable_name()), b"").unwrap();

    match inspect_cache_at(&cache, target) {
        CacheState::Invalid(reason) => assert!(reason.contains("is empty"), "{reason}"),
        other => panic!("expected invalid cache, got {other:?}"),
    }
}

#[test]
fn rejects_non_file_cache_entry() {
    let temp = tempdir().unwrap();
    let target = ReleaseTarget::WindowsX86_64;
    let cache = temp.path().join("cache");
    fs::create_dir_all(cache.join(target.executable_name())).unwrap();

    assert!(matches!(
        inspect_cache_at(&cache, target),
        CacheState::Invalid(_)
    ));
}

#[test]
fn derives_standard_cargo_paths_from_worktree_environment_only() {
    let environment = vec![
        ("HOME".to_string(), "/home/example".to_string()),
        ("CARGO_HOME".to_string(), "/opt/cargo".to_string()),
    ];
    let candidates = standard_cargo_binary_candidates(&environment, "gdfiles-lsp");
    assert_eq!(
        candidates,
        vec![
            "/opt/cargo/bin/gdfiles-lsp".to_string(),
            "/home/example/.cargo/bin/gdfiles-lsp".to_string(),
        ]
    );

    let home_only = vec![("HOME".to_string(), "/home/example".to_string())];
    assert_eq!(
        standard_cargo_binary_candidates(&home_only, "gdfiles-lsp"),
        vec!["/home/example/.cargo/bin/gdfiles-lsp".to_string()]
    );

    let duplicate = vec![
        ("HOME".to_string(), "/home/example".to_string()),
        ("CARGO_HOME".to_string(), "/home/example/.cargo".to_string()),
    ];
    assert_eq!(
        standard_cargo_binary_candidates(&duplicate, "gdfiles-lsp"),
        vec!["/home/example/.cargo/bin/gdfiles-lsp".to_string()]
    );

    let empty = vec![
        ("HOME".to_string(), String::new()),
        ("CARGO_HOME".to_string(), String::new()),
    ];
    assert!(standard_cargo_binary_candidates(&empty, "gdfiles-lsp").is_empty());

    let relative_cargo_home = vec![
        ("HOME".to_string(), "/home/example".to_string()),
        ("CARGO_HOME".to_string(), "relative/cargo".to_string()),
    ];
    assert_eq!(
        standard_cargo_binary_candidates(&relative_cargo_home, "gdfiles-lsp"),
        vec!["/home/example/.cargo/bin/gdfiles-lsp".to_string()]
    );

    let windows_paths = vec![
        (
            "CARGO_HOME".to_string(),
            "C:\\Users\\example\\.cargo".to_string(),
        ),
        ("USERPROFILE".to_string(), "C:\\Users\\example".to_string()),
    ];
    assert_eq!(
        standard_cargo_binary_candidates(&windows_paths, "gdfiles-lsp.exe"),
        vec!["C:\\Users\\example\\.cargo\\bin\\gdfiles-lsp.exe".to_string()]
    );
}

#[test]
fn unavailable_release_error_names_tag_target_asset_and_local_alternatives() {
    let here = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(here.join("crates/gdfiles-lsp/Cargo.toml").is_file());

    let error = installer::unavailable_error(
        ReleaseTarget::LinuxAarch64,
        "the release has no asset named gdfiles-lsp-v0.1.0-aarch64-unknown-linux-gnu.tar.gz",
    );
    assert!(error.contains(RELEASE_TAG), "{error}");
    assert!(error.contains("aarch64-unknown-linux-gnu"), "{error}");
    assert!(
        error.contains("gdfiles-lsp-v0.1.0-aarch64-unknown-linux-gnu.tar.gz"),
        "{error}"
    );
    assert!(error.contains("lsp.gdfiles-lsp.binary.path"), "{error}");
    assert!(
        error.contains("cargo install --path crates/gdfiles-lsp"),
        "{error}"
    );
    assert!(!error.contains("--locked"), "{error}");
}
