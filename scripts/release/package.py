#!/usr/bin/env python3
"""Build and reproducibly package the canonical Awaken release executable."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import shutil
import stat
import subprocess
import sys
import tarfile
import tempfile
import tomllib
import unittest
import zipfile
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parents[2]
PACKAGE_FILES = ("README.md", "LICENSE", "deploy/compose.yaml")


def workspace_version(repository: Path = REPOSITORY) -> str:
    with (repository / "Cargo.toml").open("rb") as manifest:
        return tomllib.load(manifest)["workspace"]["package"]["version"]


def validate_identity(version: str, tag: str | None, binary_version: str) -> None:
    if "-" in version:
        raise ValueError(f"workspace version is not a stable release: {version}")
    if tag is not None and tag != f"v{version}":
        raise ValueError(f"tag {tag!r} does not match workspace version v{version}")
    expected = f"awaken {version}"
    if binary_version.strip() != expected:
        raise ValueError(
            f"binary reports {binary_version.strip()!r}, expected {expected!r}"
        )


def host_target() -> str:
    output = subprocess.check_output(["rustc", "-vV"], text=True)
    for line in output.splitlines():
        if line.startswith("host: "):
            return line.removeprefix("host: ")
    raise RuntimeError("rustc -vV did not report a host target")


def source_epoch(repository: Path = REPOSITORY) -> int:
    configured = os.environ.get("SOURCE_DATE_EPOCH")
    if configured is not None:
        return int(configured)
    return int(
        subprocess.check_output(
            ["git", "log", "-1", "--format=%ct"], cwd=repository, text=True
        ).strip()
    )


def target_directory(repository: Path = REPOSITORY) -> Path:
    metadata = subprocess.check_output(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        cwd=repository,
        text=True,
    )
    return Path(json.loads(metadata)["target_directory"])


def add_tar_bytes(archive: tarfile.TarFile, name: str, data: bytes, mode: int, epoch: int) -> None:
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = mode
    info.mtime = epoch
    info.uid = 0
    info.gid = 0
    info.uname = ""
    info.gname = ""
    archive.addfile(info, io.BytesIO(data))


def zip_timestamp(epoch: int) -> tuple[int, int, int, int, int, int]:
    # ZIP timestamps cannot represent dates before 1980.
    import datetime

    moment = datetime.datetime.fromtimestamp(max(epoch, 315532800), datetime.UTC)
    return moment.year, moment.month, moment.day, moment.hour, moment.minute, moment.second


def create_archive(
    repository: Path,
    binary: Path,
    target: str,
    version: str,
    output_directory: Path,
    epoch: int,
) -> Path:
    base = f"awaken-v{version}-{target}"
    windows = target.endswith("windows-msvc") or target.endswith("windows-gnu")
    binary_name = "awaken.exe" if windows else "awaken"
    entries = [(binary_name, binary.read_bytes(), 0o755)]
    entries.extend(
        (name, (repository / name).read_bytes(), 0o644) for name in PACKAGE_FILES
    )
    entries.sort(key=lambda entry: entry[0])
    output_directory.mkdir(parents=True, exist_ok=True)

    if windows:
        path = output_directory / f"{base}.zip"
        with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            for name, data, mode in entries:
                info = zipfile.ZipInfo(f"{base}/{name}", zip_timestamp(epoch))
                info.create_system = 3
                info.external_attr = (stat.S_IFREG | mode) << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                archive.writestr(info, data)
    else:
        path = output_directory / f"{base}.tar.gz"
        with path.open("wb") as raw:
            with gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch, compresslevel=9) as compressed:
                with tarfile.open(fileobj=compressed, mode="w") as archive:
                    for name, data, mode in entries:
                        add_tar_bytes(archive, f"{base}/{name}", data, mode, epoch)
    return path


def write_checksum(archive: Path) -> Path:
    digest = hashlib.sha256(archive.read_bytes()).hexdigest()
    checksum = archive.with_name(f"{archive.name}.sha256")
    checksum.write_text(f"{digest}  {archive.name}\n", encoding="utf-8")
    return checksum


def build_release(target: str, repository: Path = REPOSITORY) -> Path:
    subprocess.run(
        [
            "cargo",
            "build",
            "--locked",
            "--release",
            "--package",
            "awaken-cli",
            "--bin",
            "awaken",
            "--target",
            target,
        ],
        cwd=repository,
        check=True,
    )
    suffix = ".exe" if target.endswith(("windows-msvc", "windows-gnu")) else ""
    return target_directory(repository) / target / "release" / f"awaken{suffix}"


class ReleaseContractTests(unittest.TestCase):
    # Cause/effect graph: C1 source version is stable, C2 tag is absent/matching,
    # C3 binary reports that exact version. E1 accepts only C1+C2+C3; every false
    # cause fails before an archive is published. R1 is the valid combination;
    # R2-R4 each negate one cause so all identity rejection effects are covered.
    def test_release_identity_decision_table(self) -> None:
        validate_identity("1.0.0", None, "awaken 1.0.0\n")  # R1 local
        validate_identity("1.0.0", "v1.0.0", "awaken 1.0.0")  # R1 tagged
        invalid = [
            ("1.0.0-dev", None, "awaken 1.0.0-dev"),  # R2: C1 false
            ("1.0.0", "v1.0.1", "awaken 1.0.0"),  # R3: C2 false
            ("1.0.0", None, "awaken 0.9.0"),  # R4: C3 false
        ]
        for rule in invalid:
            with self.subTest(rule=rule), self.assertRaises(ValueError):
                validate_identity(*rule)

    # Cause/effect graph: C1 target is Windows vs Unix, C2 payload is executable.
    # E1 selects ZIP+awaken.exe for Windows; E2 selects tar.gz+awaken for Unix;
    # E3 always includes README/LICENSE/Compose under one versioned root. R1/R2 cover the
    # two mutually exclusive platform rules and assert every package effect.
    def test_archive_platform_decision_table(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "README.md").write_text("readme", encoding="utf-8")
            (root / "LICENSE").write_text("license", encoding="utf-8")
            (root / "deploy").mkdir()
            (root / "deploy" / "compose.yaml").write_text("services: {}\n", encoding="utf-8")
            binary = root / "binary"
            binary.write_bytes(b"executable")
            output = root / "dist"

            unix = create_archive(root, binary, "x86_64-unknown-linux-gnu", "1.0.0", output, 0)
            self.assertTrue(unix.name.endswith(".tar.gz"))
            with tarfile.open(unix, "r:gz") as archive:
                names = archive.getnames()
                executable = archive.getmember(
                    "awaken-v1.0.0-x86_64-unknown-linux-gnu/awaken"
                )
                self.assertEqual(executable.mode, 0o755)
            self.assertEqual(
                names,
                [
                    "awaken-v1.0.0-x86_64-unknown-linux-gnu/LICENSE",
                    "awaken-v1.0.0-x86_64-unknown-linux-gnu/README.md",
                    "awaken-v1.0.0-x86_64-unknown-linux-gnu/awaken",
                    "awaken-v1.0.0-x86_64-unknown-linux-gnu/deploy/compose.yaml",
                ],
            )

            windows = create_archive(root, binary, "x86_64-pc-windows-msvc", "1.0.0", output, 0)
            self.assertEqual(windows.suffix, ".zip")
            with zipfile.ZipFile(windows) as archive:
                self.assertEqual(
                    archive.namelist(),
                    [
                        "awaken-v1.0.0-x86_64-pc-windows-msvc/LICENSE",
                        "awaken-v1.0.0-x86_64-pc-windows-msvc/README.md",
                        "awaken-v1.0.0-x86_64-pc-windows-msvc/awaken.exe",
                        "awaken-v1.0.0-x86_64-pc-windows-msvc/deploy/compose.yaml",
                    ],
                )

    # Coverage rationale: one known payload is sufficient because SHA-256 is a
    # direct function. The observable effects are the digest, two-space portable
    # checker format, archive basename (not host path), and trailing newline.
    def test_checksum_contract(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            archive = Path(temporary) / "asset.tar.gz"
            archive.write_bytes(b"awaken")
            checksum = write_checksum(archive)
            self.assertEqual(
                checksum.read_text(encoding="utf-8"),
                f"{hashlib.sha256(b'awaken').hexdigest()}  asset.tar.gz\n",
            )

    # Cause/effect graph: C1 Cargo config selects a target directory, C2 an
    # explicit CARGO_TARGET_DIR override is present. Cargo defines the constraint
    # that C2 supersedes C1. R1 (C1 only) and R2 (C1+C2) must resolve to Cargo's
    # actual output directory so packaging cannot search a parallel guessed path.
    def test_target_directory_uses_cargo_authority(self) -> None:
        from unittest.mock import patch

        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / ".cargo").mkdir()
            (root / "Cargo.toml").write_text("[workspace]\nmembers = []\n", encoding="utf-8")
            (root / ".cargo" / "config.toml").write_text(
                '[build]\ntarget-dir = "configured-target"\n', encoding="utf-8"
            )
            with patch.dict(os.environ, {}, clear=False):
                os.environ.pop("CARGO_TARGET_DIR", None)
                self.assertEqual(target_directory(root), root / "configured-target")
            override = root / "explicit-target"
            with patch.dict(os.environ, {"CARGO_TARGET_DIR": str(override)}):
                self.assertEqual(target_directory(root), override)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", help="Rust target triple; defaults to rustc host")
    parser.add_argument("--output-dir", type=Path, default=REPOSITORY / "dist")
    parser.add_argument("--self-test", action="store_true")
    args = parser.parse_args(argv)

    if args.self_test:
        suite = unittest.defaultTestLoader.loadTestsFromTestCase(ReleaseContractTests)
        return 0 if unittest.TextTestRunner(verbosity=2).run(suite).wasSuccessful() else 1

    version = workspace_version()
    target = args.target or host_target()
    binary = build_release(target)
    if not binary.is_file():
        raise FileNotFoundError(f"Cargo did not produce {binary}")
    reported_version = subprocess.check_output([str(binary), "--version"], text=True)
    tag = os.environ.get("GITHUB_REF_NAME") if os.environ.get("GITHUB_REF_TYPE") == "tag" else None
    validate_identity(version, tag, reported_version)
    archive = create_archive(REPOSITORY, binary, target, version, args.output_dir, source_epoch())
    checksum = write_checksum(archive)
    print(archive)
    print(checksum)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
