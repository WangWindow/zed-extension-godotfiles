//! Per-project, read-only resource metadata index.
//!
//! Owner metadata is reconstructed from the same persisted sources Godot uses
//! when it scans a project: `.tscn`/`.tres` headers, `.import` `[remap] uid`,
//! and `.uid` sidecars. `.godot/uid_cache.bin` is an optional, bounded fallback
//! and never overrides a live file-derived owner.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use walkdir::{DirEntry, WalkDir};

use crate::document::{ParsedDocument, Token, TokenKind};
use crate::uid::uid_to_id;
use crate::{Position, Range, Resolution, Target};

const MAX_METADATA_FILES: usize = 50_000;
const MAX_HEADER_BYTES: u64 = 256 * 1024;
const MAX_CACHE_BYTES: u64 = 16 * 1024 * 1024;
const EXCLUDED_DIRS: &[&str] = &[".git", ".godot", "bin", "obj", "node_modules"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Header,
    Import,
    Sidecar,
}

const CACHE_ENTRY_MIN_BYTES: usize = 12;

#[derive(Clone, Debug)]
struct Candidate {
    uid: u64,
    resource_type: Option<String>,
}

#[derive(Clone, Debug, Default)]
struct PathEntry {
    header: Option<Candidate>,
    import: Option<Candidate>,
    sidecar: Option<Candidate>,
}

impl PathEntry {
    fn effective(&self) -> Option<(&Candidate, Source)> {
        if let Some(candidate) = &self.header {
            Some((candidate, Source::Header))
        } else if let Some(candidate) = &self.import {
            Some((candidate, Source::Import))
        } else if let Some(candidate) = &self.sidecar {
            Some((candidate, Source::Sidecar))
        } else {
            None
        }
    }

    fn set(&mut self, source: Source, candidate: Option<Candidate>) {
        match source {
            Source::Header => self.header = candidate,
            Source::Import => self.import = candidate,
            Source::Sidecar => self.sidecar = candidate,
        }
    }
}

/// Project-wide resource metadata index.
pub struct ResourceIndex {
    root: PathBuf,
    by_path: BTreeMap<PathBuf, PathEntry>,
    by_uid: BTreeMap<u64, BTreeSet<PathBuf>>,
    cache: BTreeMap<u64, BTreeSet<PathBuf>>,
    cache_path: Option<PathBuf>,
}

