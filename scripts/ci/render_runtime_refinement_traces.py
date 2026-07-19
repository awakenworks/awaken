#!/usr/bin/env python3
"""Render async Rust ThreadCommit traces as TLC-checkable TLA+ modules."""

from __future__ import annotations

import argparse
import json
import re
import shutil
from pathlib import Path
from typing import Any


STATE_KEYS = {
    "run_state",
    "ticket_kind",
    "ticket_call",
    "call_state",
    "attempts",
    "batch_state",
    "link_state",
    "version",
}


def tla_string(value: str) -> str:
    # JSON and TLA+ share the escapes used by the stable identifiers emitted by
    # the runtime tests. Keep ensure_ascii enabled so generated modules are
    # portable across TLC launch environments.
    return json.dumps(value, ensure_ascii=True)


def tla_set(values: list[str]) -> str:
    return "{" + ", ".join(tla_string(value) for value in values) + "}"


def tla_function(calls: list[str], values: dict[str, Any], value) -> str:
    missing = set(calls) - set(values)
    extra = set(values) - set(calls)
    if missing or extra:
        raise ValueError(f"function domain drift: missing={missing}, extra={extra}")
    cases = " [] ".join(
        f"c = {tla_string(call)} -> {value(values[call])}" for call in calls
    )
    return f"[c \\in {tla_set(calls)} |-> CASE {cases}]"


def render_state(calls: list[str], state: dict[str, Any]) -> str:
    if set(state) != STATE_KEYS:
        raise ValueError(f"unexpected trace-state keys: {set(state) ^ STATE_KEYS}")
    fields = [
        f"runState |-> {tla_string(state['run_state'])}",
        f"ticketKind |-> {tla_string(state['ticket_kind'])}",
        f"ticketCall |-> {tla_string(state['ticket_call'])}",
        "callState |-> "
        + tla_function(calls, state["call_state"], lambda item: tla_string(str(item))),
        "attempts |-> "
        + tla_function(calls, state["attempts"], lambda item: str(int(item))),
        f"batchState |-> {tla_string(state['batch_state'])}",
        "linkState |-> "
        + tla_function(calls, state["link_state"], lambda item: tla_string(str(item))),
        f"version |-> {int(state['version'])}",
    ]
    return "[" + ",\n         ".join(fields) + "]"


def module_name(name: str) -> str:
    words = re.findall(r"[A-Za-z0-9]+", name)
    if not words:
        raise ValueError("trace name has no module-safe characters")
    return "RustTrace" + "".join(word[:1].upper() + word[1:] for word in words)


def render(document: dict[str, Any], output: Path) -> tuple[Path, Path]:
    name = str(document["name"])
    calls = [str(call) for call in document["calls"]]
    agent_calls = [str(call) for call in document["agent_calls"]]
    states = document["states"]
    if not calls or not states:
        raise ValueError("a refinement trace needs calls and states")
    if not set(agent_calls).issubset(calls):
        raise ValueError("agent calls must be a subset of calls")
    if [int(state["version"]) for state in states] != list(range(len(states))):
        raise ValueError("trace versions must be contiguous from zero")

    module = module_name(name)
    rendered_states = ",\n    ".join(render_state(calls, state) for state in states)
    module_terminator = "=" * 77
    tla = f"""-------------------------- MODULE {module} --------------------------
EXTENDS RustCommitSystem

Trace == <<
    {rendered_states}
>>

VARIABLE checker
TraceInit == checker = 0 /\\ state = InitialState
TraceNext == checker' = checker /\\ UNCHANGED state
TraceSpec == TraceInit /\\ [][TraceNext]_<<checker, state>>
TraceRefines == TraceIsRefinement(Trace)

{module_terminator}
"""
    cfg = f"""SPECIFICATION TraceSpec

CONSTANTS
    Calls = {tla_set(calls)}
    AgentCalls = {tla_set(agent_calls)}
    NoCall = {tla_string('no_call')}
    MaxAttempts = {int(document['max_attempts'])}
    MaxVersion = {len(states) - 1}

INVARIANT TraceRefines

CHECK_DEADLOCK FALSE
"""

    output.mkdir(parents=True, exist_ok=True)
    module_path = output / f"{module}.tla"
    config_path = output / f"{module}.cfg"
    module_path.write_text(tla, encoding="utf-8")
    config_path.write_text(cfg, encoding="utf-8")
    return module_path, config_path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("trace_dir", type=Path)
    parser.add_argument("output_dir", type=Path)
    args = parser.parse_args()

    traces = sorted(args.trace_dir.glob("*.json"))
    if not traces:
        raise SystemExit(f"no Rust refinement traces in {args.trace_dir}")
    args.output_dir.mkdir(parents=True, exist_ok=True)
    repository = Path(__file__).resolve().parents[2]
    shutil.copyfile(
        repository / "formal/tla/RustCommitSystem.tla",
        args.output_dir / "RustCommitSystem.tla",
    )
    for trace in traces:
        document = json.loads(trace.read_text(encoding="utf-8"))
        module, config = render(document, args.output_dir)
        print(f"{module}\t{config}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
