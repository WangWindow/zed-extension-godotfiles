use std::fs;
use std::path::{Path, PathBuf};

use gdfiles_lsp::{Position, Resolution, ResourceIndex, Target, project_root, uid_to_id};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixture helpers
// ---------------------------------------------------------------------------

fn write(root: &Path, relative: &str, contents: &str) -> PathBuf {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(&path, contents).unwrap();
    path
}

fn touch(root: &Path, relative: &str) -> PathBuf {
    write(root, relative, "")
}

fn index(root: &Path) -> ResourceIndex {
    let mut index = ResourceIndex::new(root.to_path_buf());
    index.refresh().unwrap();
    index
}

fn canon(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap()
}

fn pos_at_byte(text: &str, byte: usize) -> Position {
    let prefix = &text[..byte];
    let line = prefix.matches('\n').count() as u32;
    let line_start = prefix.rfind('\n').map(|index| index + 1).unwrap_or(0);
    let character = text[line_start..byte].encode_utf16().count() as u32;
    Position { line, character }
}

fn pos_of(text: &str, needle: &str) -> Position {
    let byte = text
        .find(needle)
        .unwrap_or_else(|| panic!("needle not found: {needle:?} in {text:?}"));
    pos_at_byte(text, byte)
}

fn pos_of_nth(text: &str, needle: &str, nth: usize) -> Position {
    let mut offset = 0;
    for _ in 0..nth {
        offset = text[offset..]
            .find(needle)
            .map(|index| offset + index + 1)
            .expect("nth needle not found");
    }
    let byte = text[offset..]
        .find(needle)
        .map(|index| offset + index)
        .expect("needle not found");
    pos_at_byte(text, byte)
}

fn resolve_at(
    index: &ResourceIndex,
    document: &Path,
    text: &str,
    position: Position,
) -> Resolution {
    index
        .resolve(document, text, position)
        .unwrap_or_else(|| panic!("expected a resolvable token at {position:?}"))
}

fn resolve_needle(index: &ResourceIndex, document: &Path, text: &str, needle: &str) -> Resolution {
    resolve_at(index, document, text, pos_of(text, needle))
}

fn target_paths(resolution: &Resolution) -> Vec<PathBuf> {
    resolution
        .targets
        .iter()
        .map(|target| canon(&target.path))
        .collect()
}

fn assert_single_target(resolution: &Resolution, expected: &Path) {
    let paths = target_paths(resolution);
    assert_eq!(paths, vec![canon(expected)], "targets: {paths:?}");
}

fn assert_no_targets(resolution: &Resolution) {
    assert!(
        resolution.targets.is_empty(),
        "unexpected targets: {:?}",
        resolution.targets
    );
    assert!(
        resolution
            .message
            .as_deref()
            .is_some_and(|message| !message.trim().is_empty()),
        "expected an explanatory message"
    );
}

fn message(resolution: &Resolution) -> &str {
    resolution.message.as_deref().unwrap_or("")
}