impl ResourceIndex {
    /// Create an empty index. No filesystem IO happens until [`refresh`] or
    /// [`update`] is called.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            by_path: BTreeMap::new(),
            by_uid: BTreeMap::new(),
            cache: BTreeMap::new(),
            cache_path: None,
        }
    }

    /// Project root this index was created for.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Rebuild the index with a bounded metadata scan.
    pub fn refresh(&mut self) -> io::Result<()> {
        self.by_path.clear();
        self.by_uid.clear();
        self.cache.clear();
        self.cache_path = None;

        if !self.root.is_dir() {
            return Ok(());
        }

        let walker = WalkDir::new(&self.root)
            .follow_links(false)
            .into_iter()
            .filter_entry(excluded_entry);
        let mut relevant = 0usize;
        for entry in walker {
            let entry = match entry {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if entry.depth() == 0 {
                continue;
            }
            let file_type = entry.file_type();
            if file_type.is_symlink() || !file_type.is_file() {
                continue;
            }
            let kind = match metadata_kind(entry.path()) {
                Some(kind) => kind,
                None => continue,
            };
            relevant += 1;
            if relevant > MAX_METADATA_FILES {
                break;
            }
            self.index_file(entry.path(), kind, None);
        }

        self.reload_cache();
        Ok(())
    }

    /// Apply an incremental watch/overlay update.
    ///
    /// `text = Some(...)` overlays an unsaved owner document. `None` rereads
    /// the file from disk, or removes the path's metadata when it is gone.
    pub fn update(&mut self, path: &Path, text: Option<&str>) -> io::Result<()> {
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();

        if file_name == "uid_cache.bin" {
            if text.is_none() {
                self.reload_cache();
            } else {
                // Overlay text is not a useful representation of this binary
                // file; the on-disk cache remains the bounded fallback.
                self.reload_cache();
            }
            return Ok(());
        }
        if file_name == "project.godot" {
            self.reload_cache();
            return Ok(());
        }

        if !self.is_indexable_update_path(path) {
            return Ok(());
        }

        let kind = match metadata_kind(path) {
            Some(kind) => kind,
            None => return Ok(()),
        };
        self.index_file(path, kind, text);
        Ok(())
    }

    /// Resolve the token under `position` in `text`.
    ///
    /// Returns `None` when the cursor is not on a resolvable resource token.
    pub fn resolve(
        &self,
        document_path: &Path,
        text: &str,
        position: Position,
    ) -> Option<Resolution> {
        let document = ParsedDocument::parse(text);
        let token = document.token_at(position)?;
        let (targets, message) = match &token.kind {
            TokenKind::Path(value) => {
                self.resolve_path_token(document_path, &document, token, value)
            }
            TokenKind::ExtResource(id) => {
                self.resolve_ext_resource(document_path, &document, token, id)
            }
            TokenKind::SubResource(id) => self.resolve_sub_resource(document_path, &document, id),
        };
        Some(Resolution {
            range: token.range,
            token: token.text().to_string(),
            targets,
            message,
        })
    }

    fn index_file(&mut self, path: &Path, kind: MetadataKind, overlay: Option<&str>) {
        match kind {
            MetadataKind::Header => {
                let content = match overlay {
                    Some(text) => Some(text.to_string()),
                    None => read_text(path, MAX_HEADER_BYTES),
                };
                let candidate = content.and_then(|content| header_candidate(&content));
                self.set_candidate(path.to_path_buf(), Source::Header, candidate);
            }
            MetadataKind::Sidecar => {
                let content = match overlay {
                    Some(text) => Some(text.to_string()),
                    None => read_text(path, MAX_HEADER_BYTES),
                };
                let candidate = content.as_deref().and_then(sidecar_candidate);
                self.set_candidate(owner_of_sidecar(path), Source::Sidecar, candidate);
            }
            MetadataKind::Import => {
                let content = match overlay {
                    Some(text) => Some(text.to_string()),
                    None => read_text(path, MAX_HEADER_BYTES),
                };
                let candidate = content.and_then(|content| import_candidate(&content));
                self.set_candidate(owner_of_import(path), Source::Import, candidate);
            }
        }
    }

    fn set_candidate(&mut self, owner: PathBuf, source: Source, candidate: Option<Candidate>) {
        if !self.by_path.contains_key(&owner) && candidate.is_none() {
            return;
        }
        // Incremental watch updates must not grow the index without bound.
        if !self.by_path.contains_key(&owner) && self.by_path.len() >= MAX_METADATA_FILES {
            return;
        }

        self.remove_effective(&owner);

        let mut entry = self.by_path.remove(&owner).unwrap_or_default();
        entry.set(source, candidate);
        if entry.effective().is_none() {
            return;
        }
        self.by_path.insert(owner.clone(), entry);
        self.insert_effective(&owner);
    }

    fn remove_effective(&mut self, owner: &Path) {
        let uid = self
            .by_path
            .get(owner)
            .and_then(|entry| entry.effective().map(|(candidate, _)| candidate.uid));
        if let Some(uid) = uid
            && let Some(paths) = self.by_uid.get_mut(&uid)
        {
            paths.remove(owner);
            if paths.is_empty() {
                self.by_uid.remove(&uid);
            }
        }
    }

    fn insert_effective(&mut self, owner: &Path) {
        let uid = self
            .by_path
            .get(owner)
            .and_then(|entry| entry.effective().map(|(candidate, _)| candidate.uid));
        if let Some(uid) = uid {
            self.by_uid
                .entry(uid)
                .or_default()
                .insert(owner.to_path_buf());
        }
    }

    fn is_inside_root(&self, path: &Path) -> bool {
        let root = match fs::canonicalize(&self.root) {
            Ok(root) => root,
            Err(_) => return path.starts_with(&self.root),
        };
        match resolved_path(path) {
            Some(resolved) => resolved.starts_with(root),
            None => false,
        }
    }

    fn is_indexable_update_path(&self, path: &Path) -> bool {
        if !self.is_inside_root(path) {
            return false;
        }
        if self.is_in_excluded_dir(path) {
            return false;
        }
        !self.is_in_nested_project(path)
    }

    fn is_in_excluded_dir(&self, path: &Path) -> bool {
        // Prefer the canonical relative path so a symlink inside the project
        // cannot smuggle metadata in from an excluded directory.
        let relative = fs::canonicalize(&self.root)
            .ok()
            .and_then(|root| {
                resolved_path(path)
                    .and_then(|resolved| resolved.strip_prefix(&root).ok().map(Path::to_path_buf))
            })
            .or_else(|| path.strip_prefix(&self.root).ok().map(Path::to_path_buf));
        let Some(relative) = relative else {
            return false;
        };
        relative.components().any(|component| {
            component
                .as_os_str()
                .to_str()
                .is_some_and(|name| EXCLUDED_DIRS.contains(&name))
        })
    }

    fn is_in_nested_project(&self, path: &Path) -> bool {
        let Some(parent) = path.parent() else {
            return false;
        };
        for ancestor in parent.ancestors() {
            if ancestor == self.root {
                break;
            }
            if ancestor.join("project.godot").is_file() {
                return true;
            }
        }
        false
    }

    fn resolve_path_token(
        &self,
        document_path: &Path,
        document: &ParsedDocument<'_>,
        token: &Token,
        value: &str,
    ) -> (Vec<Target>, Option<String>) {
        // A path/uid string inside an `ext_resource` header has sibling
        // attributes that give us a useful fallback and a stale-path check.
        if let Some(section) = document.section_containing(token.start_byte)
            && section.kind == "ext_resource"
        {
            if let Some(path_attr) = section.attr("path")
                && path_attr.start_byte == token.start_byte
                && path_attr.end_byte == token.end_byte
                && let Some(uid) = section.value("uid")
                && uid_to_id(uid).is_some()
            {
                let (targets, uid_message) = self.resolve_uid_value(document_path, document, uid);
                if !targets.is_empty() {
                    let message = self.stale_path_message(
                        uid,
                        path_attr.value.as_str(),
                        &targets,
                        uid_message,
                    );
                    return (targets, message);
                }
                if let Some((path_targets, path_message)) =
                    self.resolve_res_value(document_path, document, path_attr.value.as_str())
                {
                    let fallback = join_messages(
                        Some(format!(
                            "UID {uid} was not found in the project index; using the ext_resource path"
                        )),
                        uid_message,
                    );
                    return (path_targets, join_messages(fallback, path_message));
                }
            }
            if let Some(uid_attr) = section.attr("uid")
                && uid_attr.start_byte == token.start_byte
                && uid_attr.end_byte == token.end_byte
            {
                let (targets, uid_message) =
                    self.resolve_uid_value(document_path, document, uid_attr.value.as_str());
                if !targets.is_empty() {
                    return (targets, uid_message);
                }
                if let Some(path) = section.value("path")
                    && let Some((path_targets, path_message)) =
                        self.resolve_res_value(document_path, document, path)
                {
                    let fallback = join_messages(
                        Some(format!(
                            "UID {} was not found in the project index; using the ext_resource path",
                            uid_attr.value
                        )),
                        uid_message,
                    );
                    return (path_targets, join_messages(fallback, path_message));
                }
                return (Vec::new(), uid_message);
            }
        }

        if let Some(rest) = value.strip_prefix("user://") {
            let _ = rest;
            return (
                Vec::new(),
                Some("user:// paths are not part of the project filesystem".to_string()),
            );
        }
        if value.starts_with("uid://") {
            return self.resolve_uid_value(document_path, document, value);
        }
        if value.starts_with("res://") {
            if let Some((targets, message)) = self.resolve_res_value(document_path, document, value)
            {
                return (targets, message);
            }
            return (
                Vec::new(),
                Some(format!("Resource {value} was not found in the project")),
            );
        }

        (
            Vec::new(),
            Some(format!("Unsupported resource reference {value}")),
        )
    }

    fn resolve_ext_resource(
        &self,
        document_path: &Path,
        document: &ParsedDocument<'_>,
        _token: &Token,
        id: &str,
    ) -> (Vec<Target>, Option<String>) {
        let sections = document.ext_sections(id);
        if sections.is_empty() {
            return (
                Vec::new(),
                Some(format!("ExtResource {id} is not declared in this document")),
            );
        }

        let mut targets = Vec::new();
        let mut messages = Vec::new();
        for section in sections {
            let declared_type = section.value("type").map(str::to_string);
            let path = section.value("path");
            let uid = section.value("uid");

            if let Some(uid) = uid {
                let (mut uid_targets, uid_message) =
                    self.resolve_uid_value(document_path, document, uid);
                if !uid_targets.is_empty() {
                    if let Some(declared_type) = declared_type.as_deref() {
                        apply_declared_type(&mut uid_targets, declared_type);
                    }
                    if let Some(path) = path {
                        if let Some(message) =
                            self.stale_path_message(uid, path, &uid_targets, uid_message.clone())
                        {
                            messages.push(message);
                        } else if let Some(message) = uid_message {
                            messages.push(message);
                        }
                    } else if let Some(message) = uid_message {
                        messages.push(message);
                    }
                    targets.extend(uid_targets);
                    continue;
                }

                if let Some(path) = path
                    && let Some((mut path_targets, path_message)) =
                        self.resolve_res_value(document_path, document, path)
                {
                    if let Some(declared_type) = declared_type.as_deref() {
                        apply_declared_type(&mut path_targets, declared_type);
                    }
                    messages.push(format!(
                        "UID {uid} was not found in the project index; using the literal ext_resource path"
                    ));
                    if let Some(message) = uid_message
                        && !message.starts_with("UID ")
                    {
                        messages.push(message);
                    }
                    if let Some(message) = path_message {
                        messages.push(message);
                    }
                    targets.extend(path_targets);
                    continue;
                }

                if let Some(message) = uid_message {
                    messages.push(message);
                }
                messages.push(format!("ExtResource {id} has no resolvable UID or path"));
                continue;
            }

            if let Some(path) = path {
                if let Some((mut path_targets, path_message)) =
                    self.resolve_res_value(document_path, document, path)
                {
                    if let Some(declared_type) = declared_type.as_deref() {
                        apply_declared_type(&mut path_targets, declared_type);
                    }
                    if let Some(message) = path_message {
                        messages.push(message);
                    }
                    targets.extend(path_targets);
                    continue;
                }
                messages.push(format!(
                    "ExtResource {id} path {path} was not found in the project"
                ));
            } else {
                messages.push(format!("ExtResource {id} has neither a path nor a UID"));
            }
        }

        dedupe_targets(&mut targets);
        (
            targets,
            if messages.is_empty() {
                None
            } else {
                Some(messages.join("; "))
            },
        )
    }

    fn resolve_sub_resource(
        &self,
        document_path: &Path,
        document: &ParsedDocument<'_>,
        id: &str,
    ) -> (Vec<Target>, Option<String>) {
        let sections = document.sub_sections(id);
        if sections.is_empty() {
            return (
                Vec::new(),
                Some(format!("SubResource {id} is not declared in this document")),
            );
        }
        let mut targets = Vec::new();
        for section in &sections {
            targets.push(Target {
                path: document_path.to_path_buf(),
                range: section.range,
                resource_type: section.value("type").map(str::to_string),
            });
        }
        let message = if targets.len() > 1 {
            Some(format!(
                "SubResource {id} is declared more than once in this document"
            ))
        } else {
            None
        };
        (targets, message)
    }

    fn resolve_res_value(
        &self,
        document_path: &Path,
        document: &ParsedDocument<'_>,
        value: &str,
    ) -> Option<(Vec<Target>, Option<String>)> {
        let rest = value.strip_prefix("res://")?;
        let (relative, sub_id) = split_sub_resource(rest);
        if relative.is_empty() {
            return Some((
                Vec::new(),
                Some(format!("{value} does not name a project file")),
            ));
        }

        let joined = self.root.join(relative);
        let safe_path = match self.safe_existing_file(&joined) {
            Some(path) => path,
            // The current document may be an unsaved overlay; its subresources
            // are still navigable even though the file is not on disk yet.
            None if joined == document_path => document_path.to_path_buf(),
            None => {
                return Some((
                    Vec::new(),
                    Some(format!("Resource {value} was not found in the project")),
                ));
            }
        };

        if let Some(sub_id) = sub_id {
            if same_file(&safe_path, document_path) {
                let sections = document.sub_sections(sub_id);
                if sections.is_empty() {
                    return Some((
                        Vec::new(),
                        Some(format!("SubResource {sub_id} was not found in {value}")),
                    ));
                }
                let mut targets = Vec::new();
                for section in &sections {
                    targets.push(Target {
                        path: document_path.to_path_buf(),
                        range: section.range,
                        resource_type: section.value("type").map(str::to_string),
                    });
                }
                let message = (targets.len() > 1).then(|| {
                    format!("SubResource {sub_id} is declared more than once in this document")
                });
                return Some((targets, message));
            }
            return Some((
                vec![Target {
                    path: safe_path.clone(),
                    range: zero_range(),
                    resource_type: self.resource_type_for(&safe_path),
                }],
                Some(format!(
                    "SubResource {sub_id} is in another file; opened {value} at the file start"
                )),
            ));
        }

        Some((
            vec![Target {
                path: safe_path.clone(),
                range: zero_range(),
                resource_type: self.resource_type_for(&safe_path),
            }],
            None,
        ))
    }

    fn resolve_uid_value(
        &self,
        document_path: &Path,
        document: &ParsedDocument<'_>,
        value: &str,
    ) -> (Vec<Target>, Option<String>) {
        let id = match uid_to_id(value) {
            Some(id) => id,
            None => {
                return (
                    Vec::new(),
                    Some(format!("{value} is not a valid Godot UID")),
                );
            }
        };

        let mut live: Vec<(PathBuf, Option<String>)> = Vec::new();
        let recorded = self.by_uid.get(&id).cloned().unwrap_or_default();
        for path in &recorded {
            if let Some(safe) = self.safe_existing_file(path) {
                live.push((safe, self.resource_type_for(path)));
            }
        }

        // A UID stored in an unsaved/open document is still a useful target
        // even before the protocol layer has refreshed the index.
        if self.is_inside_root(document_path)
            && let Some(header) = document.header()
            && header.value("uid").and_then(uid_to_id) == Some(id)
        {
            let path = document_path.to_path_buf();
            if !live
                .iter()
                .any(|(candidate, _)| same_file(candidate, &path))
            {
                live.push((path, self.resource_type_for(document_path)));
            }
        }

        // Opening a `.uid`/`.cs.uid` sidecar directly should navigate to the
        // owner resource, not to the sidecar itself.
        if let Some(owner) = uid_sidecar_owner(document_path, document)
            && self.is_inside_root(&owner)
            && !live
                .iter()
                .any(|(candidate, _)| same_file(candidate, &owner))
            && self.safe_existing_file(&owner).is_some()
        {
            live.push((owner, None));
        }

        dedupe_live(&mut live);
        if live.len() == 1 {
            let (path, resource_type) = live.pop().unwrap();
            return (
                vec![Target {
                    path,
                    range: zero_range(),
                    resource_type,
                }],
                None,
            );
        }
        if live.len() > 1 {
            let mut targets: Vec<Target> = live
                .into_iter()
                .map(|(path, resource_type)| Target {
                    path,
                    range: zero_range(),
                    resource_type,
                })
                .collect();
            targets.sort_by(|a, b| a.path.cmp(&b.path));
            let names = targets
                .iter()
                .map(|target| self.display_path(&target.path))
                .collect::<Vec<_>>()
                .join(", ");
            let count = targets.len();
            return (
                targets,
                Some(format!(
                    "UID {value} is ambiguous: {count} project files own it ({names})"
                )),
            );
        }

        let cached = self.cache.get(&id).cloned().unwrap_or_default();
        let mut cache_live = Vec::new();
        for path in cached {
            if let Some(safe) = self.safe_existing_file(&path) {
                cache_live.push((safe, self.resource_type_for(&path)));
            }
        }
        dedupe_live(&mut cache_live);
        if cache_live.len() == 1 {
            let (path, resource_type) = cache_live.pop().unwrap();
            return (
                vec![Target {
                    path: path.clone(),
                    range: zero_range(),
                    resource_type,
                }],
                Some(format!(
                    "UID {value} has no live file-derived owner; resolved from the Godot UID cache to {}",
                    self.display_path(&path)
                )),
            );
        }
        if cache_live.len() > 1 {
            let mut targets: Vec<Target> = cache_live
                .into_iter()
                .map(|(path, resource_type)| Target {
                    path,
                    range: zero_range(),
                    resource_type,
                })
                .collect();
            targets.sort_by(|a, b| a.path.cmp(&b.path));
            let names = targets
                .iter()
                .map(|target| self.display_path(&target.path))
                .collect::<Vec<_>>()
                .join(", ");
            return (
                targets,
                Some(format!(
                    "UID {value} is ambiguous in the Godot UID cache: {names}"
                )),
            );
        }

        let message = if recorded.is_empty() || recorded.iter().all(|path| !path.exists()) {
            format!("UID {value} was not found in the project index")
        } else {
            format!("UID {value} is indexed, but its owner file is missing")
        };
        (Vec::new(), Some(message))
    }

    fn stale_path_message(
        &self,
        uid: &str,
        literal_path: &str,
        targets: &[Target],
        uid_message: Option<String>,
    ) -> Option<String> {
        let literal = literal_path
            .strip_prefix("res://")
            .map(|relative| self.root.join(split_sub_resource(relative).0))
            .and_then(|path| self.safe_existing_file(&path));

        let message = match literal {
            Some(literal) => {
                if targets
                    .iter()
                    .any(|target| same_file(&target.path, &literal))
                {
                    None
                } else {
                    Some(format!(
                        "UID {uid} resolves to {}; the literal ext_resource path {literal_path} is stale and was ignored",
                        self.display_path(&targets[0].path)
                    ))
                }
            }
            None => Some(format!(
                "the literal ext_resource path {literal_path} is missing; using the UID owner {}",
                self.display_path(&targets[0].path)
            )),
        };

        if message.is_some() {
            join_messages(message, uid_message)
        } else {
            uid_message
        }
    }

    fn resource_type_for(&self, path: &Path) -> Option<String> {
        self.by_path
            .get(path)
            .and_then(|entry| {
                entry
                    .effective()
                    .map(|(candidate, _)| candidate.resource_type.clone())
            })
            .flatten()
    }

    fn safe_existing_file(&self, path: &Path) -> Option<PathBuf> {
        let metadata = fs::metadata(path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let canonical = fs::canonicalize(path).ok()?;
        let root = fs::canonicalize(&self.root).ok()?;
        if !canonical.starts_with(&root) {
            return None;
        }
        Some(canonical)
    }

    fn display_path(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root) {
            Ok(relative) => format!("res://{}", relative.to_string_lossy().replace('\\', "/")),
            Err(_) => path.display().to_string(),
        }
    }

    fn reload_cache(&mut self) {
        self.cache.clear();
        self.cache_path = None;
        for path in self.cache_candidates() {
            // Resolve through symlinks before opening: a `.godot` directory or
            // `uid_cache.bin` file that points outside the project must not be
            // read at all.
            let Some(cache_path) = self.safe_existing_file(&path) else {
                continue;
            };
            // Oversized/truncated/malformed caches are treated as absent. A
            // partial cache could silently substitute a stale target.
            if let Some(bytes) = read_complete_bounded(&cache_path, MAX_CACHE_BYTES) {
                self.cache = self.parse_cache(&bytes).unwrap_or_default();
                self.cache_path = Some(cache_path);
            }
            break;
        }
    }

    fn cache_candidates(&self) -> Vec<PathBuf> {
        let project = self
            .safe_existing_file(&self.root.join("project.godot"))
            .and_then(|path| read_text(&path, 64 * 1024))
            .unwrap_or_default();
        let config_version = config_value(&project, "config_version")
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(5);
        let hidden = config_value(&project, "use_hidden_project_data_directory")
            .map(|value| value != "false")
            .unwrap_or(true);
        let directory = if config_version >= 5 {
            if hidden { ".godot" } else { "godot" }
        } else if hidden {
            ".import"
        } else {
            "import"
        };
        let alternate = if directory.starts_with('.') {
            directory.trim_start_matches('.').to_string()
        } else {
            format!(".{directory}")
        };
        vec![
            self.root.join(directory).join("uid_cache.bin"),
            self.root.join(alternate).join("uid_cache.bin"),
        ]
    }

    fn parse_cache(&self, bytes: &[u8]) -> Option<BTreeMap<u64, BTreeSet<PathBuf>>> {
        let mut cache: BTreeMap<u64, BTreeSet<PathBuf>> = BTreeMap::new();
        let mut offset = 0usize;
        let count = read_u32(bytes, &mut offset)? as usize;
        if count > bytes.len() / CACHE_ENTRY_MIN_BYTES {
            return None;
        }
        for _ in 0..count {
            let id = read_u64(bytes, &mut offset)?;
            let length = read_u32(bytes, &mut offset)? as usize;
            if length > 8 * 1024 * 1024 || offset.saturating_add(length) > bytes.len() {
                return None;
            }
            let path_bytes = &bytes[offset..offset + length];
            offset += length;
            let path_text = std::str::from_utf8(path_bytes).ok()?;
            let path_text = path_text.trim_end_matches('\0');
            let path = if let Some(relative) = path_text.strip_prefix("res://") {
                self.root.join(relative)
            } else {
                PathBuf::from(path_text)
            };
            if path.as_os_str().is_empty() {
                return None;
            }
            cache
                .entry(id & 0x7fff_ffff_ffff_ffff)
                .or_default()
                .insert(path);
        }
        if offset != bytes.len() {
            return None;
        }
        Some(cache)
    }
}

