#!/usr/bin/env python3
"""Contract tests for the native gdfiles-lsp release packaging.

These tests never execute the packaged binary. They build archives from a
fake executable and inspect archive paths, modes, and contents.
"""

from __future__ import annotations

import importlib.util
import io
import shlex
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "package_gdfiles_lsp.py"
WORKFLOW = ROOT / ".github" / "workflows" / "gdfiles-lsp-release.yml"

EXPECTED_TARGETS = (
    "x86_64-unknown-linux-gnu",
    "aarch64-unknown-linux-gnu",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)
LINUX = "x86_64-unknown-linux-gnu"
WINDOWS = "x86_64-pc-windows-msvc"
VERSION = "0.1.0"

FAKE_BINARY = b"\x7fELF\x02\x01\x01\x00" + b"gdfiles-lsp fixture\x00" * 32
FAKE_LICENSE = b"MIT License\n\nCopyright fixture\n"


def _load_packaging_module():
    spec = importlib.util.spec_from_file_location("package_gdfiles_lsp", SCRIPT)
    if spec is None or spec.loader is None:  # pragma: no cover - import sanity
        raise RuntimeError(f"cannot load {SCRIPT}")
    module = importlib.util.module_from_spec(spec)
    # Register before execution so dataclasses can resolve the module.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


packaging = _load_packaging_module()


class ContractConstantsTests(unittest.TestCase):
    def test_supported_targets_match_distribution_contract(self):
        self.assertEqual(tuple(packaging.SUPPORTED_TARGETS), EXPECTED_TARGETS)

    def test_archive_names(self):
        basename = "gdfiles-lsp-v0.1.0-x86_64-unknown-linux-gnu"
        self.assertEqual(
            packaging.archive_name(VERSION, LINUX), f"{basename}.tar.gz"
        )
        windows_basename = "gdfiles-lsp-v0.1.0-x86_64-pc-windows-msvc"
        self.assertEqual(
            packaging.archive_name(VERSION, WINDOWS), f"{windows_basename}.zip"
        )

    def test_executable_names(self):
        self.assertEqual(packaging.executable_name(LINUX), "gdfiles-lsp")
        self.assertEqual(
            packaging.executable_name("aarch64-apple-darwin"),
            "gdfiles-lsp",
        )
        self.assertEqual(
            packaging.executable_name(WINDOWS), "gdfiles-lsp.exe"
        )

    def test_release_tag(self):
        self.assertEqual(packaging.expected_tag(VERSION), "gdfiles-lsp-v0.1.0")
        self.assertEqual(
            packaging.verify_tag(VERSION, "gdfiles-lsp-v0.1.0"),
            "gdfiles-lsp-v0.1.0",
        )
        self.assertIsNone(packaging.verify_tag(VERSION, ""))
        self.assertIsNone(packaging.verify_tag(VERSION, None))


class ValidationTests(unittest.TestCase):
    def test_accepts_cargo_semver_versions(self):
        for version in (
            "0.0.0",
            "1.2.3",
            "10.20.30",
            "1.2.3-rc.1",
            "1.2.3-alpha.1+build.7",
            "1.2.3+20260101",
        ):
            with self.subTest(version=version):
                self.assertEqual(packaging.validate_version(version), version)

    def test_rejects_invalid_versions(self):
        for version in (
            "",
            "1",
            "1.0",
            "v1.0.0",
            "01.0.0",
            "1.0.0.0",
            "1.0.0-",
            "1.0.0+",
            "1.0.0 ",
            "1.0.0\n",
            "../1.0.0",
            "1.0.0/../../escape",
            "gdfiles-lsp-v1.0.0",
        ):
            with self.subTest(version=version):
                with self.assertRaises(packaging.PackagingError):
                    packaging.validate_version(version)
        with self.assertRaises(packaging.PackagingError):
            packaging.validate_version(None)  # type: ignore[arg-type]

    def test_rejects_unsupported_targets(self):
        for target in (
            "",
            "x86_64-linux-gnu",
            "x86_64-unknown-linux-musl",
            "aarch64-apple-darwin2",
            "../../x86_64-unknown-linux-gnu",
        ):
            with self.subTest(target=target):
                with self.assertRaises(packaging.PackagingError):
                    packaging.validate_target(target)

    def test_rejects_mismatched_release_tag(self):
        with self.assertRaises(packaging.PackagingError):
            packaging.verify_tag(VERSION, "gdfiles-lsp-v0.1.1")
        with self.assertRaises(packaging.PackagingError):
            packaging.verify_tag(VERSION, "gdfiles-lsp-v0.1.0-rc.1")
        with self.assertRaises(packaging.PackagingError):
            packaging.verify_tag(VERSION, "v0.1.0")