fn write_uid_cache(path: &Path, entries: &[(&str, &str)]) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    for (uid, res_path) in entries {
        let id = uid_to_id(uid).expect("valid uid");
        bytes.extend_from_slice(&id.to_le_bytes());
        let path = res_path.as_bytes();
        bytes.extend_from_slice(&(path.len() as u32).to_le_bytes());
        bytes.extend_from_slice(path);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

fn uid_for(path: &Path) -> &Path {
    path
}

// ---------------------------------------------------------------------------
// UID text decoding
// ---------------------------------------------------------------------------

#[test]
fn uid_text_decoding_is_godot_base34_not_base32() {
    assert_eq!(uid_to_id("uid://a"), Some(0));
    assert_eq!(uid_to_id("uid://b"), Some(1));
    assert_eq!(uid_to_id("uid://y"), Some(24));
    // `z` and `9` collide/shift because of Godot's legacy alphabet.
    assert_eq!(uid_to_id("uid://z"), Some(25));
    assert_eq!(uid_to_id("uid://0"), Some(25));
    assert_eq!(uid_to_id("uid://1"), Some(26));
    assert_eq!(uid_to_id("uid://8"), Some(33));
    assert_eq!(uid_to_id("uid://9"), Some(34));

    // Positional, base 34 (base 32 would give different values here).
    assert_eq!(uid_to_id("uid://ba"), Some(34));
    assert_eq!(uid_to_id("uid://ab"), Some(1));
    assert_eq!(uid_to_id("uid://b0"), Some(59));

    assert_eq!(uid_to_id("not-a-uid"), None);
    assert_eq!(uid_to_id("uid://A"), None);
    assert_eq!(uid_to_id("uid://a!"), None);
    assert_eq!(uid_to_id("uid://a-b"), None);

    let mask = 0x7fff_ffff_ffff_ffffu64;
    let max_13 = (0..13u32).fold(0u64, |value, _| value.wrapping_mul(34).wrapping_add(24)) & mask;
    assert_eq!(uid_to_id("uid://yyyyyyyyyyyyy"), Some(max_13));

    // More than 64 bits wraps rather than panicking.
    assert!(uid_to_id("uid://zzzzzzzzzzzzzzzzzz").is_some());
}

// ---------------------------------------------------------------------------
// Project root
// ---------------------------------------------------------------------------

#[test]
fn project_root_finds_nearest_ancestor() {
    let temp = TempDir::new().unwrap();
    let outer = temp.path();
    write(outer, "project.godot", "config_version=5\n");
    let document = outer.join("scripts/nested/thing.gd");
    fs::create_dir_all(document.parent().unwrap()).unwrap();

    assert_eq!(project_root(&document), Some(outer.to_path_buf()));

    let inner = outer.join("addons/inner");
    write(&inner, "project.godot", "config_version=5\n");
    let inner_document = inner.join("scenes/main.tscn");
    fs::create_dir_all(inner_document.parent().unwrap()).unwrap();
    assert_eq!(project_root(&inner_document), Some(inner));

    let other = TempDir::new().unwrap();
    let standalone = other.path().join("elsewhere/file.tscn");
    fs::create_dir_all(standalone.parent().unwrap()).unwrap();
    assert_eq!(project_root(&standalone), None);
}

// ---------------------------------------------------------------------------
// Index discovery: headers, sidecars, imports, excluded directories
// ---------------------------------------------------------------------------

#[test]
fn refresh_discovers_headers_sidecars_and_import_sources() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");

    let scene = write(
        root,
        "scenes/main.tscn",
        "[gd_scene load_steps=2 format=3 uid=\"uid://scene\"]\n\n[node name=\"Root\" type=\"Node\"]\n",
    );
    let resource = write(
        root,
        "materials/metal.tres",
        "[gd_resource type=\"StandardMaterial3D\" format=3 uid=\"uid://resource\"]\n",
    );
    let script = write(root, "scripts/player.gd", "extends Node\n");
    write(root, "scripts/player.gd.uid", "uid://gdscript\n");
    let csharp = write(root, "scripts/Player.cs", "class Player {}\n");
    write(root, "scripts/Player.cs.uid", "uid://csharp\n");
    let image = touch(root, "textures/icon.png");
    write(
        root,
        "textures/icon.png.import",
        "[remap]\n\nimporter=\"texture\"\ntype=\"CompressedTexture2D\"\nuid=\"uid://imported\"\npath=\"res://.godot/imported/icon.png-abc.ctex\"\n",
    );

    let index = index(root);
    let query = root.join("__query.tscn");

    assert_single_target(
        &resolve_needle(&index, &query, "uid://scene", "uid://scene"),
        &scene,
    );
    assert_single_target(
        &resolve_needle(&index, &query, "uid://resource", "uid://resource"),
        &resource,
    );
    assert_single_target(
        &resolve_needle(&index, &query, "uid://gdscript", "uid://gdscript"),
        &script,
    );
    assert_single_target(
        &resolve_needle(&index, &query, "uid://csharp", "uid://csharp"),
        &csharp,
    );
    assert_single_target(
        &resolve_needle(&index, &query, "uid://imported", "uid://imported"),
        &image,
    );

    // `.uid` and `.import` files are metadata, not additional owners.
    let sidecar = root.join("scripts/player.gd.uid");
    let resolution = resolve_needle(&index, &query, "uid://gdscript", "uid://gdscript");
    assert_single_target(&resolution, &script);
    assert_ne!(target_paths(&resolution), vec![canon(&sidecar)]);
}

#[test]
fn ext_resource_uid_declarations_are_references_not_owners() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let owner = write(
        root,
        "owner.tscn",
        "[gd_scene format=3 uid=\"uid://owner\"]\n",
    );
    write(
        root,
        "reference.tscn",
        "[gd_scene format=3]\n\next_resource = ExtResource(\"1_owner\")\n",
    );

    let index = index(root);
    let query_text = "[gd_scene format=3]\n\n[ext_resource type=\"PackedScene\" uid=\"uid://owner\" path=\"res://owner.tscn\" id=\"1_owner\"]\n";
    let resolution = resolve_needle(
        &index,
        &root.join("__query.tscn"),
        query_text,
        "uid://owner",
    );
    assert_single_target(&resolution, &owner);
    assert_eq!(
        resolution.targets.len(),
        1,
        "ext_resource declarations must not add a second owner"
    );
}