/// Find the nearest ancestor of `document` that contains `project.godot`.
pub fn project_root(document: &Path) -> Option<PathBuf> {
    let start = if document.is_dir() {
        document
    } else {
        document.parent().unwrap_or(document)
    };
    for ancestor in start.ancestors() {
        if ancestor.join("project.godot").is_file() {
            return Some(ancestor.to_path_buf());
        }
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MetadataKind {
    Header,
    Import,
    Sidecar,
}

fn metadata_kind(path: &Path) -> Option<MetadataKind> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("tscn") | Some("tres") => Some(MetadataKind::Header),
        Some("import") => Some(MetadataKind::Import),
        Some("uid") => Some(MetadataKind::Sidecar),
        _ => None,
    }
}

fn owner_of_sidecar(path: &Path) -> PathBuf {
    path.with_extension("")
}

fn owner_of_import(path: &Path) -> PathBuf {
    path.with_extension("")
}

fn header_candidate(text: &str) -> Option<Candidate> {
    let document = ParsedDocument::parse(text);
    let header = document.header()?;
    let uid = header.value("uid").and_then(uid_to_id)?;
    let resource_type = if header.kind == "gd_scene" {
        Some("PackedScene".to_string())
    } else {
        header.value("type").map(str::to_string)
    };
    Some(Candidate { uid, resource_type })
}

