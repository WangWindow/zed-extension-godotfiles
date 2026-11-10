//! Resolves the native `gdfiles-lsp` executable for the extension.
//!
//! Resolution order:
//! 1. Explicit `lsp.gdfiles-lsp.binary.path`
//! 2. `gdfiles-lsp` on the worktree `$PATH`
//! 3. The standard Cargo bin directory from the worktree shell environment
//! 4. A versioned, platform-specific copy in the extension working dir
//! 5. The pinned GitHub release archive
//!
//! The release artifacts are intentionally pinned. This module never asks for
//! the latest release, never builds or installs with Cargo, and never scans
//! arbitrary filesystem locations.

use std::fs;
use std::path::{Path, PathBuf};
use zed_extension_api::{self as zed, Architecture, DownloadedFileType, Os, Worktree};

pub(crate) const BINARY_STEM: &str = "gdfiles-lsp";
pub(crate) const GDFILES_LSP_VERSION: &str = "0.1.0";
pub(crate) const RELEASE_TAG: &str = "gdfiles-lsp-v0.1.0";
pub(crate) const RELEASE_REPO: &str = "WangWindow/zed-extension-godotfiles";
pub(crate) const CACHE_ROOT: &str = "gdfiles-lsp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReleaseTarget {
    LinuxX86_64,
    LinuxAarch64,
    MacX86_64,
    MacAarch64,
    WindowsX86_64,
}

impl ReleaseTarget {
    pub(crate) fn from_platform(os: Os, arch: Architecture) -> Result<Self, String> {
        match (os, arch) {
            (Os::Linux, Architecture::X8664) => Ok(Self::LinuxX86_64),
            (Os::Linux, Architecture::Aarch64) => Ok(Self::LinuxAarch64),
            (Os::Mac, Architecture::X8664) => Ok(Self::MacX86_64),
            (Os::Mac, Architecture::Aarch64) => Ok(Self::MacAarch64),
            (Os::Windows, Architecture::X8664) => Ok(Self::WindowsX86_64),
            (os, arch) => Err(format!(
                "Unsupported platform {os:?}/{arch:?} for {BINARY_STEM} v{GDFILES_LSP_VERSION}. \
                 Supported release targets are x86_64-unknown-linux-gnu, \
                 aarch64-unknown-linux-gnu, x86_64-apple-darwin, aarch64-apple-darwin, and \
                 x86_64-pc-windows-msvc. Set lsp.gdfiles-lsp.binary.path to a locally built \
                 {BINARY_STEM} executable or install it with \
                 `cargo install --path crates/gdfiles-lsp`."
            )),
        }
    }

    pub(crate) fn triple(self) -> &'static str {
        match self {
            Self::LinuxX86_64 => "x86_64-unknown-linux-gnu",
            Self::LinuxAarch64 => "aarch64-unknown-linux-gnu",
            Self::MacX86_64 => "x86_64-apple-darwin",
            Self::MacAarch64 => "aarch64-apple-darwin",
            Self::WindowsX86_64 => "x86_64-pc-windows-msvc",
        }
    }

    pub(crate) fn executable_name(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "gdfiles-lsp.exe",
            _ => BINARY_STEM,
        }
    }

    fn is_zip(self) -> bool {
        matches!(self, Self::WindowsX86_64)
    }

    pub(crate) fn archive_name(self) -> String {
        let extension = if self.is_zip() { ".zip" } else { ".tar.gz" };
        format!(
            "{BINARY_STEM}-v{GDFILES_LSP_VERSION}-{}{extension}",
            self.triple()
        )
    }

    fn download_url(self, asset_name: &str) -> String {
        format!("https://github.com/{RELEASE_REPO}/releases/download/{RELEASE_TAG}/{asset_name}")
    }

    fn download_file_type(self) -> DownloadedFileType {
        if self.is_zip() {
            DownloadedFileType::Zip
        } else {
            DownloadedFileType::GzipTar
        }
    }
}

pub(crate) fn current_release_target() -> Result<ReleaseTarget, String> {
    let (os, arch) = zed::current_platform();
    ReleaseTarget::from_platform(os, arch)
}

/// Returns the caller's explicit path unchanged, or `None` when automatic
/// resolution should continue. Split out so source precedence is testable
/// without a live Zed worktree.
pub(crate) fn explicit_binary_path(
    explicit_path: Option<String>,
) -> Result<Option<String>, String> {
    match explicit_path {
        Some(path) if path.trim().is_empty() => {
            Err("lsp.gdfiles-lsp.binary.path is empty. Point it at a local \
             gdfiles-lsp executable or remove it to use automatic resolution."
                .to_string())
        }
        Some(path) => Ok(Some(path)),
        None => Ok(None),
    }
}