#[test]
fn refresh_does_not_traverse_descendant_godot_projects() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let outer = write(
        root,
        "scene.tscn",
        "[gd_scene format=3 uid=\"uid://shared\"]\n",
    );
    write(root, "nested/project.godot", "config_version=5\n");
    write(
        root,
        "nested/scene.tscn",
        "[gd_scene format=3 uid=\"uid://shared\"]\n",
    );

    let index = index(root);
    let query = root.join("__query.tscn");
    let resolution = resolve_needle(&index, &query, "uid://shared", "uid://shared");
    assert_single_target(&resolution, &outer);
    assert_eq!(resolution.targets.len(), 1);
}

#[test]
fn update_rejects_symlink_escapes_outside_paths_and_excluded_dirs() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(&root).unwrap();
    write(&root, "project.godot", "config_version=5\n");
    let outside = write(
        temp.path(),
        "outside/escaped.tscn",
        "[gd_scene format=3 uid=\"uid://escaped\"]\n",
    );

    let mut index = index(&root);
    let query = root.join("__query.tscn");

    // An explicit path update outside the project root is ignored.
    index.update(&outside, None).unwrap();
    assert_no_targets(&resolve_needle(
        &index,
        &query,
        "uid://escaped",
        "uid://escaped",
    ));

    // Metadata in excluded build/editor directories is ignored too.
    let excluded = write(
        &root,
        "bin/manual.tscn",
        "[gd_scene format=3 uid=\"uid://excluded\"]\n",
    );
    index.update(&excluded, None).unwrap();
    assert_no_targets(&resolve_needle(
        &index,
        &query,
        "uid://excluded",
        "uid://excluded",
    ));
    let hidden = write(
        &root,
        ".godot/manual.tscn",
        "[gd_scene format=3 uid=\"uid://hiddenupdate\"]\n",
    );
    index.update(&hidden, None).unwrap();
    assert_no_targets(&resolve_needle(
        &index,
        &query,
        "uid://hiddenupdate",
        "uid://hiddenupdate",
    ));

    #[cfg(unix)]
    {
        let link = root.join("link.tscn");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        index.update(&link, None).unwrap();
        index
            .update(&link, Some("[gd_scene format=3 uid=\"uid://escaped\"]\n"))
            .unwrap();
        assert_no_targets(&resolve_needle(
            &index,
            &query,
            "uid://escaped",
            "uid://escaped",
        ));
    }

    // Descendant-project updates are ignored for the same isolation reason
    // as the refresh walk.
    write(&root, "nested/project.godot", "config_version=5\n");
    let nested = write(
        &root,
        "nested/scene.tscn",
        "[gd_scene format=3 uid=\"uid://nestedupdate\"]\n",
    );
    index.update(&nested, None).unwrap();
    assert_no_targets(&resolve_needle(
        &index,
        &query,
        "uid://nestedupdate",
        "uid://nestedupdate",
    ));

    // A normal in-project update still works.
    let normal = write(
        &root,
        "normal.tscn",
        "[gd_scene format=3 uid=\"uid://normalupdate\"]\n",
    );
    index.update(&normal, None).unwrap();
    assert_single_target(
        &resolve_needle(&index, &query, "uid://normalupdate", "uid://normalupdate"),
        &normal,
    );
}

#[test]
fn excluded_directories_are_not_scanned() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");

    for (directory, uid) in [
        (".godot", "uid://hidden"),
        ("bin", "uid://bin"),
        ("obj", "uid://obj"),
        (".git", "uid://git"),
        ("node_modules", "uid://node"),
    ] {
        write(
            root,
            &format!("{directory}/secret.tscn"),
            &format!("[gd_scene format=3 uid=\"{uid}\"]\n"),
        );
        touch(root, &format!("{directory}/secret.tscn"));
    }

    let index = index(root);
    let query = root.join("__query.tscn");
    for uid in [
        "uid://hidden",
        "uid://bin",
        "uid://obj",
        "uid://git",
        "uid://node",
    ] {
        let resolution = resolve_needle(&index, &query, uid, uid);
        assert_no_targets(&resolution);
        assert!(
            message(&resolution).contains("not found"),
            "message was {:?}",
            resolution.message
        );
    }
}

// ---------------------------------------------------------------------------
// Path, UID, ExtResource, SubResource and res://::sub resolution
// ---------------------------------------------------------------------------

