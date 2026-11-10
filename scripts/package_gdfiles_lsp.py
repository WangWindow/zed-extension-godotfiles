#!/usr/bin/env python3
"""Package and verify ``gdfiles-lsp`` release archives.

The distribution contract is shared with the Zed extension downloader:

* release tag: ``gdfiles-lsp-v${version}``
* artifact basename: ``gdfiles-lsp-v${version}-${target}``
* Linux/macOS: ``${basename}.tar.gz`` containing ``gdfiles-lsp``
  (mode 0755) and ``LICENCE`` as top-level members
* Linux archives target musl and are statically linked, with no dynamic
  loader or shared-library dependencies.
* Windows: ``${basename}.zip`` containing ``gdfiles-lsp.exe`` and
  ``LICENCE`` as top-level members

Only the native archives are published; there are no checksum or signature
sidecars. Only Python's standard library is used so the workflow runners do not
need extra packages. Packaging never executes the binary; it only reads bytes.
"""

from __future__ import annotations

import argparse
import gzip
import io
import os
import re
import sys
import tarfile
import tempfile
import zipfile
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

EXECUTABLE = "gdfiles-lsp"
LICENSE = "LICENCE"
TAG_PREFIX = "gdfiles-lsp-v"

SUPPORTED_TARGETS = (
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin",
    "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc",
)

# Match Cargo's semver versions without accepting a leading ``v`` or path
# separators. Build metadata and pre-release suffixes are allowed.
_VERSION_RE = re.compile(
    r"(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)

# Fixed timestamps keep archives reproducible and avoid leaking build times.
_TAR_MTIME = 315532800  # 1980-01-01T00:00:00Z, also the zip epoch.
_EXECUTABLE_MODE = 0o755
_LICENSE_MODE = 0o644


class PackagingError(ValueError):
    """Raised when an input or artifact violates the release contract."""


@dataclass(frozen=True)
class PackageResult:
    archive: Path
    executable: str
    version: str
    target: str


def validate_version(version: str) -> str:
    if not isinstance(version, str) or not _VERSION_RE.fullmatch(version):
        raise PackagingError(
            f"invalid package version {version!r}; expected semver like 0.1.0"
        )
    return version


def validate_target(target: str) -> str:
    if target not in SUPPORTED_TARGETS:
        supported = ", ".join(SUPPORTED_TARGETS)
        raise PackagingError(
            f"unsupported release target {target!r}; supported targets: {supported}"
        )
    return target


def executable_name(target: str) -> str:
    validate_target(target)
    return f"{EXECUTABLE}.exe" if target.endswith("-pc-windows-msvc") else EXECUTABLE


def artifact_basename(version: str, target: str) -> str:
    validate_version(version)
    validate_target(target)
    return f"{EXECUTABLE}-v{version}-{target}"


def archive_name(version: str, target: str) -> str:
    basename = artifact_basename(version, target)
    return (
        f"{basename}.zip"
        if target.endswith("-pc-windows-msvc")
        else f"{basename}.tar.gz"
    )


def expected_tag(version: str) -> str:
    validate_version(version)
    return f"{TAG_PREFIX}{version}"


def verify_tag(version: str, tag: str | None) -> str | None:
    """Return ``tag`` when it matches, treating an empty tag as "no release"."""
    if tag is None or tag == "":
        return None
    expected = expected_tag(version)
    if tag != expected:
        raise PackagingError(
            f"release tag {tag!r} does not match package version {version!r} "
            f"(expected {expected!r})"
        )
    return tag


def _read_input(path: str | Path, label: str) -> bytes:
    source = Path(path)
    if not source.is_file():
        raise PackagingError(f"{label} does not exist or is not a file: {source}")
    data = source.read_bytes()
    if not data:
        raise PackagingError(f"{label} is empty: {source}")
    return data


def _prepare_output_dir(output_dir: str | Path) -> Path:
    destination = Path(output_dir).expanduser()
    if destination.exists() and not destination.is_dir():
        raise PackagingError(f"output directory is not a directory: {destination}")
    destination.mkdir(parents=True, exist_ok=True)
    return destination.resolve(strict=True)


def _safe_output_path(destination: Path, filename: str) -> Path:
    # Filenames are generated from validated version/target values, but keep
    # the guard explicit so future callers cannot escape the destination.
    if not filename or Path(filename).name != filename or filename in {".", ".."}:
        raise PackagingError(f"unsafe output filename: {filename!r}")
    path = destination / filename
    try:
        path.resolve().relative_to(destination)
    except ValueError as error:
        raise PackagingError(
            f"refusing output path outside {destination}: {path}"
        ) from error
    return path


def _write_atomic(destination: Path, filename: str, data: bytes) -> Path:
    path = _safe_output_path(destination, filename)
    descriptor, temporary = tempfile.mkstemp(prefix=".tmp-", dir=destination)
    try:
        with os.fdopen(descriptor, "wb") as handle:
            handle.write(data)
            handle.flush()
            os.fsync(handle.fileno())
        os.replace(temporary, path)
    except BaseException:
        try:
            os.unlink(temporary)
        except FileNotFoundError:
            pass
        raise
    return path


def _build_tar(executable: bytes, executable_name: str, license: bytes) -> bytes:
    raw = io.BytesIO()
    with tarfile.open(fileobj=raw, mode="w", format=tarfile.PAX_FORMAT) as archive:
        for name, data, mode in (
            (executable_name, executable, _EXECUTABLE_MODE),
            (LICENSE, license, _LICENSE_MODE),
        ):
            info = tarfile.TarInfo(name)
            info.size = len(data)
            info.mode = mode
            info.mtime = _TAR_MTIME
            info.uid = 0
            info.gid = 0
            info.uname = ""
            info.gname = ""
            archive.addfile(info, io.BytesIO(data))

    compressed = io.BytesIO()
    with gzip.GzipFile(
        fileobj=compressed, mode="wb", filename="", mtime=_TAR_MTIME
    ) as stream:
        stream.write(raw.getvalue())
    return compressed.getvalue()


def _zip_member(name: str, data: bytes, mode: int) -> tuple[zipfile.ZipInfo, bytes]:
    info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
    info.compress_type = zipfile.ZIP_DEFLATED
    info.create_system = 3  # Unix, so external_attr carries the file mode.
    info.external_attr = (0o100000 | mode) << 16
    return info, data


def _build_zip(executable: bytes, executable_name: str, license: bytes) -> bytes:
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, mode="w") as archive:
        for info, data in (
            _zip_member(executable_name, executable, _EXECUTABLE_MODE),
            _zip_member(LICENSE, license, _LICENSE_MODE),
        ):
            archive.writestr(info, data)
    return buffer.getvalue()


