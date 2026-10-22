"""Recovery bundle admission and post-recovery value oracles."""
from __future__ import annotations

import json
from pathlib import Path, PurePosixPath
import re
import tarfile

from native_services import sha256

WAL_BYTES = 16 * 1024 * 1024


def _wal_position(name: str) -> tuple[int, int]:
    if not re.fullmatch(r'[0-9A-F]{24}', name):
        raise ValueError('invalid recovery WAL coordinate')
    timeline, log, segment = int(name[:8], 16), int(name[8:16], 16), int(name[16:], 16)
    if timeline == 0 or segment >= 256:
        raise ValueError('invalid 16 MiB recovery WAL coordinate')
    return timeline, log * 256 + segment


def select_wal_for_backup(backup: Path, archive: Path, last_segment: str) -> list[Path]:
    label = (backup / 'backup_label').read_text(encoding='utf-8')
    starts = re.findall(r'^START WAL LOCATION: ([0-9A-F]{1,8})/([0-9A-F]{1,8}) \(file ([0-9A-F]{24})\)$', label, re.M)
    timelines = re.findall(r'^START TIMELINE: ([1-9][0-9]*)$', label, re.M)
    if len(starts) != 1 or len(timelines) != 1:
        raise ValueError('backup label needs one exact WAL start and timeline')
    high, low, first = starts[0]
    timeline, begin = _wal_position(first)
    end_timeline, end = _wal_position(last_segment)
    if timeline != int(timelines[0]) or begin != ((int(high, 16) << 32) + int(low, 16)) // WAL_BYTES:
        raise ValueError('backup LSN, WAL name and timeline differ')
    if timeline != end_timeline or not begin <= end < begin + 64:
        raise ValueError('recovery WAL interval is outside the bounded backup')
    if archive.is_symlink() or not archive.is_dir():
        raise ValueError('recovery WAL archive must be an owned directory')
    result=[]
    for index in range(begin, end + 1):
        path=archive/f'{timeline:08X}{index // 256:08X}{index % 256:08X}'
        if path.is_symlink() or not path.is_file() or path.stat().st_size != WAL_BYTES:
            raise ValueError('recovery WAL interval is missing a complete segment')
        result.append(path)
    return result


def require(condition: bool, message: str) -> None:
    if not condition:
        raise RuntimeError(message)


def validate_manifest(value: object) -> dict:
    if not isinstance(value, dict) or set(value) != {'lsn', 'key', 'sha256'}:
        raise ValueError('recovery manifest must contain exactly its three coordinates')
    if not isinstance(value['lsn'], str) or not re.fullmatch(r'[0-9A-F]+/[0-9A-F]+', value['lsn']):
        raise ValueError('invalid recovery target LSN')
    if not isinstance(value['sha256'], str) or not re.fullmatch(r'[0-9a-f]{64}', value['sha256']):
        raise ValueError('invalid recovery content digest')
    if value['key'] != 'reliability-recovery/sha256/' + value['sha256']:
        raise ValueError('recovery object coordinate differs from its content digest')
    return dict(value)


def unpack_bundle(bundle: Path, expected_digest: str, destination: Path, expected_manifest: dict) -> None:
    validate_manifest(expected_manifest)
    require(sha256(bundle) == expected_digest, 'downloaded recovery bundle digest differs')
    if destination.exists() or destination.is_symlink():
        raise ValueError('recovery extraction requires a fresh owned directory')
    with tarfile.open(bundle, 'r:*') as archive:
        members = archive.getmembers()
        if not members or len(members) > 100000:
            raise ValueError('recovery bundle member budget exceeded')
        seen = set()
        total = 0
        for member in members:
            path = PurePosixPath(member.name)
            if (path.is_absolute() or '..' in path.parts or not path.parts
                    or '\\' in member.name or ':' in member.name
                    or path.parts[0] not in ('base', 'wal', 'backup-manifest.json')
                    or (path.parts[0] == 'backup-manifest.json' and len(path.parts) != 1)
                    or not (member.isfile() or member.isdir()) or str(path) in seen):
                raise ValueError('unsafe or duplicate recovery bundle member')
            seen.add(str(path));total += member.size
            if total > 2 * 1024**3:
                raise ValueError('recovery bundle extraction budget exceeded')
        destination.mkdir()
        archive.extractall(destination, members=members, filter='data')
    actual = validate_manifest(json.loads((destination / 'backup-manifest.json').read_text(encoding='utf-8')))
    require(actual == expected_manifest, 'restored manifest differs from the backup coordinate')
    for name in ('base', 'wal'):
        require((destination / name).is_dir() and not (destination / name).is_symlink(), 'missing owned recovery directory')


def verify_recovery_values(*, rows: str, primary_rows: str, row_coordinate: str,
                           manifest: dict, restored_content: Path, leaked_writes: str) -> None:
    validate_manifest(manifest)
    require(rows == 'at-target,base', 'restored database does not stop at the committed target')
    require(primary_rows == '3', 'primary lost one of the three recovery operations')
    require(row_coordinate == manifest['key'] + '|' + manifest['sha256'], 'restored row references another immutable object')
    require(sha256(restored_content) == manifest['sha256'], 'restored immutable content digest differs')
    require(leaked_writes == '0', 'restored-cluster write leaked into the primary')
