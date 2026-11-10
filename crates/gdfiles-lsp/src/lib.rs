//! Read-only Godot 4 resource index and resolver core.
//!
//! The crate intentionally keeps the transport (LSP framing, previews) out of
//! this module. The public surface below is the contract used by the protocol
//! layer.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

mod document;
mod index;
mod uid;

pub use index::{ResourceIndex, project_root};
pub use uid::uid_to_id;

/// LSP position, using UTF-16 code units for `character`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

/// LSP range, using UTF-16 code units for `character`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

/// A resolved resource location.
#[derive(Clone, Debug)]
pub struct Target {
    pub path: PathBuf,
    pub range: Range,
    pub resource_type: Option<String>,
}

/// Result of resolving the token under the cursor.
#[derive(Clone, Debug)]
pub struct Resolution {
    /// Source range of the token, in LSP UTF-16 coordinates.
    pub range: Range,
    /// Text of the token that was resolved.
    pub token: String,
    /// All existing, valid candidates. Duplicate owners are never collapsed
    /// into a silent single winner.
    pub targets: Vec<Target>,
    /// Missing/ambiguous/fallback explanation (plain text, not Markdown).
    pub message: Option<String>,
}