class PackagingFixture(unittest.TestCase):
    def setUp(self):
        self._temporary = tempfile.TemporaryDirectory()
        self.root = Path(self._temporary.name)
        self.binary = self.root / "gdfiles-lsp"
        self.binary.write_bytes(FAKE_BINARY)
        self.binary.chmod(0o755)
        self.license = self.root / "LICENCE"
        self.license.write_bytes(FAKE_LICENSE)
        self.dist = self.root / "dist"

    def tearDown(self):
        self._temporary.cleanup()

    def package_target(
        self,
        target=LINUX,
        *,
        binary=None,
        version=VERSION,
        tag=None,
        output_dir=None,
    ):
        return packaging.package(
            binary=binary if binary is not None else self.binary,
            license_path=self.license,
            version=version,
            target=target,
            output_dir=output_dir if output_dir is not None else self.dist,
            tag=tag,
        )


class TarPackagingTests(PackagingFixture):
    def test_archive_paths_modes_and_contents(self):
        result = self.package_target(LINUX)
        basename = f"gdfiles-lsp-v{VERSION}-{LINUX}"
        self.assertEqual(result.archive, self.dist / f"{basename}.tar.gz")

        with tarfile.open(result.archive, mode="r:gz") as archive:
            members = {member.name: member for member in archive.getmembers()}
            self.assertEqual(set(members), {"gdfiles-lsp", "LICENCE"})
            executable = members["gdfiles-lsp"]
            self.assertTrue(executable.isfile())
            self.assertEqual(executable.mode & 0o777, 0o755)
            self.assertEqual(archive.extractfile(executable).read(), FAKE_BINARY)
            self.assertEqual(
                archive.extractfile(members["LICENCE"]).read(), FAKE_LICENSE
            )
            for member in archive.getmembers():
                self.assertFalse(Path(member.name).is_absolute())
                self.assertNotIn("..", Path(member.name).parts)

    def test_package_does_not_execute_the_binary(self):
        marker = self.root / "binary-was-executed"
        script = self.root / "gdfiles-lsp"
        script.write_bytes(
            f"#!/bin/sh\n: > {shlex.quote(str(marker))}\nexit 42\n".encode()
        )
        script.chmod(0o755)
        self.package_target(LINUX, binary=script)
        self.package_target(WINDOWS, binary=script)
        self.assertFalse(marker.exists())


class ZipPackagingTests(PackagingFixture):
    def test_zip_paths_modes_and_contents(self):
        result = self.package_target(WINDOWS)
        basename = f"gdfiles-lsp-v{VERSION}-{WINDOWS}"
        self.assertEqual(result.archive, self.dist / f"{basename}.zip")

        with zipfile.ZipFile(result.archive) as archive:
            self.assertEqual(
                set(archive.namelist()),
                {"gdfiles-lsp.exe", "LICENCE"},
            )
            self.assertEqual(archive.read("gdfiles-lsp.exe"), FAKE_BINARY)
            self.assertEqual(archive.read("LICENCE"), FAKE_LICENSE)
            executable = archive.getinfo("gdfiles-lsp.exe")
            self.assertFalse(executable.is_dir())
            self.assertEqual((executable.external_attr >> 16) & 0o777, 0o755)
            for name in archive.namelist():
                self.assertFalse(Path(name).is_absolute())
                self.assertNotIn("..", Path(name).parts)


class SafetyTests(PackagingFixture):
    def test_rejects_traversal_version_without_writing(self):
        before = sorted(path.name for path in self.root.iterdir())
        with self.assertRaises(packaging.PackagingError):
            self.package_target(version="../../escape")
        self.assertFalse(self.dist.exists())
        self.assertEqual(sorted(path.name for path in self.root.iterdir()), before)

    def test_rejects_mismatched_tag_without_writing(self):
        with self.assertRaises(packaging.PackagingError):
            self.package_target(tag="gdfiles-lsp-v0.1.1")
        self.assertFalse(self.dist.exists())

    def test_rejects_symlinked_archive_that_escapes_destination(self):
        self.dist.mkdir()
        outside = self.root / "outside.tar.gz"
        outside.write_bytes(b"keep me")
        archive = self.dist / packaging.archive_name(VERSION, LINUX)
        archive.symlink_to(outside)
        with self.assertRaises(packaging.PackagingError):
            self.package_target(LINUX)
        self.assertEqual(outside.read_bytes(), b"keep me")

    def test_rejects_output_directory_that_is_a_file(self):
        blocked = self.root / "not-a-directory"
        blocked.write_bytes(b"x")
        with self.assertRaises(packaging.PackagingError):
            self.package_target(LINUX, output_dir=blocked)

    def test_rejects_missing_and_empty_inputs(self):
        with self.assertRaises(packaging.PackagingError):
            self.package_target(LINUX, binary=self.root / "missing")
        empty = self.root / "empty"
        empty.write_bytes(b"")
        with self.assertRaises(packaging.PackagingError):
            self.package_target(LINUX, binary=empty)
        with self.assertRaises(packaging.PackagingError):
            packaging.package(
                binary=self.binary,
                license_path=self.root / "missing-licence",
                version=VERSION,
                target=LINUX,
                output_dir=self.dist,
            )

    def test_all_targets_create_unique_contract_archives(self):
        names = set()
        for target in EXPECTED_TARGETS:
            result = self.package_target(target)
            packaging.verify_archive(VERSION, target, result.archive)
            names.add(result.archive.name)
        self.assertEqual(len(names), 5)