const MAIN_SCENE: &str = concat!(
    "[gd_scene format=3 uid=\"uid://main\"]\n",
    "\n",
    "[ext_resource type=\"Resource\" uid=\"uid://other\" path=\"res://other.tres\" id=\"1_other\"]\n",
    "\n",
    "[sub_resource type=\"Resource\" id=\"Res_local\"]\n",
    "\n",
    "[resource]\n",
    "child = SubResource(\"Res_local\")\n",
    "other = ExtResource(\"1_other\")\n",
    "path = \"res://textures/icon.png\"\n",
);

#[test]
fn resolves_paths_uids_and_local_subresources() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let main = write(root, "main.tscn", MAIN_SCENE);
    let other = write(
        root,
        "other.tres",
        "[gd_resource type=\"Resource\" format=3 uid=\"uid://other\"]\n",
    );
    let icon = touch(root, "textures/icon.png");

    let index = index(root);

    let direct_path = resolve_needle(&index, &main, MAIN_SCENE, "res://textures/icon.png");
    assert_single_target(&direct_path, &icon);

    let direct_uid = resolve_needle(&index, &main, MAIN_SCENE, "uid://other");
    assert_single_target(&direct_uid, &other);

    let ext = resolve_needle(&index, &main, MAIN_SCENE, "ExtResource(\"1_other\")");
    assert_single_target(&ext, &other);
    assert_eq!(ext.token, "1_other");

    let sub = resolve_needle(&index, &main, MAIN_SCENE, "SubResource(\"Res_local\")");
    assert_single_target(&sub, &main);
    assert_eq!(sub.token, "Res_local");
    assert_eq!(sub.targets[0].range.start.line, 4);
    assert_eq!(sub.targets[0].resource_type.as_deref(), Some("Resource"));

    let with_sub = format!("{MAIN_SCENE}\nextra = \"res://main.tscn::Res_local\"\n");
    let cross_file_sub = resolve_needle(&index, &main, &with_sub, "res://main.tscn::Res_local");
    assert_single_target(&cross_file_sub, &main);
    assert_eq!(cross_file_sub.targets[0].range.start.line, 4);

    // Direct res:// path strings in a resource are targets as well.
    let ext_path = resolve_needle(&index, &main, MAIN_SCENE, "res://other.tres");
    assert_single_target(&ext_path, &other);
}

#[test]
fn uid_wins_over_a_stale_literal_ext_resource_path() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let real = write(
        root,
        "real.tres",
        "[gd_resource type=\"Resource\" format=3 uid=\"uid://real\"]\n",
    );
    write(
        root,
        "stale.tres",
        "[gd_resource type=\"Resource\" format=3]\n",
    );

    let text = concat!(
        "[gd_scene format=3]\n",
        "\n",
        "[ext_resource type=\"Resource\" uid=\"uid://real\" path=\"res://stale.tres\" id=\"1_real\"]\n",
        "\n",
        "usage = ExtResource(\"1_real\")\n",
    );
    let document = root.join("uses_stale.tscn");
    let index = index(root);

    let by_id = resolve_needle(&index, &document, text, "ExtResource(\"1_real\")");
    assert_single_target(&by_id, &real);
    assert!(
        message(&by_id).contains("stale"),
        "message was {:?}",
        by_id.message
    );

    let by_literal_path = resolve_needle(&index, &document, text, "res://stale.tres");
    assert_single_target(&by_literal_path, &real);
    assert!(
        message(&by_literal_path).contains("stale"),
        "message was {:?}",
        by_literal_path.message
    );
}

#[test]
fn missing_uid_falls_back_to_the_literal_path() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let exists = write(root, "exists.tres", "[gd_resource format=3]\n");

    let text = concat!(
        "[gd_scene format=3]\n",
        "\n",
        "[ext_resource type=\"Resource\" uid=\"uid://missing\" path=\"res://exists.tres\" id=\"1_fallback\"]\n",
        "\n",
        "usage = ExtResource(\"1_fallback\")\n",
    );
    let document = root.join("uses_fallback.tscn");
    let index = index(root);

    let by_id = resolve_needle(&index, &document, text, "ExtResource(\"1_fallback\")");
    assert_single_target(&by_id, &exists);
    assert!(
        message(&by_id).contains("not found"),
        "message was {:?}",
        by_id.message
    );

    let by_uid_literal = resolve_needle(&index, &document, text, "\"uid://missing\"");
    assert_single_target(&by_uid_literal, &exists);

    let missing_text = concat!(
        "[gd_scene format=3]\n",
        "\n",
        "[ext_resource type=\"Resource\" uid=\"uid://missing\" path=\"res://nope.tres\" id=\"1_missing\"]\n",
        "\n",
        "usage = ExtResource(\"1_missing\")\n",
    );
    let by_missing = resolve_needle(
        &index,
        &document,
        missing_text,
        "ExtResource(\"1_missing\")",
    );
    assert_no_targets(&by_missing);

    let direct_missing = resolve_needle(&index, &document, missing_text, "res://nope.tres");
    assert_no_targets(&direct_missing);
}

