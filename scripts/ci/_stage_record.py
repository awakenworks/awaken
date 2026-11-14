#!/usr/bin/env python3
"""Append stage rows to the enclosing build task, and find that task.

The unittest runner, the parallel hook transport and the native Lefthook
entry only locate the active build task and append rows to it. They used to
import ci_performance.py, whose report, workflow and self-test code imports
generate_hooks, crate_test and their own imports. ``generate_hooks.py
--script-tests`` follows imports, so every script test inherited that whole
closure and ran on nearly any scripts/ci edit. ci_performance.py re-exports
these functions; this module is their one definition.
"""
from __future__ import annotations

import json
import math
import os
from pathlib import Path
import re
import uuid

from _atomic_record import write_record


def task_directory(environment=None):
    environment = os.environ if environment is None else environment
    raw = environment.get('AWAKEN_BUILD_RUN_DIRECTORY')
    if not raw:
        return None
    path = Path(raw)
    if not path.is_absolute():
        raise ValueError('build run directory must be absolute')
    if not (path / 'evidence.json').is_file():
        # The host launcher can be the outermost owner (Cargo external command).
        # Its invocation is not a repository task bundle. Start a child task
        # with an explicit cross-run link rather than reusing a foreign schema.
        invocation = environment.get('AWAKEN_BUILD_INVOCATION_ID', '')
        if re.fullmatch(r'[A-Za-z0-9][A-Za-z0-9_.-]{0,127}', invocation):
            candidate = path / 'invocations' / invocation / 'observation.json'
            if candidate.is_file():
                value = json.loads(candidate.read_text())
                if (value.get('schema_version') == 1 and value.get('kind') == 'cargo-build-observation'
                        and value.get('run_id') == environment.get('AWAKEN_BUILD_RUN_ID')
                        and value.get('invocation_id') == invocation):
                    return None
        raise ValueError('build context has no matching task or host invocation')
    record = json.loads((path / 'evidence.json').read_text(encoding='utf-8'))
    if (record.get('kind') != 'build-observations' or record.get('observation_only') is not True
            or record.get('run_id') != environment.get('AWAKEN_BUILD_RUN_ID')):
        raise ValueError('build run context does not match its record')
    return path


def require_active_task(directory):
    record = json.loads((directory / 'evidence.json').read_text())
    if record.get('status') != 'running':
        raise ValueError('cannot append to or finish a completed build run')
    return record



def record_stage(label, elapsed, status, *, semantic_result=None):
    """One writer for CLI stages, checker rules and framework case timings."""
    if type(elapsed) not in (int, float) or not math.isfinite(elapsed) or elapsed < 0:
        raise ValueError('stage requires a nonnegative finite elapsed duration')
    directory = task_directory()
    if directory is None:
        return
    active = require_active_task(directory)
    row = {'label': label, 'duration_seconds': elapsed, 'exit_code': status}
    if semantic_result is not None:
        row['semantic_result'] = semantic_result
    write_record(directory / 'commands' / (str(uuid.uuid4()) + '.json'),
                 {'run_id': active['run_id'], 'rows': [row]})

