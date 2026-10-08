#!/usr/bin/env python3
"""One bounded static check; deadline.py retains process-tree ownership."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import signal
import time

import deadline


def interrupted(number: int, _frame: object) -> None:
    raise SystemExit(128 + number)


def main() -> int:
    parser = argparse.ArgumentParser(allow_abbrev=False)
    parser.add_argument("--receipt", type=Path, required=True)
    parser.add_argument("--timeout", type=int, required=True)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if args.timeout <= 0 or not command:
        parser.error("positive --timeout and a command are required")
    if args.receipt.exists():
        parser.error("receipt must be fresh")
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
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