fn sidecar_candidate(text: &str) -> Option<Candidate> {
    let line = text
        .trim_start_matches('\u{feff}')
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let line = line.split(';').next().unwrap_or(line).trim();
    let uid = uid_to_id(line)?;
    Some(Candidate {
        uid,
        resource_type: None,
    })
}

fn import_candidate(text: &str) -> Option<Candidate> {
    // `.import` stores UID/type as properties under `[remap]`, not as header
    // attributes:
    //
    //     [remap]
    //     importer="texture"
    //     type="CompressedTexture2D"
    //     uid="uid://..."
    let mut values = section_body_values(text, "remap");
    // Be liberal about fixtures/older writers that put the same values on the
    // `[remap ...]` header instead of in the body.
    let document = ParsedDocument::parse(text);
    if let Some(remap) = document
        .sections
        .iter()
        .find(|section| section.kind == "remap")
    {
        for key in ["uid", "type"] {
            if !values.contains_key(key)
                && let Some(value) = remap.value(key)
            {
                values.insert(key.to_string(), value.to_string());
            }
        }
    }
    let uid = values.get("uid").and_then(|uid| uid_to_id(uid))?;
    let resource_type = values.get("type").cloned();
    Some(Candidate { uid, resource_type })
}

fn section_body_values(text: &str, section_name: &str) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    let mut in_section = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('[') {
            let name = rest
                .split(|ch: char| ch.is_whitespace() || ch == ']')
                .next()
                .unwrap_or_default();
            in_section = name == section_name;
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once('=') {
            values.insert(key.trim().to_string(), unquote(value.trim()));
        }
    }
    values
}