/// Resolves the command path while preserving the caller's explicit settings
/// as the highest-priority source.
pub(crate) fn resolve_binary_path(
    explicit_path: Option<String>,
    worktree: &Worktree,
) -> Result<String, String> {
    if let Some(path) = explicit_binary_path(explicit_path)? {
        return Ok(path);
    }

    if let Some(path) = worktree.which(BINARY_STEM) {
        return Ok(path);
    }

    let target = current_release_target()?;

    // The extension sandbox cannot inspect arbitrary host paths. The worktree
    // shell environment gives us the Cargo locations, and `which` performs the
    // actual host-side executable lookup for those exact candidates.
    let shell_env = worktree.shell_env();
    for candidate in standard_cargo_binary_candidates(&shell_env, target.executable_name()) {
        if let Some(path) = worktree.which(&candidate) {
            return Ok(path);
        }
    }

    let cache_dir = cache_dir(target);
    let invalid_cache = match inspect_cache_at(&cache_dir, target) {
        CacheState::Present(executable) => return Ok(absolute_path(&executable)),
        CacheState::Absent => None,
        CacheState::Invalid(reason) => {
            remove_managed_cache(target).map_err(|error| {
                format!(
                    "The cached {BINARY_STEM} v{GDFILES_LSP_VERSION} for {} is unusable ({reason}) \
                     and could not be removed: {error}",
                    target.triple()
                )
            })?;
            Some(reason)
        }
    };

    match download_and_install(target) {
        Ok(path) => Ok(path),
        Err(error) => match invalid_cache {
            Some(reason) => Err(format!(
                "{error} The unusable cached copy was discarded first: {reason}."
            )),
            None => Err(error),
        },
    }
}

/// Builds the Cargo bin paths from the worktree shell environment only. The
/// extension never reads the host process HOME/CARGO_HOME directly inside wasm.
pub(crate) fn standard_cargo_binary_candidates(
    shell_env: &[(String, String)],
    executable_name: &str,
) -> Vec<String> {
    let mut candidates = Vec::new();

    // Cargo permits a relative CARGO_HOME, but `Worktree::which` would resolve
    // it against the project instead of the shell's directory. Only accept
    // absolute host paths from the worktree environment.
    if let Some(cargo_home) = env_value(shell_env, "CARGO_HOME")
        .filter(|value| !value.is_empty())
        .filter(|value| is_absolute_host_path(value))
    {
        push_unique(
            &mut candidates,
            join_host_bin_path(cargo_home, executable_name),
        );
    }

    if let Some(home) = env_value(shell_env, "HOME")
        .filter(|value| !value.is_empty())
        .filter(|value| is_absolute_host_path(value))
    {
        push_unique(
            &mut candidates,
            join_host_bin_path(&join_host_path(home, ".cargo"), executable_name),
        );
    }

    // Windows shells commonly expose USERPROFILE instead of HOME.
    if let Some(profile) = env_value(shell_env, "USERPROFILE")
        .filter(|value| !value.is_empty())
        .filter(|value| is_absolute_host_path(value))
    {
        push_unique(
            &mut candidates,
            join_host_bin_path(&join_host_path(profile, ".cargo"), executable_name),
        );
    }

    candidates
}

fn env_value<'a>(shell_env: &'a [(String, String)], key: &str) -> Option<&'a str> {
    shell_env
        .iter()
        .rev()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.as_str())
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !values.iter().any(|existing| existing == &value) {
        values.push(value);
    }
}

/// A wasm component always uses WASI path semantics, even on a Windows host.
/// Recognize host Windows drive and UNC paths from the worktree environment
/// explicitly instead of relying on `Path::is_absolute`.
fn is_absolute_host_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    value.starts_with('/')
        || (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
        || value.starts_with("\\\\")
}

fn join_host_path(root: &str, child: &str) -> String {
    let root = root.trim_end_matches(['/', '\\']);
    let separator = host_separator(root);
    format!("{root}{separator}{child}")
}

fn join_host_bin_path(root: &str, executable_name: &str) -> String {
    let root = root.trim_end_matches(['/', '\\']);
    let separator = host_separator(root);
    format!("{root}{separator}bin{separator}{executable_name}")
}

fn host_separator(root: &str) -> char {
    if root.contains('\\') && !root.contains('/') {
        '\\'
    } else {
        '/'
    }
}

