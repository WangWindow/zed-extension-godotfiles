# Godot Files for Zed

Godot file and scripting support for Zed.

## Features

- Syntax highlighting for Godot scenes, resources, GDShader and GDScript.
- Jump to resources through `uid://`, `res://`, `ExtResource` and `SubResource`.
- Hover information with resolved paths, resource types and image thumbnails.
- UID indexing from resource headers, `.uid` sidecars and `.import` metadata.
- GDScript language service and debugging through the Godot editor.
- Optional GDShader diagnostics and completion through an external LSP.

![Godot resource hover in Zed showing a resolved path, resource type, open link and image preview](assets/2026-11-10_21-20-18.png)

## Installation

> C# language features are provided by [zed-csharp](https://github.com/zed-extensions/csharp),
> with debugging through [zed-netcoredbg](https://github.com/qwadrox/zed-netcoredbg).

Run `zed: install dev extension` in Zed and select this repository. Disable the
upstream GDScript extension if installed.

The resource server is found in this order: a configured binary path, local
installation, cached release, then automatic download from GitHub Releases.

To build and install the server locally:

```sh
cargo install --path crates/gdfiles-lsp
```

The resource server starts when opening Godot Resource, Godot UID or GDShader files.


## C# debugging and tasks

Install [zed-netcoredbg](https://github.com/qwadrox/zed-netcoredbg) for the
`netcoredbg` debug adapter, and use the .NET edition of Godot with the .NET SDK.
The examples below assume the Zed worktree root contains `project.godot` and your
`.csproj` or `.slnx`(`sln`). Adjust paths if your Godot project is in a subdirectory.

> The task expects `godot-mono` on PATH; replace that command if your executable
has a different name or location.

### Build and debug

Add to `.zed/debug.json`:

```json
[
  {
    "label": "Godot C#: Build & Debug",
    "adapter": "netcoredbg",
    "request": "launch",
    "program": "godot-mono",
    "args": ["--path", "${ZED_WORKTREE_ROOT}"],
    "cwd": "${ZED_WORKTREE_ROOT}",
    "stopAtEntry": false,
    "build": {
      "command": "dotnet build -c Debug",
      "cwd": "$ZED_WORKTREE_ROOT"
    }
  }
]
```

This launch configuration builds the project before starting Godot under the
debugger; it does not require selecting a running process ID.

### Build and play without debugging

Add to `.zed/tasks.json`:

```json
[
  {
    "label": "Build",
    "command": "dotnet build -c Debug",
    "cwd": "$ZED_WORKTREE_ROOT"
  },
  {
    "label": "Build & Play",
    "command": "dotnet build -c Debug && godot-mono --path \"$ZED_WORKTREE_ROOT\"",
    "cwd": "$ZED_WORKTREE_ROOT",
    "use_new_terminal": true
  }
]
```

These tasks run without attaching a debugger.

## GDScript

For completion, diagnostics and navigation:

1. Open the same project in the Godot editor and keep it running.
2. In Godot's Editor Settings, search for **Language Server** and check its port
   (default: `6005`).
3. Open a `.gd` file in Zed. The extension connects to `127.0.0.1:6005` through
   `gdfiles-lsp`; it does not start Godot for you.

### GDScript syntax highlighting only

If you do not need the GDScript language service, disable it in Zed's settings:

```json
{
  "languages": {
    "GDScript": { "language_servers": [] }
  }
}
```

This prevents the GDScript language service and bridge process from starting.
GDScript highlighting, resource navigation, image previews and C# support remain
available without a running Godot editor.

### Connection settings

If Godot uses a different host or port, match it in Zed's settings:

```json
{
  "lsp": {
    "gdscript": {
      "settings": {
        "bridge": {
          "host": "127.0.0.1",
          "port": 6005,
          "connectTimeoutMs": 5000
        }
      }
    }
  }
}
```

If Zed reports `Server reset the connection` with `Connection refused` in stderr,
the bridge could not connect to Godot's language server. Check that Godot has the
project open and that the host and port match, then restart the GDScript language
server in Zed or reopen the Zed window. Increasing `connectTimeoutMs` will not fix
a port with no server listening. If you do not need GDScript features, use the
disable setting above instead.

### GDScript debugging

Keep the project open in Godot for debugging as well. The `godot` adapter connects
to the editor's debug adapter at `127.0.0.1:6006`, separate from the LSP port `6005`.
Add a launch or attach task to `.zed/debug.json`:

```json
[
  { "adapter": "godot", "label": "Godot: launch", "request": "launch" },
  { "adapter": "godot", "label": "Godot: attach", "request": "attach" }
]
```

For C#, use the separate `netcoredbg` adapter described above, not `godot`.

## GDShader

[gdshader-lsp-cpp](https://github.com/Dead-Shrimp-Studio/gdshader-lsp-cpp) provides
code checks and other language features without a running Godot editor. It is
optional and off by default in current Zed versions.

Install an [upstream release](https://github.com/Dead-Shrimp-Studio/gdshader-lsp-cpp/releases)
matching your Godot version: `v0.3.2` for 4.7+, or `v0.2.6` for 4.5–4.6. Open the
directory containing `project.godot` as the Zed project root for `res://` includes.
Enable the server and point to its executable:

```json
{
  "languages": {
    "GDShader": {
      "language_servers": ["gdfiles-lsp", "gdshader-lsp"]
    }
  },
  "lsp": {
    "gdshader-lsp": {
      "binary": { "path": "/absolute/path/to/gdshader-lsp" }
    }
  }
}
```

Alternatively, install the upstream executable as `gdshader-lsp` or `gdshader_lsp`
on PATH (with `.exe` on Windows) and omit `binary.path`. The hyphenated name is
preferred when both exist. Rename the downloaded
executable if needed; an explicit `binary.path` accepts any filename.
It passes `--stdio --log-path=none` by default to avoid creating log files in your
project. If you override `binary.arguments`, keep `--stdio` and choose your logging
setting explicitly. The backend is not downloaded automatically.

With the official `v0.3.2` Linux release:

- Diagnostics, clearing errors after edits, completion, references, rename and
  `res://` includes were tested. Reference results may highlight the whole
  declaration rather than only its name.
- Semantic tokens, inlay hints, folding, code actions, call hierarchy, workspace
  symbols and colors are advertised, but have not been individually tested here.
- Formatting, Shader symbol hover/definition, document symbols and signature help
  are not advertised, so Zed cannot use them. Existing resource hover and path
  navigation from `gdfiles-lsp` remain available.
- Directly opening a function-bearing `.gdshaderinc` may incorrectly report
  `GDS3012` (missing `shader_type`); this is an upstream limitation.

The upstream Linux binary requires glibc 2.38+ and C++ runtime libraries; it is
separate from this extension's static musl `gdfiles-lsp` binary. Upstream does not
provide a Linux ARM64 binary.

To disable shader code intelligence while keeping highlighting and resource features:

```json
{
  "languages": {
    "GDShader": {
      "language_servers": ["gdfiles-lsp"]
    }
  }
}
```

The community backend's diagnostics may differ from Godot's shader compiler;
check the shader in Godot before relying on it in a game.

## Image previews

PNG, JPEG, WebP, SVG, ICO, BMP, GIF, TGA, TIFF, DDS and HDR are supported.
GIF previews use the first frame. DDS support covers DXT1/DXT3/DXT5.
SVG previews render vector shapes without text or embedded images; filtered SVGs show metadata.
Thumbnails fit within 192 × 128 px, keep their aspect ratio and never enlarge small images.

To select a binary or disable previews, add to Zed's settings:

```json
{
  "lsp": {
    "gdfiles-lsp": {
      "binary": { "path": "/absolute/path/to/gdfiles-lsp" },
      "initialization_options": { "enablePreview": false }
    }
  }
}
```

## Thanks

- [zed-gdscript](https://github.com/grndctrl/zed-gdscript) — original extension.
- [zed-csharp](https://github.com/zed-extensions/csharp) and [zed-netcoredbg](https://github.com/qwadrox/zed-netcoredbg) — C# language features and .NET debugging.
- [Godot Files for VS Code](https://github.com/AlfishSoftware/godot-files-vscode) — resource navigation and preview reference.
- [tree-sitter-gdscript](https://github.com/PrestonKnopp/tree-sitter-gdscript) and [tree-sitter-godot-resource](https://github.com/PrestonKnopp/tree-sitter-godot-resource) — Godot grammars.
- [tree-sitter-gdshader](https://github.com/airblast-dev/tree-sitter-gdshader) — GDShader grammar and highlight queries.
- [gdshader-lsp-cpp](https://github.com/Dead-Shrimp-Studio/gdshader-lsp-cpp) — optional GDShader language server.
- [Zed extension API](https://github.com/zed-industries/zed/tree/main/crates/extension_api) and [lsp-server](https://github.com/rust-lang/rust-analyzer/tree/master/lib/lsp-server) — editor integration and LSP transport.
- [image](https://github.com/image-rs/image) and [resvg](https://github.com/linebender/resvg) — image decoding and SVG rendering.
