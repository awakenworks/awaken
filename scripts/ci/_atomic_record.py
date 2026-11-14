"""Write a canonical JSON record atomically: the one record writer.

Kept apart from formal_run_evidence.py so a writer that only appends rows
(_stage_record.py, which every unittest run reaches through
_strict_unittest.py) does not import the formal plan and source modules:
generate_hooks.py --script-tests follows imports, so that made every script
test run on any formal plan edit. formal_run_evidence.py re-exports both.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import tempfile


def canonical_bytes(value: object) -> bytes:
    return (json.dumps(value, sort_keys=True, separators=(",", ":"), ensure_ascii=True) + "\n").encode()


def write_record(path: Path, record: dict) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    # Readers see the previous complete record or the new one, never torn JSON.
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False) as temporary:
        temporary.write(canonical_bytes(record))
        temporary.flush()
        os.fsync(temporary.fileno())
        name = temporary.name
    try:
        os.replace(name, path)
    finally:
        if os.path.exists(name):
            os.unlink(name)