pub(crate) fn cache_dir(target: ReleaseTarget) -> PathBuf {
    Path::new(CACHE_ROOT)
        .join(format!("v{GDFILES_LSP_VERSION}"))
        .join(target.triple())
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CacheState {
    Absent,
    Present(PathBuf),
    Invalid(String),
}

/// Checks only the managed cache path. An existing extracted executable is
/// reusable offline; no network or metadata request is made for a cache hit.
pub(crate) fn inspect_cache_at(directory: &Path, target: ReleaseTarget) -> CacheState {
    let directory_metadata = match fs::symlink_metadata(directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return CacheState::Absent,
        Err(error) => {
            return CacheState::Invalid(format!(
                "could not inspect cache directory {}: {error}",
                directory.display()
            ));
        }
    };

    if !directory_metadata.file_type().is_dir() {
        return CacheState::Invalid(format!(
            "cache path {} is not a regular directory",
            directory.display()
        ));
    }

    let executable = directory.join(target.executable_name());
    match ensure_regular_file(&executable) {
        Ok(()) => CacheState::Present(executable),
        Err(error) => CacheState::Invalid(error),
    }
}

fn remove_managed_cache(target: ReleaseTarget) -> Result<(), String> {
    let directory = cache_dir(target);
    match fs::symlink_metadata(&directory) {
        Ok(_) => fs::remove_dir_all(&directory).map_err(|error| {
            format!(
                "failed to remove managed cache directory {}: {error}",
                directory.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "failed to inspect managed cache directory {}: {error}",
            directory.display()
        )),
    }
}

fn download_and_install(target: ReleaseTarget) -> Result<String, String> {
    remove_managed_cache(target)?;

    let directory = cache_dir(target);
    // `download_file` treats this as an extraction destination directory, but
    // only creates the extension work dir itself, so create it explicitly.
    fs::create_dir_all(&directory).map_err(|error| {
        format!(
            "failed to create managed cache directory {}: {error}",
            directory.display()
        )
    })?;

    match download_and_install_inner(target, &directory) {
        Ok(executable) => Ok(absolute_path(&executable)),
        Err(error) => {
            // Never leave a partial download behind as a future cache hit.
            let _ = remove_managed_cache(target);
            Err(error)
        }
    }
}

fn download_and_install_inner(target: ReleaseTarget, directory: &Path) -> Result<PathBuf, String> {
    let archive_name = target.archive_name();

    let release = zed::github_release_by_tag_name(RELEASE_REPO, RELEASE_TAG)
        .map_err(|error| unavailable_error(target, &format!("release lookup failed: {error}")))?;

    let archive_asset = release
        .assets
        .iter()
        .find(|asset| asset.name == archive_name)
        .ok_or_else(|| {
            unavailable_error(
                target,
                &format!(
                    "the release has no asset named {archive_name}; available assets: {}",
                    available_asset_names(&release)
                ),
            )
        })?;

    let archive_url =
        validate_release_asset_url(&archive_asset.download_url, target, &archive_name)?;

    zed::download_file(
        &archive_url,
        &directory.to_string_lossy(),
        target.download_file_type(),
    )
    .map_err(|error| {
        unavailable_error(
            target,
            &format!("downloading/extracting {archive_name} failed: {error}"),
        )
    })?;

    let executable = directory.join(target.executable_name());
    ensure_regular_file(&executable).map_err(|error| {
        format!(
            "The downloaded {BINARY_STEM} v{GDFILES_LSP_VERSION} archive for {} did not contain \
             the expected top-level executable {}: {error}. Required asset: {archive_name}. Set \
             lsp.gdfiles-lsp.binary.path to a local build or reinstall from \
             crates/gdfiles-lsp. The downloaded executable was not run.",
            target.triple(),
            target.executable_name()
        )
    })?;

    zed::make_file_executable(&executable.to_string_lossy()).map_err(|error| {
        format!(
            "The downloaded {BINARY_STEM} v{GDFILES_LSP_VERSION} executable for {} could not be \
             made executable: {error}. Set lsp.gdfiles-lsp.binary.path to a trusted local \
             build instead.",
            target.triple()
        )
    })?;

    Ok(executable)
}

fn available_asset_names(release: &zed::GithubRelease) -> String {
    if release.assets.is_empty() {
        return "<none>".to_string();
    }
    release
        .assets
        .iter()
        .map(|asset| asset.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn validate_release_asset_url(
    url: &str,
    target: ReleaseTarget,
    asset_name: &str,
) -> Result<String, String> {
    let expected = target.download_url(asset_name);
    if url != expected {
        return Err(unavailable_error(
            target,
            &format!("asset {asset_name} has URL {url}, expected exactly {expected}"),
        ));
    }
    if !expected.starts_with("https://github.com/") {
        return Err(unavailable_error(
            target,
            &format!("asset {asset_name} is not an HTTPS GitHub download URL: {expected}"),
        ));
    }
    Ok(expected)
}

fn ensure_regular_file(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "expected regular file {} is missing or unreadable: {error}",
            path.display()
        )
    })?;

    if !metadata.file_type().is_file() {
        return Err(format!(
            "{} is not a regular file (symlinks and directories are not accepted)",
            path.display()
        ));
    }

    if metadata.len() == 0 {
        return Err(format!("{} is empty", path.display()));
    }

    Ok(())
}

fn absolute_path(path: &Path) -> String {
    match std::env::current_dir() {
        Ok(directory) => directory.join(path).to_string_lossy().into_owned(),
        Err(_) => path.to_string_lossy().into_owned(),
    }
}

pub(crate) fn unavailable_error(target: ReleaseTarget, detail: &str) -> String {
    format!(
        "{BINARY_STEM} v{GDFILES_LSP_VERSION} is not available for target {} from GitHub \
         release tag {RELEASE_TAG} ({detail}). Required asset is {}. Set \
         lsp.gdfiles-lsp.binary.path to a local {BINARY_STEM} executable, or install it from \
         crates/gdfiles-lsp with `cargo install --path crates/gdfiles-lsp`. No release \
         was built or executed.",
        target.triple(),
        target.archive_name()
    )
}