def package(
    *,
    binary: str | Path,
    license_path: str | Path,
    version: str,
    target: str,
    output_dir: str | Path,
    tag: str | None = None,
) -> PackageResult:
    """Create the release archive for one target."""
    validate_version(version)
    validate_target(target)
    verify_tag(version, tag)

    executable = _read_input(binary, "binary")
    license_data = _read_input(license_path, "licence")
    name = executable_name(target)

    destination = _prepare_output_dir(output_dir)
    filename = archive_name(version, target)
    # Resolve the output path before writing so a pre-existing symlink cannot
    # redirect the archive outside the requested destination.
    _safe_output_path(destination, filename)

    if target.endswith("-pc-windows-msvc"):
        archive_data = _build_zip(executable, name, license_data)
    else:
        archive_data = _build_tar(executable, name, license_data)

    archive_path = _write_atomic(destination, filename, archive_data)
    return PackageResult(
        archive=archive_path,
        executable=name,
        version=version,
        target=target,
    )


def _read_tar_members(path: Path) -> dict[str, tuple[bytes, int]]:
    members: dict[str, tuple[bytes, int]] = {}
    with tarfile.open(path, mode="r:gz") as archive:
        for member in archive.getmembers():
            if member.name in members:
                raise PackagingError(f"duplicate archive member: {member.name}")
            if not member.isfile():
                raise PackagingError(
                    f"archive member is not a regular file: {member.name}"
                )
            stream = archive.extractfile(member)
            if stream is None:
                raise PackagingError(f"cannot read archive member: {member.name}")
            members[member.name] = (stream.read(), member.mode & 0o777)
    return members