#[test]
fn ambiguous_uid_returns_all_candidates_with_a_message() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let first = write(
        root,
        "a.tscn",
        "[gd_scene format=3 uid=\"uid://duplicate\"]\n",
    );
    let second = write(
        root,
        "b.tscn",
        "[gd_scene format=3 uid=\"uid://duplicate\"]\n",
    );

    let index = index(root);
    let document = root.join("__query.tscn");

    let direct = resolve_needle(&index, &document, "uid://duplicate", "uid://duplicate");
    let mut paths = target_paths(&direct);
    paths.sort();
    let mut expected = vec![canon(&first), canon(&second)];
    expected.sort();
    assert_eq!(paths, expected);
    assert!(message(&direct).contains("ambiguous"));

    let text = concat!(
        "[gd_scene format=3]\n",
        "\n",
        "[ext_resource type=\"PackedScene\" uid=\"uid://duplicate\" path=\"res://a.tscn\" id=\"1_dup\"]\n",
        "\n",
        "usage = ExtResource(\"1_dup\")\n",
    );
    let by_id = resolve_needle(&index, &document, text, "ExtResource(\"1_dup\")");
    assert_eq!(by_id.targets.len(), 2);
    assert!(message(&by_id).contains("ambiguous"));
}

// ---------------------------------------------------------------------------
// Incremental updates, overlays, deletions
// ---------------------------------------------------------------------------

#[test]
fn overlay_updates_replace_and_remove_owners() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let document = write(
        root,
        "scene.tscn",
        "[gd_scene format=3 uid=\"uid://one\"]\n",
    );
    let mut index = index(root);
    let query = root.join("__query.tscn");

    assert_single_target(
        &resolve_needle(&index, &query, "uid://one", "uid://one"),
        &document,
    );

    // Unsaved overlay: ownership moves from one to two.
    index
        .update(&document, Some("[gd_scene format=3 uid=\"uid://two\"]\n"))
        .unwrap();
    assert_no_targets(&resolve_needle(&index, &query, "uid://one", "uid://one"));
    assert_single_target(
        &resolve_needle(&index, &query, "uid://two", "uid://two"),
        &document,
    );

    // None rereads disk, restoring the on-disk UID.
    index.update(&document, None).unwrap();
    assert_single_target(
        &resolve_needle(&index, &query, "uid://one", "uid://one"),
        &document,
    );
    assert_no_targets(&resolve_needle(&index, &query, "uid://two", "uid://two"));
}

#[test]
fn sidecar_and_import_updates_remove_stale_ownership() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let script = write(root, "script.gd", "extends Node\n");
    let sidecar = write(root, "script.gd.uid", "uid://sidecar\n");
    let asset = touch(root, "asset.png");
    let import = write(
        root,
        "asset.png.import",
        "[remap]\nuid=\"uid://import\"\ntype=\"CompressedTexture2D\"\n",
    );

    let mut index = index(root);
    let query = root.join("__query.tscn");
    assert_single_target(
        &resolve_needle(&index, &query, "uid://sidecar", "uid://sidecar"),
        &script,
    );
    assert_single_target(
        &resolve_needle(&index, &query, "uid://import", "uid://import"),
        &asset,
    );

    fs::remove_file(&sidecar).unwrap();
    index.update(&sidecar, None).unwrap();
    assert_no_targets(&resolve_needle(
        &index,
        &query,
        "uid://sidecar",
        "uid://sidecar",
    ));

    index
        .update(
            &import,
            Some("[remap]\nuid=\"uid://import2\"\ntype=\"CompressedTexture2D\"\n"),
        )
        .unwrap();
    assert_no_targets(&resolve_needle(
        &index,
        &query,
        "uid://import",
        "uid://import",
    ));
    assert_single_target(
        &resolve_needle(&index, &query, "uid://import2", "uid://import2"),
        &asset,
    );

    fs::remove_file(&import).unwrap();
    index.update(&import, None).unwrap();
    assert_no_targets(&resolve_needle(
        &index,
        &query,
        "uid://import2",
        "uid://import2",
    ));
}

