#!/usr/bin/env python3
"""Read-only evidence from the disposable fleet's Core NATS broker.

Wire contract: https://docs.nats.io/reference/system/monitor/varz
This checks broker traffic, not exactly-once effects or per-Run wake causality.
"""

import json
import sys
from pathlib import Path


def snapshot(raw: str) -> dict:
    value = json.loads(raw)
    if not isinstance(value, dict):
        raise ValueError("NATS observation must be an object")
    identity = value.get("server_id")
    if not isinstance(identity, str) or not identity.strip():
        raise ValueError("NATS observation has no server identity")
    for key in ("connections", "subscriptions", "in_msgs", "out_msgs"):
        # JSON booleans and numeric strings are not observations of counters.
        if type(value.get(key)) is not int or value[key] < 0:
            raise ValueError(f"invalid NATS counter: {key}")
    return value


def qualifies(current: dict, baseline: dict | None = None) -> bool:
    if current["connections"] < 3 or current["subscriptions"] == 0:
        return False
    if baseline is None:
        return True
    return (
        qualifies(baseline)
        and current["server_id"] == baseline["server_id"]
        and current["in_msgs"] > baseline["in_msgs"]
        and current["out_msgs"] > baseline["out_msgs"]
    )


def main(paths: list[str]) -> int:
    if len(paths) not in (1, 2):
        raise ValueError("usage: nats_wake_evidence.py current.json [baseline.json]")
    current = snapshot(Path(paths[0]).read_text())
    baseline = snapshot(Path(paths[1]).read_text()) if len(paths) == 2 else None
    if not qualifies(current, baseline):
        return 1
    print("NATS:", {key: current[key] for key in (
        "server_id", "connections", "subscriptions", "in_msgs", "out_msgs"
    )})
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except (OSError, ValueError) as error:
        print(f"NATS evidence unavailable: {error}", file=sys.stderr)
        sys.exit(1)