fn unquote(value: &str) -> String {
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

fn excluded_entry(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    if !entry.file_type().is_dir() {
        return true;
    }
    // Descendant Godot projects are independent roots; indexing them into
    // this project would create duplicate owners that do not exist here.
    if entry.path().join("project.godot").is_file() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    !EXCLUDED_DIRS.iter().any(|excluded| *excluded == name)
}

fn read_text(path: &Path, limit: u64) -> Option<String> {
    let bytes = read_bytes(path, limit)?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_bytes(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let file = fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// Read a whole metadata file, rejecting files that are too large or change
/// size while being read. Used for the binary UID cache, where a partial read
/// would produce a plausible but wrong mapping.
fn read_complete_bounded(path: &Path, limit: u64) -> Option<Vec<u8>> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > limit {
        return None;
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fs::File::open(path).ok()?.read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 != metadata.len() {
        return None;
    }
    Some(bytes)
}

/// Resolve a path through the filesystem where possible, or through its
/// nearest existing ancestor plus the remaining lexical components. This lets
/// containment checks reject symlinked metadata without requiring the final
/// file to exist (important for delete notifications).
fn resolved_path(path: &Path) -> Option<PathBuf> {
    let mut probe = path;
    let mut suffix: Vec<OsString> = Vec::new();
    loop {
        if probe.exists()
            && let Ok(mut resolved) = fs::canonicalize(probe)
        {
            for part in suffix.iter().rev() {
                if part.as_os_str() == OsStr::new("..") {
                    resolved.pop();
                } else if part.as_os_str() != OsStr::new(".") {
                    resolved.push(part);
                }
            }
            return Some(resolved);
        }
        match probe.file_name() {
            Some(name) => suffix.push(name.to_os_string()),
            None => {
                if probe.ends_with("..") {
                    suffix.push(OsString::from(".."));
                } else if probe.ends_with(".") {
                    suffix.push(OsString::from("."));
                } else {
                    return None;
                }
            }
        }
        let parent = probe.parent()?;
        if parent == probe {
            return None;
        }
        probe = parent;
    }
}

fn read_u32(bytes: &[u8], offset: &mut usize) -> Option<u32> {
    let chunk = bytes.get(*offset..*offset + 4)?;
    *offset += 4;
    Some(u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
}

fn read_u64(bytes: &[u8], offset: &mut usize) -> Option<u64> {
    let chunk = bytes.get(*offset..*offset + 8)?;
    *offset += 8;
    Some(u64::from_le_bytes([
        chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
    ]))
}

fn config_value(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim_start();
        if line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        let Some(index) = line.find(key) else {
            continue;
        };
        let rest = &line[index + key.len()..];
        let rest = rest.trim_start();
        if let Some(value) = rest.strip_prefix('=') {
            return Some(value.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn split_sub_resource(value: &str) -> (&str, Option<&str>) {
    match value.find("::") {
        Some(index) => {
            let sub = &value[index + 2..];
            (&value[..index], (!sub.is_empty()).then_some(sub))
        }
        None => (value, None),
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    if left == right {
        return true;
    }
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => false,
    }
}

fn uid_sidecar_owner(path: &Path, document: &ParsedDocument<'_>) -> Option<PathBuf> {
    let name = path.file_name()?.to_str()?;
    if !name.ends_with(".uid") {
        return None;
    }
    let uid = document.tokens.iter().find_map(|token| match &token.kind {
        TokenKind::Path(value) if value.starts_with("uid://") => Some(value.as_str()),
        _ => None,
    })?;
    let _ = uid_to_id(uid)?;
    Some(path.with_extension(""))
}

fn apply_declared_type(targets: &mut [Target], declared_type: &str) {
    for target in targets {
        if target.resource_type.is_none() {
            target.resource_type = Some(declared_type.to_string());
        }
    }
}

fn dedupe_targets(targets: &mut Vec<Target>) {
    let mut seen = BTreeSet::new();
    targets.retain(|target| {
        seen.insert((
            target.path.clone(),
            target.range.start.line,
            target.range.start.character,
            target.range.end.line,
            target.range.end.character,
        ))
    });
}

fn dedupe_live(live: &mut Vec<(PathBuf, Option<String>)>) {
    let mut seen = BTreeSet::new();
    live.retain(|(path, _)| seen.insert(path.clone()));
}

fn join_messages(first: Option<String>, second: Option<String>) -> Option<String> {
    match (first, second) {
        (Some(first), Some(second)) if first == second => Some(first),
        (Some(first), Some(second)) => Some(format!("{first}; {second}")),
        (Some(message), None) | (None, Some(message)) => Some(message),
        (None, None) => None,
    }
}

fn zero_range() -> Range {
    Range {
        start: Position::default(),
        end: Position::default(),
    }
}