#[test]
fn opened_uid_sidecar_resolves_to_its_owner() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");

    let owners = [
        ("Player.cs", "class Player {}\n", "uid://player"),
        ("player.gd", "extends Node\n", "uid://gdplayer"),
        (
            "shader.gdshader",
            "shader_type canvas_item;\n",
            "uid://shader",
        ),
        (
            "extension.gdextension",
            "[configuration]\n",
            "uid://extension",
        ),
    ];
    let index = index(root);

    for (owner, contents, uid) in owners {
        let owner_path = write(root, owner, contents);
        let sidecar = write(root, &format!("{owner}.uid"), &format!("{uid}\n"));
        let text = format!("{uid}\n");

        let resolution = resolve_at(
            &index,
            &sidecar,
            &text,
            Position {
                line: 0,
                character: 0,
            },
        );
        assert_single_target(&resolution, &owner_path);
        assert_ne!(target_paths(&resolution), vec![canon(&sidecar)]);

        // The direct sidecar path also works before a refresh.
        let fresh = ResourceIndex::new(root.to_path_buf());
        let resolution = resolve_at(
            &fresh,
            &sidecar,
            &text,
            Position {
                line: 0,
                character: 0,
            },
        );
        assert_single_target(&resolution, &owner_path);
    }
}

#[test]
fn unsaved_document_header_uid_resolves_to_itself() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let document = root.join("new_scene.tscn");
    let text = "[gd_scene format=3 uid=\"uid://unsaved\"]\n\n[resource]\n";

    let index = ResourceIndex::new(root.to_path_buf());
    let resolution = resolve_needle(&index, &document, text, "uid://unsaved");
    assert_eq!(resolution.targets.len(), 1);
    assert_eq!(resolution.targets[0].path, document);
}

// ---------------------------------------------------------------------------
// Optional uid_cache.bin fallback
// ---------------------------------------------------------------------------

#[test]
fn uid_cache_is_a_fallback_only() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let cached = write(root, "cached.tscn", "[gd_scene format=3]\n");
    let cache = root.join(".godot/uid_cache.bin");
    write_uid_cache(&cache, &[("uid://cached", "res://cached.tscn")]);

    let mut resource_index = index(root);
    let query = root.join("__query.tscn");
    let resolution = resolve_needle(&resource_index, &query, "uid://cached", "uid://cached");
    assert_single_target(&resolution, &cached);
    assert!(
        message(&resolution).contains("cache"),
        "message was {:?}",
        resolution.message
    );

    // A live file-derived owner always wins over the cache.
    let real = write(
        root,
        "real.tscn",
        "[gd_scene format=3 uid=\"uid://cached\"]\n",
    );
    resource_index.refresh().unwrap();
    let resolution = resolve_needle(&resource_index, &query, "uid://cached", "uid://cached");
    assert_single_target(&resolution, &real);
    assert!(
        !message(&resolution).contains("cache"),
        "cache must not override a live owner: {:?}",
        resolution.message
    );
}

#[test]
fn corrupt_uid_cache_does_not_crash_or_override() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let real = write(
        root,
        "real.tscn",
        "[gd_scene format=3 uid=\"uid://real\"]\n",
    );
    let cache = root.join(".godot/uid_cache.bin");
    if let Some(parent) = cache.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    // A huge entry count with a truncated body.
    fs::write(&cache, [0xff, 0xff, 0xff, 0xff, 1, 2, 3, 4, 5, 6, 7, 8]).unwrap();

    let index = index(root);
    let query = root.join("__query.tscn");
    assert_single_target(
        &resolve_needle(&index, &query, "uid://real", "uid://real"),
        &real,
    );
    assert_no_targets(&resolve_needle(
        &index,
        &query,
        "uid://anything",
        "uid://anything",
    ));
}