def _read_zip_members(path: Path) -> dict[str, tuple[bytes, int]]:
    members: dict[str, tuple[bytes, int]] = {}
    with zipfile.ZipFile(path) as archive:
        for info in archive.infolist():
            if info.filename in members:
                raise PackagingError(f"duplicate archive member: {info.filename}")
            if info.is_dir():
                raise PackagingError(f"archive member is a directory: {info.filename}")
            mode = (info.external_attr >> 16) & 0o777
            members[info.filename] = (archive.read(info), mode)
    return members


def verify_archive(version: str, target: str, archive_path: str | Path) -> None:
    """Verify one archive against the distribution contract."""
    validate_version(version)
    validate_target(target)
    path = Path(archive_path)
    expected_archive = archive_name(version, target)
    if path.name != expected_archive:
        raise PackagingError(
            f"unexpected archive name {path.name!r}; expected {expected_archive!r}"
        )
    if not path.is_file():
        raise PackagingError(f"missing archive: {path}")

    name = executable_name(target)
    if target.endswith("-pc-windows-msvc"):
        members = _read_zip_members(path)
    else:
        members = _read_tar_members(path)

    expected_members = {name, LICENSE}
    actual_members = set(members)
    if actual_members != expected_members:
        missing = sorted(expected_members - actual_members)
        extra = sorted(actual_members - expected_members)
        raise PackagingError(
            f"{path.name} member mismatch; missing={missing}, unexpected={extra}"
        )

    _, mode = members[name]
    if mode != _EXECUTABLE_MODE:
        raise PackagingError(f"{path.name}: {name} mode is {mode:o}, expected 755")


def verify_release(
    dist_dir: str | Path, version: str, tag: str | None = None
) -> list[Path]:
    """Verify the complete release archive set for a version."""
    validate_version(version)
    verify_tag(version, tag)

    dist = Path(dist_dir).expanduser()
    if not dist.is_dir():
        raise PackagingError(f"release asset directory does not exist: {dist}")

    expected = {archive_name(version, target) for target in SUPPORTED_TARGETS}
    actual: set[str] = set()
    for entry in dist.iterdir():
        if entry.is_symlink() or not entry.is_file():
            raise PackagingError(f"unexpected non-file release asset: {entry.name}")
        actual.add(entry.name)

    if actual != expected:
        missing = sorted(expected - actual)
        extra = sorted(actual - expected)
        raise PackagingError(
            f"release asset set mismatch in {dist}; missing={missing}, unexpected={extra}"
        )

    verified: list[Path] = []
    for target in SUPPORTED_TARGETS:
        archive_path = dist / archive_name(version, target)
        verify_archive(version, target, archive_path)
        verified.append(archive_path)
    return verified


def _argument_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)

    package_parser = commands.add_parser(
        "package", help="build one native release archive"
    )
    package_parser.add_argument("--binary", required=True)
    package_parser.add_argument("--license", default=LICENSE)
    package_parser.add_argument("--version", required=True)
    package_parser.add_argument("--target", required=True)
    package_parser.add_argument("--output-dir", required=True)
    package_parser.add_argument(
        "--tag",
        default=None,
        help="release tag to validate; empty means not publishing",
    )

    verify_parser = commands.add_parser(
        "verify-release", help="verify all five native archives"
    )
    verify_parser.add_argument("--dist", required=True)
    verify_parser.add_argument("--version", required=True)
    verify_parser.add_argument("--tag", default=None)

    tag_parser = commands.add_parser(
        "expected-tag", help="print the release tag for a package version"
    )
    tag_parser.add_argument("--version", required=True)
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = _argument_parser().parse_args(argv)
    try:
        if args.command == "package":
            result = package(
                binary=args.binary,
                license_path=args.license,
                version=args.version,
                target=args.target,
                output_dir=args.output_dir,
                tag=args.tag,
            )
            print(result.archive)
        elif args.command == "verify-release":
            verified = verify_release(args.dist, args.version, args.tag)
            print(
                f"verified {len(verified)} release archives for "
                f"{expected_tag(args.version)}"
            )
        elif args.command == "expected-tag":
            print(expected_tag(args.version))
        else:  # pragma: no cover - argparse enforces the command set
            raise PackagingError(f"unknown command: {args.command}")
    except PackagingError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())
