"""Install pinned Windows service archives into a fresh task-owned directory."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import shutil
import stat
import urllib.request
import zipfile

from native_services import sha256, windows_executable

PINS = {
    "nats": {
        "archive": "nats-server-v2.15.0-windows-amd64.zip",
        "url": "https://github.com/nats-io/nats-server/releases/download/v2.15.0/nats-server-v2.15.0-windows-amd64.zip",
        "sha256": "2d0861dc2ca3567d17f7d19611d7b346cc2ccbd7086a0a77cc957a2c71663200",
        "bytes": 7423380,
        "executable": "nats-server-v2.15.0-windows-amd64/nats-server.exe",
    },
    "postgres": {
        "archive": "postgresql-16.15-5-windows-x64-binaries.zip",
        "url": "https://get.enterprisedb.com/postgresql/postgresql-16.15-5-windows-x64-binaries.zip",
        "sha256": "43bb45f173a6f08cf1d29a97a6d8deb119e8e8093a24c00d2d1001a0ccaa8281",
        "bytes": 373254386,
        "executable": "pgsql/bin/postgres.exe",
    },
    "minio": {
        "archive": "minio.exe",
        "url": "https://github.com/minio/minio/releases/download/RELEASE.2024-12-18T13-15-44Z/minio.windows-amd64.RELEASE.2024-12-18T13-15-44Z.exe",
        "sha256": "5dd4fdc24e3f583f158bdf51671efdd11b4cc7a33d2bf47aa178403d9e5c03a8",
        "bytes": 118784512,
        "executable": "minio.exe",
    },
    "mc": {
        "archive": "mc.exe",
        "url": "https://github.com/minio/mc/releases/download/RELEASE.2024-11-21T17-21-54Z/mc.windows-amd64.RELEASE.2024-11-21T17-21-54Z.exe",
        "sha256": "e77b5d926f18412c69ed994f2e064ef5d228df75a7b2dc754d4a5de6b009e9ca",
        "bytes": 28904448,
        "executable": "mc.exe",
    },
}

SERVICE_COMPONENTS = {"nats": ("nats",), "postgres": ("postgres",), "s3": ("minio", "mc")}


def extract_runtime(archive: Path, destination: Path, kind: str) -> int:
    if kind not in ("nats", "postgres"): raise ValueError("unsupported native service ZIP format")
    count, total = 0, 0
    with zipfile.ZipFile(archive) as bundle:
        for member in bundle.infolist():
            original = member.orig_filename
            path = PurePosixPath(original)
            if (path.is_absolute() or ".." in path.parts or "\\" in original
                    or ":" in original or stat.S_ISLNK(member.external_attr >> 16)):
                raise ValueError("unsafe native service archive member")
            selected = (member.filename == PINS["nats"]["executable"] if kind == "nats"
                        else path.parts[:2] in (("pgsql", "bin"), ("pgsql", "lib"), ("pgsql", "share")))
            if not selected or member.is_dir():
                continue
            target = destination.joinpath(*path.parts)
            if not target.resolve().is_relative_to(destination.resolve()):
                raise ValueError("native service extraction escaped its owned root")
            total += member.file_size
            if total > 2 * 1024**3:
                raise ValueError("native service archive exceeds runtime extraction budget")
            target.parent.mkdir(parents=True, exist_ok=True)
            with bundle.open(member) as source, target.open("xb") as output:
                shutil.copyfileobj(source, output)
            count += 1
    return count


def install(output: Path, archives: Path | None = None, service: str | None = None) -> dict:
    if os.name != "nt":
        raise RuntimeError("native service installation requires Windows")
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    record = {"platform": "Windows", "status": "running", "services": {}}
    receipt = output / "installation.json"
    def save():
        receipt.write_text(json.dumps(record, indent=2) + "\n", encoding="utf-8")
    save()
    try:
        components = tuple(PINS) if service is None else SERVICE_COMPONENTS[service]
        for kind in components:
            pin = PINS[kind]
            archive = output / pin["archive"]
            if archives is not None:
                with (archives / pin["archive"]).open("rb") as source, archive.open("xb") as target:
                    shutil.copyfileobj(source, target)
            else:
                with urllib.request.urlopen(pin["url"], timeout=60) as source, archive.open("xb") as target:
                    while block := source.read(1024 * 1024):
                        target.write(block)
                        if target.tell() > pin["bytes"]:
                            raise ValueError("native service download exceeds its pinned size")
            if archive.stat().st_size != pin["bytes"] or sha256(archive) != pin["sha256"]:
                raise ValueError("native service archive does not match its reviewed checksum")
            destination = output / kind
            destination.mkdir()
            if kind in ("minio", "mc"):
                shutil.copyfile(archive, destination / pin["executable"])
                count = 1
            else:
                count = extract_runtime(archive, destination, kind)
            executable = windows_executable(destination / pin["executable"])
            record["services"][kind] = {**pin, "extracted_files": count, "binary": executable}
            save()
        record["status"] = "passed"
    except Exception:
        record["status"] = "failed"
        raise
    finally:
        save()
    return record


if __name__ == "__main__":
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument("--service", choices=sorted(SERVICE_COMPONENTS), required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--archives", type=Path, help="optional directory of the same pinned archives")
    arguments = parser.parse_args()
    print(json.dumps(install(arguments.output, arguments.archives, arguments.service)))