#[test]
fn truncated_or_oversized_cache_is_ignored_entirely() {
    // Truncated: a structurally valid prefix must not be used as a partial
    // cache, because it could point at the wrong target.
    let truncated = TempDir::new().unwrap();
    let trunc_root = truncated.path();
    write(trunc_root, "project.godot", "config_version=5\n");
    let ghost = write(trunc_root, "ghost.tscn", "[gd_scene format=3]\n");
    let cache = trunc_root.join(".godot/uid_cache.bin");
    fs::create_dir_all(cache.parent().unwrap()).unwrap();
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&1u32.to_le_bytes());
    bytes.extend_from_slice(&uid_to_id("uid://ghost").unwrap().to_le_bytes());
    bytes.extend_from_slice(&(b"res://ghost.tscn".len() as u32).to_le_bytes());
    bytes.extend_from_slice(b"res://ghost.tscn");
    fs::write(&cache, &bytes[..bytes.len() - 1]).unwrap();

    let trunc_index = index(trunc_root);
    let query = trunc_root.join("__query.tscn");
    assert_no_targets(&resolve_needle(
        &trunc_index,
        &query,
        "uid://ghost",
        "uid://ghost",
    ));
    // The owner file itself is still valid but the sidecar/cache mapping is not.
    assert!(ghost.is_file());

    // Oversized: reject without reading it as a partial cache.
    let oversized = TempDir::new().unwrap();
    let over_root = oversized.path();
    write(over_root, "project.godot", "config_version=5\n");
    let over_cache = over_root.join(".godot/uid_cache.bin");
    if let Some(parent) = over_cache.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let file = fs::File::create(&over_cache).unwrap();
    file.set_len(16 * 1024 * 1024 + 1).unwrap();
    drop(file);

    let over_index = index(over_root);
    let query = over_root.join("__query.tscn");
    assert_no_targets(&resolve_needle(
        &over_index,
        &query,
        "uid://ghost",
        "uid://ghost",
    ));
}

#[cfg(unix)]
#[test]
fn cache_symlinked_outside_root_is_not_read() {
    // Both an external `.godot` directory and an external `uid_cache.bin` file
    // must be ignored. The cache maps to an existing inside-root owner, so a
    // target-level rejection alone would not explain an empty result.
    for file_layout in [false, true] {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(&root).unwrap();
        write(&root, "project.godot", "config_version=5\n");
        let owner = write(&root, "owner.tscn", "[gd_scene format=3]\n");

        let external = temp.path().join("external");
        let external_cache = external.join("uid_cache.bin");
        write_uid_cache(&external_cache, &[("uid://leak", "res://owner.tscn")]);

        if file_layout {
            fs::create_dir_all(root.join(".godot")).unwrap();
            std::os::unix::fs::symlink(&external_cache, root.join(".godot/uid_cache.bin")).unwrap();
        } else {
            std::os::unix::fs::symlink(&external, root.join(".godot")).unwrap();
        }

        let index = index(&root);
        let query = root.join("__query.tscn");
        let resolution = resolve_needle(&index, &query, "uid://leak", "uid://leak");
        assert_no_targets(&resolution);
        assert!(owner.is_file());
    }
}

#[test]
fn uid_cache_honors_hidden_project_data_setting() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(
        root,
        "project.godot",
        "config_version=5\n[application]\nconfig/use_hidden_project_data_directory=false\n",
    );
    let target = write(root, "texture.png", "");
    let cache = root.join("godot/uid_cache.bin");
    write_uid_cache(&cache, &[("uid://visible", "res://texture.png")]);

    let index = index(root);
    let query = root.join("__query.tscn");
    assert_single_target(
        &resolve_needle(&index, &query, "uid://visible", "uid://visible"),
        &target,
    );
}

// ---------------------------------------------------------------------------
// Text handling: comments, escapes, multiline strings, UTF-16/CRLF/Unicode
// ---------------------------------------------------------------------------

#[test]
fn comments_are_ignored_and_utf16_ranges_are_correct() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let icon = touch(root, "textures/icon.png");
    let text = concat!(
        "[gd_scene format=3]\r\n",
        "; comment res://commented.png\r\n",
        "[ext_resource type=\"Tëxture😀\" path=\"res://textures/icon.png\" id=\"1_icon\"]\r\n",
        "[node name=\"Ünicode😀\"]\r\n",
        "path = \"res://textures/icon.png\"\r\n",
    );
    let document = root.join("unicode.tscn");

    let index = index(root);
    let position = pos_of(text, "res://textures/icon.png");
    let resolution = resolve_at(&index, &document, text, position);
    assert_single_target(&resolution, &icon);
    assert_eq!(resolution.range.start.line, 2);
    let prefix = "[ext_resource type=\"Tëxture😀\" path=\"";
    assert_eq!(
        resolution.range.start.character,
        prefix.encode_utf16().count() as u32
    );

    // The string in the comment is not a token.
    assert!(
        index
            .resolve(&document, text, pos_of(text, "res://commented.png"),)
            .is_none()
    );

    // The same path later on a non-ASCII line is still found.
    let later = resolve_at(
        &index,
        &document,
        text,
        pos_of_nth(text, "res://textures/icon.png", 1),
    );
    assert_single_target(&later, &icon);
    assert_eq!(later.range.start.line, 4);
}

