#!/usr/bin/env python3
"""One bounded static check; deadline.py retains process-tree ownership."""
from __future__ import annotations

import argparse
from collections import deque
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import time
import uuid

import deadline


def parser():
    parser = argparse.ArgumentParser(allow_abbrev=False)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--receipt", type=Path)
    mode.add_argument("--hook-label")
    parser.add_argument("--timeout", type=int, required=True)
    parser.add_argument("--shell")
    parser.add_argument("--read-only", action="store_true")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    return parser


def observe_hook(args, arguments):
    started, started_unix = time.monotonic(), time.time()
    from ci_performance import task_directory, require_active_task
    from formal_run_evidence import source_snapshot, write_record
    directory = task_directory()
    if directory is None:
        raise ValueError('hook observation requires its active pre-commit context')
    active = require_active_task(directory)
    root = Path.cwd()
    owned = directory / 'hooks' / str(uuid.uuid4())
    owned.mkdir(parents=True)
    script, log, receipt = (owned / name for name in ('command.sh', 'output.log', 'observation.json'))
    script.write_text(args.shell + '\n', encoding='utf-8', newline='\n')
    before = source_snapshot(root) if args.read_only else None
    record = {'schema_version': 1, 'kind': 'hook-check-observation', 'run_id': active['run_id'],
              'label': args.hook_label, 'read_only': args.read_only, 'status': 'running',
              'started_at_unix': started_unix, 'started_monotonic': started,
              'command_digest': hashlib.sha256(args.shell.encode()).hexdigest(),
              'arguments_digest': hashlib.sha256(json.dumps(arguments).encode()).hexdigest(),
              'duration_seconds': None, 'exit_code': None, 'source_before': before,
              'collector_setup_seconds': time.monotonic() - started,
              'duration_scope': 'worker observation entry through terminal write start; excludes native launcher startup'}
    write_record(receipt, record)  # Core evidence failure prevents execution.
    print(f'[start] {args.hook_label}', flush=True)
    status = 74
    try:
        environment = os.environ.copy()
        if 'AWAKEN_HOOK_PARENT_CONFIG_SET' in environment:
            if environment.pop('AWAKEN_HOOK_PARENT_CONFIG_SET') == 'x':
                environment['LEFTHOOK_CONFIG'] = environment.pop('AWAKEN_HOOK_PARENT_CONFIG')
            else:
                environment.pop('LEFTHOOK_CONFIG', None)
                environment.pop('AWAKEN_HOOK_PARENT_CONFIG', None)
        with log.open('w', encoding='utf-8') as output:
            executing = time.monotonic()
            status = deadline.run(args.timeout,
                [sys.executable, str(Path(__file__).with_name('_native_shell.py')), str(script), *arguments],
                stdout=output, stderr=subprocess.STDOUT, environment=environment)
            record['execution_seconds'] = time.monotonic() - executing
        if before is not None:
            verifying = time.monotonic()
            after = source_snapshot(root)
            record['source_verify_seconds'] = time.monotonic() - verifying
            record['source_unchanged'] = after == before
            if after != before:
                status = status or 1
                record['reason'] = 'read-only hook changed its source/index subject'
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        record['reason'] = type(error).__name__ + ': ' + str(error)
        status = status or 74
    finally:
        duration = time.monotonic() - started
        record.update(status='passed' if status == 0 else 'failed', exit_code=status,
                      duration_seconds=duration, finished_at_unix=time.time())
        try:
            write_record(receipt, record)
        except (OSError, ValueError, TypeError) as error:
            print(f'Cannot finalize hook observation: {error}', file=sys.stderr, flush=True)
            status = status or 74
        print(f'[{"pass" if status == 0 else "fail"}] {args.hook_label}: {duration:.2f}s; exit {status}; log {log}', flush=True)
        if status and log.is_file():
            with log.open(encoding='utf-8', errors='replace') as output:
                print(''.join(deque(output, maxlen=200)), end='', flush=True)
    return status


def main() -> int:
    argument_parser = parser()
    args = argument_parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if args.timeout <= 0 or (not args.hook_label and not command):
        argument_parser.error("positive --timeout and a command are required")
    if args.hook_label:
        if not args.shell or not args.hook_label.replace('-', '').replace('_', '').replace('.', '').isalnum():
            argument_parser.error('hook label and shell body are required')
        try:
            return observe_hook(args, command)
        except (OSError, ValueError, TypeError, subprocess.SubprocessError) as error:
            print(f'Cannot begin hook observation: {error}', file=sys.stderr)
            return 74
    if args.shell or args.read_only:
        argument_parser.error('shell/read-only options require hook mode')
    if args.receipt.exists():
        argument_parser.error("receipt must be fresh")
    started = time.monotonic()
    status = 1
    try:
        status = deadline.run(args.timeout, command)
    finally:
        with args.receipt.open("x", encoding="utf-8") as stream:
            json.dump({"status": status, "duration_seconds": round(time.monotonic() - started, 3)}, stream)
    return status


if __name__ == "__main__":
    raise SystemExit(main())
