"""Copy a WAL archive member only into the owned recovery cluster."""
from __future__ import annotations

import argparse
from pathlib import Path
import re
import shutil


def sql_literal(value: str) -> str:
    return "'" + value.replace('\\', '\\\\').replace("'", "''") + "'"


def copy_archived_wal(archive: Path, data: Path, name: str, target: Path) -> bool:
    if not re.fullmatch(r'[0-9A-F]{24}(?:\.[0-9A-F]{8}\.backup)?|[0-9A-F]{8}\.history', name):
        raise ValueError('invalid WAL member name')
    archive, data = archive.resolve(strict=True), data.resolve(strict=True)
    if not archive.is_dir() or not data.is_dir() or archive == data:
        raise ValueError('distinct owned WAL and cluster directories are required')
    destination = target.resolve()
    if destination.parent != (data / 'pg_wal').resolve(strict=True) or target.is_symlink():
        raise ValueError('WAL restore target escaped its cluster WAL directory')
    source = archive / name
    if source.is_symlink() or source.resolve().parent != archive:
        raise ValueError('WAL archive member escaped its archive')
    if not source.exists():
        return False
    if not source.is_file():
        raise ValueError('WAL archive member is not a regular file')
    shutil.copyfile(source, destination)
    return True


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False, description=__doc__)
    parser.add_argument('--archive', type=Path, required=True)
    parser.add_argument('--data', type=Path, required=True)
    parser.add_argument('name')
    parser.add_argument('target', type=Path)
    args = parser.parse_args(argv)
    try:
        return 0 if copy_archived_wal(args.archive, args.data, args.name, args.target) else 1
    except (ValueError, OSError):
        # PostgreSQL invokes restore_command while probing absent timeline files.
        # Error text must not copy connection or user-supplied path information.
        return 2


if __name__ == '__main__':
    raise SystemExit(main())