#[test]
fn gdshader_include_paths_resolve_and_comments_are_ignored() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let include = write(root, "shaders/common.inc", "// shared\n");
    let text = concat!(
        "#include \"res://shaders/common.inc\"\n",
        "// #include \"res://shaders/commented.inc\"\n",
        "void fragment() {}\n",
    );
    let document = root.join("shader.gdshader");

    let index = ResourceIndex::new(root.to_path_buf());
    let resolution = resolve_needle(&index, &document, text, "res://shaders/common.inc");
    assert_single_target(&resolution, &include);
    assert!(
        index
            .resolve(&document, text, pos_of(text, "res://shaders/commented.inc"),)
            .is_none()
    );
}

#[test]
fn escapes_and_multiline_strings_are_resolved() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    write(root, "project.godot", "config_version=5\n");
    let icon = touch(root, "textures/icon.png");
    let nested = write(root, "dir/file.tres", "[gd_resource format=3]\n");

    let index = index(root);
    let query = root.join("__query.tscn");

    let escaped = concat!(
        "[gd_scene format=3]\n",
        "\n",
        "[ext_resource type=\"Resource\" path=\"res://dir\\u002Ffile.tres\" id=\"1_escaped\"]\n",
        "\n",
        "usage = ExtResource(\"1_escaped\")\n",
    );
    let escaped_resolution = resolve_needle(&index, &query, escaped, "ExtResource(\"1_escaped\")");
    assert_single_target(&escaped_resolution, &nested);

    let escaped_path = resolve_needle(&index, &query, escaped, "res://dir\\u002Ffile.tres");
    assert_single_target(&escaped_path, &nested);

    let multiline = concat!(
        "[gd_scene format=3]\n",
        "\n",
        "description = \"\"\"res://textures/icon.png\"\"\"\n",
    );
    let multiline_resolution = resolve_needle(&index, &query, multiline, "res://textures/icon.png");
    assert_single_target(&multiline_resolution, &icon);
}

// ---------------------------------------------------------------------------
// Isolation and rejection
// ---------------------------------------------------------------------------

#[test]
fn rejects_user_scheme_root_escapes_and_symlink_escapes() {
    let temp = TempDir::new().unwrap();
    let root = temp.path().join("project");
    fs::create_dir_all(&root).unwrap();
    write(&root, "project.godot", "config_version=5\n");
    let outside = temp.path().join("outside.tres");
    fs::write(&outside, "[gd_resource format=3]\n").unwrap();

    let index = index(&root);
    let document = root.join("__query.tscn");

    let user = resolve_needle(&index, &document, "user://save.dat", "user://save.dat");
    assert_no_targets(&user);

    let escape = resolve_needle(
        &index,
        &document,
        "res://../outside.tres",
        "res://../outside.tres",
    );
    assert_no_targets(&escape);

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, root.join("link.tres")).unwrap();
        let symlink_escape =
            resolve_needle(&index, &document, "res://link.tres", "res://link.tres");
        assert_no_targets(&symlink_escape);
    }

    assert!(
        index
            .resolve(
                &document,
                "file:///etc/passwd",
                Position {
                    line: 0,
                    character: 0
                },
            )
            .is_none()
    );
}

#[test]
fn projects_are_isolated_by_root() {
    let temp = TempDir::new().unwrap();
    let first = temp.path().join("first");
    let second = temp.path().join("second");
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    write(&first, "project.godot", "config_version=5\n");
    write(&second, "project.godot", "config_version=5\n");
    let first_scene = write(
        &first,
        "scene.tscn",
        "[gd_scene format=3 uid=\"uid://shared\"]\n",
    );
    write(
        &second,
        "scene.tscn",
        "[gd_scene format=3 uid=\"uid://shared\"]\n",
    );

    let index = index(&first);
    let query = first.join("__query.tscn");
    let resolution = resolve_needle(&index, &query, "uid://shared", "uid://shared");
    assert_single_target(&resolution, &first_scene);

    let second_document = second.join("scene.tscn");
    assert_eq!(project_root(&second_document), Some(second));
}

// ---------------------------------------------------------------------------
// Small sanity checks for public shapes
// ---------------------------------------------------------------------------

#[test]
fn target_shape_is_public_and_constructible() {
    let target = Target {
        path: PathBuf::from("x"),
        range: gdfiles_lsp::Range::default(),
        resource_type: Some("Resource".to_string()),
    };
    assert_eq!(target.path, PathBuf::from("x"));
    assert_eq!(uid_for(&target.path), Path::new("x"));
}