class VerifyReleaseTests(PackagingFixture):
    def setUp(self):
        super().setUp()
        for target in EXPECTED_TARGETS:
            self.package_target(target)

    def test_accepts_complete_asset_set(self):
        verified = packaging.verify_release(self.dist, VERSION, "gdfiles-lsp-v0.1.0")
        self.assertEqual(len(verified), 5)
        self.assertEqual(
            {path.name for path in verified},
            {packaging.archive_name(VERSION, target) for target in EXPECTED_TARGETS},
        )

    def test_rejects_missing_archive(self):
        (self.dist / packaging.archive_name(VERSION, LINUX)).unlink()
        with self.assertRaises(packaging.PackagingError):
            packaging.verify_release(self.dist, VERSION)

    def test_rejects_extra_asset(self):
        (self.dist / "SHA256SUMS").write_text("unexpected\n", encoding="ascii")
        with self.assertRaises(packaging.PackagingError):
            packaging.verify_release(self.dist, VERSION)

    def test_rejects_non_file_asset(self):
        (self.dist / "nested").mkdir()
        with self.assertRaises(packaging.PackagingError):
            packaging.verify_release(self.dist, VERSION)

    def test_rejects_release_tag_mismatch(self):
        with self.assertRaises(packaging.PackagingError):
            packaging.verify_release(self.dist, VERSION, "gdfiles-lsp-v0.1.1")

    def test_rejects_non_executable_archive_member(self):
        bad_dir = self.root / "bad-mode"
        bad_dir.mkdir()
        archive_path = bad_dir / packaging.archive_name(VERSION, LINUX)
        with tarfile.open(archive_path, mode="w:gz") as archive:
            for name, data, mode in (
                ("gdfiles-lsp", FAKE_BINARY, 0o644),
                ("LICENCE", FAKE_LICENSE, 0o644),
            ):
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mode = mode
                archive.addfile(info, io.BytesIO(data))
        with self.assertRaises(packaging.PackagingError):
            packaging.verify_archive(VERSION, LINUX, archive_path)

    def test_rejects_duplicate_archive_member(self):
        bad_dir = self.root / "duplicates"
        bad_dir.mkdir()
        archive_path = bad_dir / packaging.archive_name(VERSION, LINUX)
        with tarfile.open(archive_path, mode="w:gz") as archive:
            for name, data, mode in (
                ("gdfiles-lsp", FAKE_BINARY, 0o755),
                ("LICENCE", FAKE_LICENSE, 0o644),
                ("LICENCE", b"second licence\n", 0o644),
            ):
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mode = mode
                archive.addfile(info, io.BytesIO(data))
        with self.assertRaises(packaging.PackagingError):
            packaging.verify_archive(VERSION, LINUX, archive_path)

    def test_rejects_archive_member_path_traversal(self):
        bad_dir = self.root / "bad-paths"
        bad_dir.mkdir()
        archive_path = bad_dir / packaging.archive_name(VERSION, LINUX)
        with tarfile.open(archive_path, mode="w:gz") as archive:
            for name, data in (
                ("../gdfiles-lsp", FAKE_BINARY),
                ("LICENCE", FAKE_LICENSE),
            ):
                info = tarfile.TarInfo(name)
                info.size = len(data)
                info.mode = 0o755
                archive.addfile(info, io.BytesIO(data))
        with self.assertRaises(packaging.PackagingError):
            packaging.verify_archive(VERSION, LINUX, archive_path)


class CliTests(PackagingFixture):
    def run_cli(self, *arguments):
        return subprocess.run(
            [sys.executable, str(SCRIPT), *arguments],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_expected_tag_command(self):
        result = self.run_cli("expected-tag", "--version", VERSION)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, "gdfiles-lsp-v0.1.0\n")
        invalid = self.run_cli("expected-tag", "--version", "../0.1.0")
        self.assertEqual(invalid.returncode, 2)
        self.assertIn("invalid package version", invalid.stderr)

    def test_package_command_writes_valid_archive(self):
        result = self.run_cli(
            "package",
            "--binary",
            str(self.binary),
            "--license",
            str(self.license),
            "--version",
            VERSION,
            "--target",
            LINUX,
            "--output-dir",
            str(self.dist),
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            [path.name for path in self.dist.iterdir()],
            [packaging.archive_name(VERSION, LINUX)],
        )
        packaging.verify_archive(
            VERSION, LINUX, self.dist / packaging.archive_name(VERSION, LINUX)
        )

    def test_package_command_accepts_empty_tag_for_dry_run(self):
        result = self.run_cli(
            "package",
            "--binary",
            str(self.binary),
            "--license",
            str(self.license),
            "--version",
            VERSION,
            "--target",
            LINUX,
            "--output-dir",
            str(self.dist),
            "--tag",
            "",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        packaging.verify_archive(
            VERSION, LINUX, self.dist / packaging.archive_name(VERSION, LINUX)
        )

    def test_verify_release_command(self):
        for target in EXPECTED_TARGETS:
            self.package_target(target)
        result = self.run_cli(
            "verify-release",
            "--dist",
            str(self.dist),
            "--version",
            VERSION,
            "--tag",
            "gdfiles-lsp-v0.1.0",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("verified 5 release archives", result.stdout)

    def test_package_command_rejects_wrong_tag(self):
        result = self.run_cli(
            "package",
            "--binary",
            str(self.binary),
            "--license",
            str(self.license),
            "--version",
            VERSION,
            "--target",
            LINUX,
            "--output-dir",
            str(self.dist),
            "--tag",
            "gdfiles-lsp-v9.9.9",
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("does not match package version", result.stderr)
        self.assertFalse(self.dist.exists())


class WorkflowContractTests(unittest.TestCase):
    """Static contract checks; remote behavior needs a real GitHub tag run."""

    def setUp(self):
        self.workflow = WORKFLOW.read_text(encoding="utf-8")

    def test_only_tag_or_manual_dispatch_trigger(self):
        self.assertIn("tags:", self.workflow)
        self.assertIn('"gdfiles-lsp-v*"', self.workflow)
        self.assertIn("workflow_dispatch:", self.workflow)
        self.assertNotIn("branches:", self.workflow)

    def test_native_runner_matrix_uses_current_official_labels(self):
        for label in (
            "ubuntu-24.04",
            "ubuntu-24.04-arm",
            "macos-15-intel",
            "macos-26",
            "windows-2025",
        ):
            with self.subTest(runner=label):
                self.assertIn(f"runner: {label}", self.workflow)
        self.assertNotIn("macos-13", self.workflow)
        self.assertNotIn("ubuntu-22.04", self.workflow)
        self.assertNotIn("windows-2022", self.workflow)
        for target in EXPECTED_TARGETS:
            with self.subTest(target=target):
                self.assertIn(target, self.workflow)

    def test_build_is_scoped_to_native_crate_and_gates_on_validation(self):
        self.assertIn(
            "cargo build --release -p gdfiles-lsp --bin gdfiles-lsp --target",
            self.workflow,
        )
        self.assertIn("cargo test -p gdfiles-lsp", self.workflow)
        self.assertIn("needs: validate", self.workflow)
        self.assertNotIn("extension.wasm", self.workflow)

    def test_publish_permission_is_minimal_and_tag_gated(self):
        permission_lines = [line.strip() for line in self.workflow.splitlines()]
        self.assertEqual(permission_lines.count("contents: write"), 1)
        self.assertEqual(permission_lines.count("contents: read"), 1)
        self.assertIn("github.event_name == 'push'", self.workflow)
        self.assertIn(
            "startsWith(github.ref, 'refs/tags/gdfiles-lsp-v')", self.workflow
        )
        self.assertIn("GH_TOKEN: ${{ github.token }}", self.workflow)
        self.assertIn("gh release create", self.workflow)
        self.assertIn("--verify-tag", self.workflow)

    def test_assets_are_verified_with_shared_packaging_contract(self):
        self.assertIn("verify-release", self.workflow)
        self.assertIn(
            "python3 -m unittest discover -s tests -p test_release_packaging.py",
            self.workflow,
        )
        self.assertIn("actions/upload-artifact@v4", self.workflow)
        self.assertIn("actions/download-artifact@v4", self.workflow)
        self.assertNotIn("sha256", self.workflow.lower())


if __name__ == "__main__":
    unittest.main()
